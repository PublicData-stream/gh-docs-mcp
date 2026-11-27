#!/usr/bin/env python3
"""Run one approved documentation build over the VM's virtio serial port."""

import os
from pathlib import Path, PurePosixPath
import subprocess
import tarfile
import tempfile

from wire import BUNDLE_BYTES, JSON_BYTES, read_body, read_header, write_body, write_header


def unpack(bundle, root):
    seen = set()
    total = 0
    with tarfile.open(bundle, "r:") as archive:
        for index, member in enumerate(archive):
            path = PurePosixPath(member.name)
            total += member.size
            if (
                not member.isfile()
                or path.is_absolute()
                or path.as_posix() != member.name
                or not path.parts
                or path.parts[0] not in {"workspace", "registry"}
                or len(path.parts) < 2
                or ".." in path.parts
                or path in seen
                or index >= 20_000
                or total > BUNDLE_BYTES
            ):
                raise ValueError("invalid Rustdoc bundle member")
            seen.add(path)
            dest = root.joinpath(*path.parts)
            dest.parent.mkdir(parents=True, exist_ok=True)
            with archive.extractfile(member) as source, dest.open("xb") as target:
                while data := source.read(1024 * 1024):
                    target.write(data)
            dest.chmod(0o755 if member.mode & 0o111 else 0o644)


def run(port):
    with port.open("r+b", buffering=0) as serial, tempfile.TemporaryDirectory(
        prefix="docs-mcp-job-", dir="/tmp"
    ) as tmp:
        root = Path(tmp)
        bundle = root / "job.tar"
        result = root / "rustdoc.json"
        try:
            metadata = read_header(serial)
            read_body(serial, metadata["body_bytes"], BUNDLE_BYTES, bundle)
            unpack(bundle, root)
            bundle.unlink()
            subprocess.run(
                ["/usr/local/bin/docs-mcp-build", str(root), metadata["package"], metadata["target"], str(result)],
                check=True, timeout=1050, stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                env={
                    "PATH": "/usr/local/bin:/usr/bin:/bin",
                    "HOME": "/var/lib/docs-mcp-builder",
                    "TMPDIR": tmp,
                    "CARGO_NET_OFFLINE": "true",
                    "DOCS_MCP_RUSTWIDE_OFFLINE": "1",
                    "RUSTUP_TOOLCHAIN": "nightly-2026-11-25",
                    "DOCKER_HOST": os.environ["DOCKER_HOST"],
                },
            )
            size = result.stat().st_size
            if not 0 < size <= JSON_BYTES:
                raise ValueError("invalid Rustdoc output")
        except (OSError, ValueError, EOFError, subprocess.SubprocessError):
            write_header(serial, {"status": "failed"})
            return
        write_header(serial, {"status": "complete", "body_bytes": size})
        write_body(serial, result)


if __name__ == "__main__":
    os.umask(0o077)
    run(Path("/dev/virtio-ports/docs-mcp.rustdoc"))
