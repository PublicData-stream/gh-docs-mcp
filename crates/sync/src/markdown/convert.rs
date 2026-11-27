//! Convert Rustdoc JSON into a deterministic, bounded projection of public prose.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use docs_mcp_core::{
    Result,
    markdown::{
        DOCUMENTS, Document, FORMAT, Grants, JSON_BYTES, SNAPSHOT_BYTES, clean_path, digest, slug,
    },
};
use serde::Deserialize;
use serde_json::value::RawValue;

#[derive(Deserialize)]
struct Rustdoc {
    root: u32,
    includes_private: bool,
    index: HashMap<u32, Item>,
    target: Target,
    format_version: u32,
}

#[derive(Deserialize)]
struct Target {
    triple: String,
}

#[derive(Deserialize)]
struct Item {
    id: u32,
    crate_id: u32,
    name: Option<String>,
    docs: Option<String>,
    span: Option<Span>,
    visibility: Box<RawValue>,
    attrs: Vec<Box<RawValue>>,
    inner: BTreeMap<String, Box<RawValue>>,
}

#[derive(Deserialize)]
struct Span {
    filename: String,
    begin: (u64, u64),
}

#[derive(Default, Deserialize)]
struct Edges {
    #[serde(default)]
    items: Vec<u32>,
    #[serde(default)]
    fields: Vec<u32>,
    #[serde(default)]
    variants: Vec<u32>,
    #[serde(default)]
    impls: Vec<u32>,
    #[serde(default)]
    is_stripped: bool,
    #[serde(default)]
    is_synthetic: bool,
    r#trait: Option<Box<RawValue>>,
    r#for: Option<ImplTarget>,
    kind: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct ImplTarget {
    resolved_path: Option<ResolvedPath>,
}

#[derive(Deserialize)]
struct ResolvedPath {
    id: u32,
}

#[derive(Deserialize)]
struct Reexport {
    id: Option<u32>,
    name: String,
    is_glob: bool,
}

type Pending = (Vec<String>, u32, Option<String>, bool);

pub fn project(
    raw: &[u8],
    target: &str,
    grants: &Grants,
    map_span: impl Fn(&str) -> Option<String>,
) -> Result<Vec<Document>> {
    if raw.len() > JSON_BYTES {
        return Err("rustdoc JSON exceeds its byte limit".into());
    }
    let rustdoc: Rustdoc = serde_json::from_slice(raw).map_err(|_| "invalid rustdoc JSON")?;
    if rustdoc.format_version != FORMAT
        || rustdoc.includes_private
        || rustdoc.target.triple != target
    {
        return Err("unsupported rustdoc JSON format, visibility or target".into());
    }
    if rustdoc.index.len() > DOCUMENTS * 4 {
        return Err("rustdoc JSON exceeds its item limit".into());
    }
    let root = rustdoc
        .index
        .get(&rustdoc.root)
        .ok_or("missing rustdoc root")?;
    let krate = root
        .name
        .as_deref()
        .filter(|name| slug(name))
        .ok_or("invalid rustdoc crate name")?;
    if root.crate_id != 0 || !root.inner.contains_key("module") {
        return Err("invalid rustdoc root".into());
    }
    let mut pending = BTreeSet::from([(vec![], rustdoc.root, None, true)]);
    let mut visited = HashSet::new();
    let mut entries: BTreeMap<String, Vec<Document>> = BTreeMap::new();
    let mut total = 0;
    let mut count = 0;
    while let Some((parents, id, alias, visible_by_owner)) = pending.pop_first() {
        if parents.len() > 64 || pending.len() > DOCUMENTS * 4 {
            return Err("rustdoc graph exceeds its traversal limit".into());
        }
        let Some(item) = rustdoc.index.get(&id) else {
            continue;
        };
        if item.id != id
            || item.crate_id != root.crate_id
            || item.inner.len() != 1
            || item.attrs.iter().any(|attr| attr.get() == "\"doc_hidden\"")
            || !(visible_by_owner || item.visibility.get() == "\"public\"")
            || !visited.insert(id)
        {
            continue;
        }
        let (kind, inner) = item
            .inner
            .first_key_value()
            .ok_or("missing rustdoc item kind")?;
        if kind == "use" {
            let alias: Reexport = serde_json::from_str(inner.get())?;
            if let Some(id) = alias.id.filter(|_| !alias.is_glob) {
                pending.insert((parents, id, Some(alias.name), true));
            }
            continue;
        }
        let Some(name) = alias.as_deref().or(item.name.as_deref()) else {
            continue;
        };
        if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) || !slug(kind)
        {
            continue;
        }
        let mut children = parents.clone();
        if id != rustdoc.root {
            children.push(if kind == "module" {
                escape(name)
            } else {
                format!(
                    "{}.{}",
                    if kind == "function" { "fn" } else { kind },
                    escape(name)
                )
            });
        }
        enqueue(&rustdoc, &mut pending, id, kind, inner, &children)?;
        if let Some((docs, span)) = item
            .docs
            .as_ref()
            .filter(|docs| !docs.is_empty())
            .zip(item.span.as_ref())
            && clean_path(&span.filename)
            && span.filename.ends_with(".rs")
            && let Some(source_path) = map_span(&span.filename)
        {
            let path = format!(
                "rustdoc/{krate}/{}",
                if children.is_empty() {
                    "index.md".into()
                } else if kind == "module" {
                    format!("{}/index.md", children.join("/"))
                } else {
                    format!("{}.md", children.join("/"))
                }
            );
            let document = Document {
                path: path.clone(),
                source_path,
                item: if children.is_empty() {
                    krate.to_owned()
                } else {
                    format!(
                        "{krate}::{}",
                        children
                            .iter()
                            .map(|part| part
                                .split_once('.')
                                .map_or(part.as_str(), |(_, name)| name))
                            .collect::<Vec<_>>()
                            .join("::")
                    )
                },
                kind: if id == rustdoc.root {
                    "crate".into()
                } else {
                    kind.clone()
                },
                line: span.begin.0,
                markdown: docs.clone(),
            };
            if !document.valid() {
                return Err("invalid or oversized rustdoc document".into());
            }
            total += document.markdown.len()
                + document.path.len()
                + document.source_path.len()
                + document.item.len();
            count += 1;
            if total > SNAPSHOT_BYTES || count > DOCUMENTS {
                return Err("rustdoc Markdown exceeds its snapshot budget".into());
            }
            entries.entry(path).or_default().push(document);
        }
    }
    let mut result = Vec::new();
    for (path, docs) in entries {
        let collisions = docs.len() > 1;
        let mut allowed = Vec::new();
        for doc in docs {
            let unqualified = grants.allows(&doc.source_path, &path);
            let qualified = if collisions {
                let bytes = serde_json::to_vec(&doc)?;
                let qualified = format!(
                    "{}.{}.md",
                    path.trim_end_matches(".md"),
                    &digest(&bytes)[..24]
                );
                grants
                    .allows(&doc.source_path, &qualified)
                    .then_some(qualified)
            } else {
                None
            };
            if unqualified || qualified.is_some() {
                allowed.push((doc, unqualified, qualified));
            }
        }
        let ambiguous = allowed.len() > 1;
        for (mut doc, unqualified, qualified) in allowed {
            if ambiguous || !unqualified {
                let Some(path) = qualified else {
                    continue;
                };
                doc.path = path;
            }
            if doc.valid() {
                result.push(doc);
            }
        }
    }
    result.sort_by(|left, right| left.path.cmp(&right.path));
    result.dedup_by(|left, right| left.path == right.path);
    Ok(result)
}

fn enqueue(
    rustdoc: &Rustdoc,
    pending: &mut BTreeSet<Pending>,
    id: u32,
    kind: &str,
    inner: &RawValue,
    parents: &[String],
) -> Result<()> {
    if !matches!(
        kind,
        "module" | "trait" | "enum" | "struct" | "union" | "variant" | "primitive"
    ) {
        return Ok(());
    }
    let edges: Edges = serde_json::from_str(inner.get())?;
    let mut push = |id, visible| {
        pending.insert((parents.to_vec(), id, None, visible));
    };
    if !edges.is_stripped {
        for child in edges.items {
            push(child, kind == "trait");
        }
    }
    for child in edges.variants {
        push(child, true);
    }
    for child in edges.fields {
        push(child, kind == "variant");
    }
    // Struct and enum variant fields are nested in their kind descriptor.
    if let Some(fields) = edges.kind {
        for (mode, value) in serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(fields.get())
            .unwrap_or_default()
        {
            let fields = if mode == "tuple" {
                serde_json::from_str::<Vec<Option<u32>>>(value.get())?
                    .into_iter()
                    .flatten()
                    .collect()
            } else {
                serde_json::from_str::<Edges>(value.get())?.fields
            };
            for child in fields {
                push(child, kind == "variant");
            }
        }
    }
    for impl_id in edges.impls {
        let Some(implementation) = rustdoc
            .index
            .get(&impl_id)
            .filter(|item| item.crate_id == 0)
        else {
            continue;
        };
        let Some(inner) = implementation.inner.get("impl") else {
            continue;
        };
        let implementation: Edges = serde_json::from_str(inner.get())?;
        if implementation.r#trait.is_none()
            && !implementation.is_synthetic
            && implementation
                .r#for
                .and_then(|ty| ty.resolved_path)
                .is_some_and(|ty| ty.id == id)
        {
            for child in implementation.items {
                push(child, false);
            }
        }
    }
    Ok(())
}

fn escape(name: &str) -> String {
    use std::fmt::Write;
    let mut result = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') {
            result.push(char::from(byte));
        } else {
            write!(&mut result, "%{byte:02X}").expect("write to string");
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use docs_mcp_core::markdown::Approval;
    use serde_json::{Value, json};

    use super::project;

    #[test]
    fn projects_only_public_span_bound_docs_matching_one_grant() {
        let approval: Approval = serde_json::from_value(json!({
            "source_id":"35cf5ef7-85b9-4ee0-b006-65ad6a3bc4fa", "policy_version":1,
            "input":{"kind":"docsrs_rustdoc", "package":"fixture", "version":"1.0.0",
                "target":"x86_64-unknown-linux-gnu","format":61},
            "reviewer":"fixture", "notice":"fixture", "license_evidence":"fixture",
            "grants":[
                {"source_path_glob":"src/lib.rs","path_glob":"rustdoc/**/fn.open.md"},
                {"source_path_glob":"src/other.rs","path_glob":"rustdoc/**/fn.foreign.md"}
            ]
        }))
        .unwrap();
        let mut artifact = json!({
            "format_version":61, "includes_private":false, "root":0,
            "target":{"triple":"x86_64-unknown-linux-gnu"}, "index":{
                "0": item(0, 0, "fixture", "public", "src/lib.rs", json!({"module":{"items":[5,4,3,2,1]}})),
                "1": item(1, 0, "open", "public", "src/lib.rs", json!({"function":{}})),
                "2": item(2, 0, "private", "crate", "src/lib.rs", json!({"function":{}})),
                "3": item(3, 1, "foreign", "public", "src/other.rs", json!({"function":{}})),
                "4": item(4, 0, "foreign", "public", "src/lib.rs", json!({"function":{}})),
                "5": item(5, 0, "open", "public", "../src/lib.rs", json!({"function":{}}))
            }
        });
        let grants = approval.validate().unwrap();
        let project = |input: &Value| {
            project(
                &serde_json::to_vec(input).unwrap(),
                approval.input.package_target().1,
                &grants,
                |span| Some(span.to_owned()),
            )
        };
        let entries = project(&artifact).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "rustdoc/fixture/fn.open.md");
        assert_eq!(entries[0].item, "fixture::open");
        assert_eq!(
            entries[0].markdown,
            "First line\n\n```rust\nlet _ = 1;\n```"
        );
        artifact["includes_private"] = json!(true);
        assert!(project(&artifact).is_err());
        artifact["includes_private"] = json!(false);
        artifact["format_version"] = json!(60);
        assert!(project(&artifact).is_err());
    }

    fn item(id: u32, krate: u32, name: &str, visibility: &str, path: &str, inner: Value) -> Value {
        json!({
            "id":id,"crate_id":krate,"name":name,"visibility":visibility,
            "docs":"First line\n\n```rust\nlet _ = 1;\n```",
            "span":{"filename":path,"begin":[1,1]},"attrs":[], "inner":inner
        })
    }
}
