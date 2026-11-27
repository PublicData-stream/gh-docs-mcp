#!/usr/bin/env python3
"""Transfer authorized jobs between Sync and disposable networkless builder VMs."""

import argparse
import os
from pathlib import Path
import socket
import ssl
import subprocess
import tempfile
import time

from wire import BUNDLE_BYTES, JSON_BYTES, read_body, read_header, write_body, write_header


def rpc(config, tls, request, outgoing=None, incoming=None):
    with socket.create_connection((config.host, config.port), timeout=10) as tcp:
        with tls.wrap_socket(tcp, server_hostname=config.server_name) as sock:
            sock.settimeout(680 if request["operation"] == "lease" else 120)
            with sock.makefile("rwb") as stream:
                write_header(stream, request)
                if outgoing is not None:
                    write_body(stream, outgoing)
                header = read_header(stream)
                if incoming is not None and header["status"] == "job":
                    read_body(stream, header["body_bytes"], BUNDLE_BYTES, incoming)
                return header


def build(config, root, job, bundle, output):
    channel = root / "guest.sock"
    accelerator = "kvm" if os.access("/dev/kvm", os.R_OK | os.W_OK) else "tcg"
    with subprocess.Popen(
        [
            config.qemu, "-machine", f"q35,accel={accelerator}", "-smp", "2", "-m", "8192",
            "-display", "none", "-serial", "none", "-monitor", "none", "-nic", "none", "-no-reboot",
            "-sandbox", "on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny",
            "-drive", f"file={config.image},if=virtio,format=qcow2,snapshot=on",
            "-device", "virtio-serial-pci",
            "-chardev", f"socket,id=docs,server=on,wait=off,path={channel}",
            "-device", "virtserialport,chardev=docs,name=docs-mcp.rustdoc",
        ],
        env={"PATH": "/usr/bin:/bin", "TMPDIR": str(root)},
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    ) as guest:
        try:
            wait_until = time.monotonic() + 60
            with socket.socket(socket.AF_UNIX) as port:
                while True:
                    try:
                        port.connect(str(channel))
                        break
                    except (FileNotFoundError, ConnectionRefusedError):
                        if guest.poll() is not None or time.monotonic() >= wait_until:
                            raise TimeoutError("Rustdoc builder guest did not start") from None
                        time.sleep(0.25)
                port.settimeout(1100)
                with port.makefile("rwb") as stream:
                    write_header(stream, {
                        "body_bytes": bundle.stat().st_size, "package": job["package"], "target": job["target"],
                    })
                    write_body(stream, bundle)
                    result = read_header(stream)
                    if result["status"] != "complete":
                        return False
                    read_body(stream, result["body_bytes"], JSON_BYTES, output)
                    return True
        finally:
            if guest.poll() is None:
                guest.kill()
            guest.wait()


def run(config):
    tls = ssl.create_default_context(cafile=config.ca)
    tls.minimum_version = ssl.TLSVersion.TLSv1_2
    tls.load_cert_chain(config.cert, config.key)
    while True:
        with tempfile.TemporaryDirectory(prefix="docs-mcp-runner-", dir="/tmp") as directory:
            root = Path(directory)
            bundle = root / "job.tar"
            output = root / "rustdoc.json"
            try:
                job = rpc(config, tls, {"operation": "lease"}, incoming=bundle)
                if job["status"] == "job":
                    request = {"job_id": job["job_id"], "attempt": job["attempt"]}
                    try:
                        complete = build(config, root, job, bundle, output)
                    except (OSError, ValueError, EOFError, KeyError):
                        complete = False
                    if complete:
                        request.update(operation="complete", body_bytes=output.stat().st_size)
                        result = rpc(config, tls, request, outgoing=output)
                    else:
                        request.update(operation="fail")
                        result = rpc(config, tls, request)
                    print(f"Rustdoc job {job['job_id']} {result['status']}", flush=True)
                elif job["status"] != "idle":
                    print("Rustdoc build input unavailable", flush=True)
            except (OSError, ValueError, EOFError, KeyError):
                print("Rustdoc runner transport unavailable", flush=True)
        if config.once:
            return
        time.sleep(5)


if __name__ == "__main__":
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--server-name", required=True)
    parser.add_argument("--ca", type=Path, required=True)
    parser.add_argument("--cert", type=Path, required=True)
    parser.add_argument("--key", type=Path, required=True)
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--qemu", default="/usr/bin/qemu-system-x86_64")
    parser.add_argument("--once", action="store_true")
    args = parser.parse_args()
    args.image = args.image.resolve(strict=True)
    if not args.image.is_file() or "," in str(args.image):
        parser.error("expected a regular qcow2 builder image without a comma in its filename")
    run(args)
