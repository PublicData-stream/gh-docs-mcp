//! Fetch only crates.io tuples in the committed lockfile.

use std::{collections::BTreeMap, io::Write, time::Duration};

use docs_mcp_core::{
    Result,
    markdown::{digest, exact_version, slug, valid_digest},
};
use reqwest::{StatusCode, header};
use serde::Deserialize;
use tar::Builder;

use crate::upstream::fixed_client;

const MAX_DEPS: usize = 1024;
const MAX_PACKAGE: usize = 16 * 1024 * 1024;
const MAX_INDEX: usize = 8 * 1024 * 1024;
const MAX_REGISTRY: usize = 128 * 1024 * 1024;

#[derive(Deserialize)]
struct Lock {
    version: u8,
    package: Vec<Locked>,
}

#[derive(Deserialize)]
struct Locked {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
}

#[derive(Deserialize)]
struct Entry<'a> {
    name: &'a str,
    vers: &'a str,
    cksum: &'a str,
}

pub async fn prefetch<T: Write>(
    archive: &mut Builder<T>,
    lock: &[u8],
    fixture: Option<&crate::upstream::Fixture>,
) -> Result<()> {
    let locked = dependencies(lock)?;
    if locked.is_empty() {
        return Ok(());
    }
    let index_host = "index.crates.io";
    let crate_host = "static.crates.io";
    let (index, index_url) = fixed_client(index_host, "https://index.crates.io", fixture)
        .await
        .map_err(|_| "crates.io index unavailable")?;
    let (crates, crates_url) = fixed_client(crate_host, "https://static.crates.io", fixture)
        .await
        .map_err(|_| "crates.io artifacts unavailable")?;
    let mut size = 0;
    for (name, versions) in locked {
        let path = index_path(&name);
        let entries = fetch(&index, format!("{index_url}/{path}"), MAX_INDEX).await?;
        let index = select_records(&name, &versions, &entries)?;
        size += index.len();
        if size > MAX_REGISTRY {
            return Err("crates.io dependency bundle exceeds byte limit".into());
        }
        append(
            archive,
            &format!("registry/index/{path}"),
            index.as_bytes(),
            false,
        )?;
        for (version, checksum) in versions {
            let file = format!("{name}-{version}.crate");
            let body = fetch(
                &crates,
                format!("{crates_url}/crates/{name}/{file}"),
                MAX_PACKAGE,
            )
            .await?;
            size += body.len();
            if size > MAX_REGISTRY || digest(&body) != checksum {
                return Err("crates.io artifact checksum or byte budget failed".into());
            }
            append(archive, &format!("registry/{file}"), &body, false)?;
        }
    }
    Ok(())
}

fn dependencies(raw: &[u8]) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
    if raw.len() > 1024 * 1024 {
        return Err("Cargo.lock exceeds byte limit".into());
    }
    let lock: Lock = toml::from_str(std::str::from_utf8(raw)?).map_err(|_| "invalid Cargo.lock")?;
    if !matches!(lock.version, 3 | 4) || lock.package.len() > MAX_DEPS {
        return Err("unsupported Cargo.lock version or package count".into());
    }
    let mut entries = BTreeMap::<String, BTreeMap<String, String>>::new();
    for package in lock.package {
        if !slug(&package.name) || !exact_version(&package.version) {
            return Err("invalid Cargo.lock package coordinates".into());
        }
        if let Some(source) = package.source {
            if source != "registry+https://github.com/rust-lang/crates.io-index" {
                return Err("Cargo.lock includes an unsupported dependency source".into());
            }
            let checksum = package
                .checksum
                .filter(|checksum| valid_digest(checksum))
                .ok_or("missing or invalid registry checksum")?;
            if entries
                .entry(package.name)
                .or_default()
                .insert(package.version, checksum)
                .is_some()
            {
                return Err("duplicate Cargo.lock registry package".into());
            }
        } else if package.checksum.is_some() {
            return Err("unexpected checksum for local Cargo.lock package".into());
        }
    }
    Ok(entries)
}

fn select_records(name: &str, versions: &BTreeMap<String, String>, data: &[u8]) -> Result<String> {
    let mut records = BTreeMap::new();
    for line in std::str::from_utf8(data)?.lines() {
        let entry: Entry<'_> =
            serde_json::from_str(line).map_err(|_| "invalid crates.io index record")?;
        if let Some(checksum) = versions.get(entry.vers)
            && (entry.name != name
                || entry.cksum != checksum
                || records.insert(entry.vers, line).is_some())
        {
            return Err("crates.io index differs from Cargo.lock".into());
        }
    }
    if records.len() != versions.len() {
        return Err("locked package version is missing from crates.io".into());
    }
    let mut selected = String::new();
    for record in records.into_values() {
        selected.push_str(record);
        selected.push('\n');
    }
    Ok(selected)
}

fn index_path(name: &str) -> String {
    let name = name.to_ascii_lowercase();
    match name.len() {
        1 | 2 => format!("{}/{name}", name.len()),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    }
}

async fn fetch(client: &reqwest::Client, url: String, limit: usize) -> Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .timeout(Duration::from_secs(30))
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|_| "crates.io request failed")?;
    if response.status() != StatusCode::OK
        || response.headers().contains_key(header::CONTENT_ENCODING)
    {
        return Err("crates.io rejected exact dependency request".into());
    }
    let mut body = Vec::new();
    while let Some(bytes) = response
        .chunk()
        .await
        .map_err(|_| "crates.io transfer failed")?
    {
        if body.len() + bytes.len() > limit {
            return Err("crates.io response exceeds byte limit".into());
        }
        body.extend_from_slice(&bytes);
    }
    Ok(body)
}

pub fn append<T: Write>(
    archive: &mut Builder<T>,
    path: &str,
    bytes: &[u8],
    executable: bool,
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_mode(if executable { 0o755 } else { 0o644 });
    header.set_size(bytes.len() as u64);
    header.set_cksum();
    archive.append_data(&mut header, path, bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{dependencies, index_path, select_records};

    #[test]
    fn derives_only_checksum_bound_index_records() {
        let checksum = "a".repeat(64);
        let entries = dependencies(
            format!(
                r#"version = 4
[[package]]
name = "fixture"
version = "1.2.3"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "{checksum}"
"#
            )
            .as_bytes(),
        )
        .unwrap();
        let good = format!(r#"{{"name":"fixture","vers":"1.2.3","cksum":"{checksum}"}}"#);
        assert_eq!(
            select_records("fixture", &entries["fixture"], good.as_bytes()).unwrap(),
            good + "\n"
        );
        assert!(select_records("fixture", &entries["fixture"], b"").is_err());
        assert_eq!(index_path("Serde"), "se/rd/serde");
        assert_eq!(index_path("syn"), "3/s/syn");
    }
}
