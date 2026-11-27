//! Export and validate a single approved Git workspace under reader fences.

use std::{collections::BTreeSet, time::Duration};

use docs_mcp_core::{
    Result,
    git::{ObjectType, Oid},
    markdown::{Input, clean_path, digest},
};
use docs_mcp_git::reader::{Batch, TreeMode};
use docs_mcp_storage::publication::read_generation;
use tokio_postgres::Client;

use super::{
    convert,
    jobs::{self, Job, Outcome},
    registry,
};

const EXPORT_BYTES: usize = 96 * 1024 * 1024;
const EXPORT_FILES: usize = 16_384;
pub const BUNDLE_BYTES: usize = 256 * 1024 * 1024;

struct Workspace {
    archive: tar::Builder<Vec<u8>>,
    lock: Vec<u8>,
    package_path: String,
    origins: BTreeSet<String>,
}

pub struct Bundle {
    pub package_path: String,
    pub archive: Vec<u8>,
}

pub async fn bundle(
    client: &mut Client,
    job: &Job,
    fixture: Option<&crate::upstream::Fixture>,
) -> Result<Bundle> {
    let mut workspace = export(client, job, true).await?;
    tokio::time::timeout(
        Duration::from_secs(540),
        registry::prefetch(&mut workspace.archive, &workspace.lock, fixture),
    )
    .await??;
    let archive = workspace.archive.into_inner()?;
    if archive.len() > BUNDLE_BYTES {
        return Err("Rustdoc build input exceeds byte budget".into());
    }
    Ok(Bundle {
        package_path: workspace.package_path,
        archive,
    })
}

pub async fn complete(client: &mut Client, job: &Job, raw: Vec<u8>) -> Result<bool> {
    let workspace = export(client, job, false).await?;
    let target = job.approval.input.package_target().1.to_owned();
    let grants = job.approval.validate()?;
    let Input::GitRustdoc {
        workspace: root, ..
    } = &job.approval.input
    else {
        return Err("expected Git Rustdoc approval".into());
    };
    let root = root.clone();
    let (artifact, docs) = tokio::task::spawn_blocking(move || -> Result<_> {
        let artifact = digest(&raw);
        let docs = convert::project(&raw, &target, &grants, |span| {
            let local = join(&workspace.package_path, span);
            workspace
                .origins
                .contains(&local)
                .then(|| join(root.as_deref().unwrap_or(""), &local))
        })?;
        Ok((artifact, docs))
    })
    .await??;
    jobs::finish(
        client,
        job,
        Outcome::Publish {
            artifact: &artifact,
            docs: &docs,
            etag: None,
        },
    )
    .await
}

async fn export(client: &mut Client, job: &Job, include_bytes: bool) -> Result<Workspace> {
    let Input::GitRustdoc {
        workspace,
        package,
        reference,
        ..
    } = &job.approval.input
    else {
        return Err("expected Git Rustdoc approval".into());
    };
    let git = job.git.as_ref().ok_or("missing Git generation binding")?;
    let reference = reference.as_bytes().to_vec();
    let root = workspace.clone();
    let package = package.clone();
    let commit = git.commit;
    read_generation(
        client,
        git.repository,
        Some(git.generation),
        move |generation, reader| {
            if generation.format != commit.format()
                || !generation.refs.iter().any(|entry| {
                    entry.name.bytes() == reference && entry.peeled_commit == Some(commit)
                })
            {
                return Err("Git Rustdoc ref and generation differ".into());
            }
            reader.with_batch(|batch| -> Result<_> {
                let mut tree = batch.commit(commit)?.tree;
                if let Some(root) = root {
                    for part in root.split('/') {
                        tree = batch
                            .tree(tree)?
                            .into_iter()
                            .find(|entry| {
                                entry.mode == TreeMode::Directory && entry.name == part.as_bytes()
                            })
                            .ok_or("approved workspace directory unavailable")?
                            .oid;
                    }
                }
                let mut archive = tar::Builder::new(Vec::new());
                let mut manifests = Vec::new();
                let mut origins = BTreeSet::new();
                let mut files = 0;
                let mut size = 0;
                let mut lock = None;
                let mut stack = vec![(String::new(), tree, 0)];
                while let Some((path, oid, depth)) = stack.pop() {
                    for entry in batch.tree(oid)? {
                        let name = std::str::from_utf8(&entry.name)
                            .map_err(|_| "non-UTF-8 build input path")?;
                        let relative = join(&path, name);
                        if !clean_path(&relative) || depth >= 64 || unsafe_config(&relative) {
                            return Err(
                                "unsupported build input path or Cargo configuration".into()
                            );
                        }
                        match entry.mode {
                            TreeMode::Directory => stack.push((relative, entry.oid, depth + 1)),
                            TreeMode::Regular | TreeMode::Executable => {
                                files += 1;
                                if files > EXPORT_FILES {
                                    return Err("workspace exceeds file budget".into());
                                }
                                if relative.ends_with(".rs") {
                                    origins.insert(relative.clone());
                                }
                                let manifest = name == "Cargo.toml";
                                if !include_bytes && !manifest {
                                    continue;
                                }
                                let bytes = bounded_blob(batch, entry.oid, &mut size)?;
                                if manifest {
                                    let parsed: toml::Table = toml::from_str(std::str::from_utf8(
                                        &bytes,
                                    )?)
                                    .map_err(|_| "invalid Cargo manifest in approved workspace")?;
                                    if parsed
                                        .get("package")
                                        .and_then(|p| p.get("name"))
                                        .and_then(toml::Value::as_str)
                                        == Some(package.as_str())
                                    {
                                        manifests.push(path.clone());
                                    }
                                } else if relative == "Cargo.lock" {
                                    lock = Some(bytes.clone());
                                }
                                if include_bytes {
                                    registry::append(
                                        &mut archive,
                                        &format!("workspace/{relative}"),
                                        &bytes,
                                        entry.mode == TreeMode::Executable,
                                    )?;
                                }
                            }
                            TreeMode::Symlink | TreeMode::Gitlink => {
                                return Err(
                                    "build workspace contains a symlink or Git submodule".into()
                                );
                            }
                        }
                    }
                }
                let [package_path] = manifests.as_slice() else {
                    return Err(
                        "approved workspace must contain exactly one selected package".into(),
                    );
                };
                let lock = if include_bytes {
                    lock.ok_or("approved workspace requires a committed Cargo.lock")?
                } else {
                    Vec::new()
                };
                Ok(Workspace {
                    archive,
                    lock,
                    package_path: package_path.clone(),
                    origins,
                })
            })
        },
    )
    .await
}

fn bounded_blob(batch: &mut Batch<'_>, oid: Oid, size: &mut usize) -> Result<Vec<u8>> {
    let info = batch.info(oid)?.ok_or("workspace object unavailable")?;
    if info.kind != ObjectType::Blob || info.size > EXPORT_BYTES - *size {
        return Err("workspace exceeds byte budget".into());
    }
    *size += info.size;
    Ok(batch.contents(oid, ObjectType::Blob)?)
}

fn join(parent: &str, leaf: &str) -> String {
    if parent.is_empty() {
        leaf.to_owned()
    } else {
        format!("{parent}/{leaf}")
    }
}

fn unsafe_config(path: &str) -> bool {
    path.ends_with(".cargo/config") || path.ends_with(".cargo/config.toml")
}
