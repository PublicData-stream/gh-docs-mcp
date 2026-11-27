//! Installed in the pinned upstream docs_rs_build package by prepare-builder.sh.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail, ensure};
use docs_rs_rustwide::{BuildEnvironment, SandboxImageSource};
use rustwide::{Crate, Toolchain};

const NIGHTLY: &str = "nightly-2026-11-25";
const TARGET: &str = "x86_64-unknown-linux-gnu";
const RUSTWIDE: &str = "/opt/rustwide";
const MAX_OUTPUT: u64 = 128 * 1024 * 1024;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let rustwide = std::env::var_os("DOCS_MCP_RUSTWIDE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(RUSTWIDE));
    let image_path = std::env::var_os("DOCS_MCP_RUSTDOC_IMAGE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/docs-mcp-rustdoc-image"));
    let image = fs::read_to_string(image_path)?;
    let image = image.trim();
    let (_, digest) = image
        .rsplit_once("@sha256:")
        .context("expected a pinned build image")?;
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "expected a pinned build image"
    );
    let diagnostics = std::env::var_os("DOCS_MCP_BUILDER_DIAGNOSTICS").is_some();
    if diagnostics {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .try_init()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
    }
    docs_rs_rustwide::logging::init(diagnostics);
    let docker = Command::new("docker")
        .args([
            "info",
            "--format",
            "{{range .SecurityOptions}}{{println .}}{{end}}",
        ])
        .output()?;
    ensure!(
        docker.status.success()
            && std::str::from_utf8(&docker.stdout)?
                .lines()
                .any(|line| line == "name=rootless"),
        "Rustdoc builds require a rootless Docker daemon"
    );
    if args == ["--prepare"] {
        prepare(&rustwide, image)?;
        return Ok(());
    }
    let [job, package, target, output] = args.as_slice() else {
        bail!("usage: docs-mcp-build JOB_DIRECTORY PACKAGE TARGET OUTPUT")
    };
    ensure!(
        std::env::var("DOCS_MCP_RUSTWIDE_OFFLINE").as_deref() == Ok("1"),
        "Rustdoc builds require the preprovisioned offline Rustwide environment"
    );
    ensure!(target == TARGET, "unsupported local build target");
    let job = Path::new(job);
    let workspace = job.join("workspace");
    let cargo_home = rustwide.join("cargo-home");
    let registry = cargo_home.join("docs-mcp-registry");
    fs::create_dir_all(job.join("registry/index"))?;
    copy_registry(&job.join("registry"), &registry)?;
    // Cargo resolves source paths from Cargo home's parent. Rustwide mounts
    // that same Cargo home at /opt/rustwide/cargo-home in the sandbox.
    fs::write(
        cargo_home.join("config.toml"),
        "[net]\noffline = true\n[source.crates-io]\nreplace-with = \"docs-mcp\"\n[source.docs-mcp]\nlocal-registry = \"cargo-home/docs-mcp-registry\"\n",
    )?;
    let approved = read_lock(&workspace.join("Cargo.lock"))?;
    let packaged = package_source(&workspace, package, job, &rustwide)?;
    let packaged_lock = read_lock(&packaged.join("Cargo.lock"))?;
    ensure!(
        packaged_lock.iter().all(|tuple| approved.contains(tuple)),
        "packaging changed locked dependency identities"
    );
    configure_manifest(&packaged.join("Cargo.toml"), package)?;
    vendor(&packaged, &rustwide)?;
    // Directory sources are location independent. Cargo can consume these
    // unpacked dependencies from Rustwide's read-only Cargo home.
    fs::write(
        cargo_home.join("config.toml"),
        "[net]\noffline = true\n[source.crates-io]\nreplace-with = \"docs-mcp\"\n[source.docs-mcp]\ndirectory = \"cargo-home/docs-mcp-vendor\"\n",
    )?;
    fs::remove_dir_all(registry)?;

    let mut environment = prepare(&rustwide, image)?;
    let source = Crate::local(&packaged);
    let result = environment
        .release(&source)
        .run(|build| Ok(build.build_rustdoc_json(TARGET)?))?
        .into_inner()
        .into_inner();
    let input = File::open(result.path())?;
    let metadata = input.metadata()?;
    ensure!(
        metadata.is_file() && (1..=MAX_OUTPUT).contains(&metadata.len()),
        "unexpected rustdoc output size"
    );
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let written = io::copy(&mut input.take(MAX_OUTPUT + 1), &mut output)?;
    ensure!(
        written == metadata.len(),
        "rustdoc output changed during copy"
    );
    Ok(())
}

fn prepare(workspace: &Path, image: &str) -> Result<BuildEnvironment> {
    BuildEnvironment::builder(workspace)
        .toolchain(Toolchain::dist(NIGHTLY))
        .sandbox_image(SandboxImageSource::local(image))
        .fast_init(true)
        .include_default_targets(false)
        .build()
}

fn copy_registry(origin: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    for entry in fs::read_dir(origin)? {
        let entry = entry?;
        let path = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_registry(&entry.path(), &path)?;
        } else {
            ensure!(kind.is_file(), "unexpected registry file type");
            fs::copy(entry.path(), path)?;
        }
    }
    Ok(())
}

fn package_source(
    workspace: &Path,
    package: &str,
    scratch: &Path,
    rustwide: &Path,
) -> Result<PathBuf> {
    let target = scratch.join("package-target");
    let status = cargo(workspace, rustwide)
        .args([
            "package",
            "--allow-dirty",
            "--no-verify",
            "--offline",
            "--locked",
            "--package",
            package,
        ])
        .arg("--target-dir")
        .arg(&target)
        .status()?;
    ensure!(status.success(), "approved Cargo packaging failed");
    let archives = fs::read_dir(target.join("package"))?
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && entry.path().extension().is_some_and(|ext| ext == "crate")
        })
        .collect::<Vec<_>>();
    let [archive] = archives.as_slice() else {
        bail!("expected exactly one Cargo archive")
    };
    let extracted = docs_rs_crate_archive::unpack_crate_archive(File::open(archive.path())?)?;
    let packaged = scratch.join("packaged");
    fs::rename(extracted.path(), &packaged).context("move the validated Cargo package")?;
    Ok(packaged)
}

fn cargo(directory: &Path, rustwide: &Path) -> Command {
    let mut command = Command::new("cargo");
    command
        .env("CARGO_HOME", rustwide.join("cargo-home"))
        .env("RUSTUP_HOME", rustwide.join("rustup-home"))
        .env("RUSTUP_TOOLCHAIN", NIGHTLY)
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(directory);
    command
}

fn vendor(packaged: &Path, rustwide: &Path) -> Result<()> {
    let destination = rustwide.join("cargo-home/docs-mcp-vendor");
    fs::create_dir(&destination)?;
    let status = cargo(packaged, rustwide)
        .args([
            "vendor",
            "--offline",
            "--locked",
            "--respect-source-config",
            "--versioned-dirs",
        ])
        .arg(destination)
        .status()?;
    ensure!(
        status.success(),
        "approved dependencies could not be vendored offline"
    );
    Ok(())
}

type LockedPackage = (String, String, Option<String>, Option<String>);

fn read_lock(path: &Path) -> Result<Vec<LockedPackage>> {
    let lock: toml::Table = toml::from_str(&fs::read_to_string(path)?)?;
    lock.get("package")
        .and_then(toml::Value::as_array)
        .context("missing lockfile packages")?
        .iter()
        .map(|package| {
            let get = |key| {
                package
                    .get(key)
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned)
            };
            Ok((
                get("name").context("missing locked name")?,
                get("version").context("missing locked version")?,
                get("source"),
                get("checksum"),
            ))
        })
        .collect()
}

fn configure_manifest(path: &Path, expected: &str) -> Result<()> {
    let mut manifest: toml::Table = toml::from_str(&fs::read_to_string(path)?)?;
    let package = manifest
        .get_mut("package")
        .and_then(toml::Value::as_table_mut)
        .context("missing packaged crate metadata")?;
    ensure!(
        package.get("name").and_then(toml::Value::as_str) == Some(expected),
        "unexpected package"
    );
    let mut metadata = package
        .entry("metadata".to_owned())
        .or_insert(toml::Value::Table(toml::Table::new()));
    for key in ["docs", "rs"] {
        metadata = metadata
            .as_table_mut()
            .context("invalid docs.rs metadata")?
            .entry(key.to_owned())
            .or_insert(toml::Value::Table(toml::Table::new()));
    }
    let metadata = metadata
        .as_table_mut()
        .context("invalid docs.rs metadata")?;
    if let Some(value) = metadata.get("cargo-args") {
        ensure!(
            value
                .as_array()
                .is_some_and(|args| args.is_empty()
                    || (args.len() == 1 && args[0].as_str() == Some("--locked"))),
            "unsupported custom Cargo arguments"
        );
    }
    metadata.insert(
        "cargo-args".into(),
        toml::Value::Array(vec![toml::Value::String("--locked".into())]),
    );
    metadata.insert("default-target".into(), toml::Value::String(TARGET.into()));
    metadata.insert("targets".into(), toml::Value::Array(Vec::new()));
    metadata.remove("additional-targets");
    manifest.insert("workspace".into(), toml::Value::Table(toml::Table::new()));
    fs::write(path, toml::to_string(&manifest)?)?;
    Ok(())
}
