#!/usr/bin/env python3
"""Rebuild and qualify the exact Phase 0 rootful Compose stack."""

import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import ipaddress
import json
import os
from pathlib import Path
import platform
import secrets
from selectors import DefaultSelector, EVENT_READ
import shlex
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
from threading import Thread
import time
import traceback
import urllib.request

import firewall
import fixtures
import mcp_wire

ROOT = Path(__file__).resolve().parents[2]
DOCKER = ["docker", "--context", "default"]
SERVICES = ("serve", "sync", "feed", "postgres", "proxy", "sink", "migrate", "volume-admin")


def run(args, *, expected=0, timeout=30, env=None, **kwargs):
    result = subprocess.run(
        args, cwd=ROOT, env=env, text=True, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, timeout=timeout, **kwargs,
    )
    if expected is not None and result.returncode != expected:
        raise RuntimeError(f"{shlex.join(map(str, args))}: exit {result.returncode}\n{result.stdout}")
    return result


class Stack:
    def __init__(self, root, project, image_tag, *, mcp_fixture=False):
        self.root = root
        self.env = fixtures.prepare(root)
        self.compose_command = [
            *DOCKER, "compose", "-f", str(ROOT / "compose.yml"), "-p", project,
            "--profile", "feed", "--profile", "operator", "--profile", "fixtures",
        ]
        if mcp_fixture:
            override = root / "phase0-mcp.yml"
            override.write_text("services:\n  serve:\n    command: [--phase0-mcp-fixture]\n")
            self.compose_command.extend(["-f", str(override)])
        self.context = {**os.environ, **self.env, "GH_DOCS_IMAGE_TAG": image_tag}
        self.ids = {}

    def compose(self, *args, **options):
        return run([*self.compose_command, *args], env=self.context, **options)

    def exec(self, role, *args, **options):
        return self.compose("exec", "-T", role, *args, **options)

    def one_shot(self, role):
        self.compose("start", role)
        status = run([*DOCKER, "wait", self.ids[role]]).stdout.strip()
        if status != "0":
            logs = self.compose("logs", "--no-color", role).stdout
            raise RuntimeError(f"{role} exited {status}\n{logs}")

    def create(self):
        self.compose("config", "--quiet")
        images = self.compose("config", "--images").stdout.splitlines()
        run([*DOCKER, "image", "inspect", "--format", "{{.Id}}", *sorted(set(images))])
        # Serialize creation so a failure cannot race cleanup with sibling creates.
        self.compose("--parallel", "1", "create", "--no-build", "--pull", "never", *SERVICES, timeout=90)
        for role in SERVICES:
            self.ids[role] = self.compose("ps", "--all", "--quiet", role).stdout.strip()
            assert self.ids[role], role

    def start(self):
        self.one_shot("volume-admin")
        self.compose("up", "--no-build", "--no-recreate", "--pull", "never",
                     "--wait", "--wait-timeout", "130", "postgres", timeout=145)
        self.one_shot("migrate")
        self.one_shot("migrate")
        self.compose("start", "serve", "sync", "feed", "proxy", "sink")
        for role in ("serve", "sync", "feed"):
            deadline = time.monotonic() + 25
            while True:
                ready = self.exec(role, "wget", "-qO", "/dev/null", "http://127.0.0.1:8081/healthz", expected=None)
                if ready.returncode == 0:
                    break
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"{role} health failed: {ready.stdout}")
                time.sleep(0.3)


def db_tests(stack):
    binaries = {"serve": "gh-docs-serve", "sync": "gh-docs-sync"}
    for role, binary in binaries.items():
        identity = stack.exec(role, binary, "--db-identity").stdout.strip().split()
        assert identity[:4] == [f"gh_docs_{role}", "UTF8", "i", "und"], identity
        assert len(identity) == 8 and identity[4] == identity[5] and identity[6] == "tls=true", identity
        assert identity[7] == f"default_read_only={str(role == 'serve').lower()}", identity
        print(f"A02 {' '.join(identity)}", flush=True)
        stack.exec(role, binary, "--db-select")
        for option in ("--db-ddl", "--db-temp", "--db-private"):
            denied = stack.exec(role, binary, option, expected=None)
            assert denied.returncode != 0 and "SQLSTATE 42501" in denied.stdout, denied.stdout
    stack.exec("sync", "gh-docs-sync", "--db-dml")
    denied = stack.exec("serve", "gh-docs-serve", "--db-dml", expected=None)
    assert denied.returncode != 0 and "SQLSTATE 42501" in denied.stdout, denied.stdout
    stack.exec("serve", "gh-docs-serve", "--db-lock")
    for setting, value, reason in (
        ("DB_CA_FILE", "/run/secrets/wrong-ca.crt", "UnknownIssuer"),
        ("DB_HOST", "wrong-db.phase0.test", "NotValidForName"),
    ):
        result = stack.compose("exec", "-T", "-e", f"{setting}={value}", "serve", "gh-docs-serve",
                               "--db-select", expected=None)
        assert result.returncode != 0 and reason in result.stdout, result.stdout

    # Send only a plaintext startup packet over the already allowed DB route.
    payload = b"user\0gh_docs_serve\0database\0gh_docs\0\0"
    packet = struct.pack("!II", 8 + len(payload), 196608) + payload
    escaped = "".join(f"\\{value:03o}" for value in packet)
    result = stack.exec("serve", "sh", "-c",
                        'printf "%b" "$1" | nc -w 3 "$2" 5432 | grep -ao "no encryption"',
                        "--", escaped, stack.env["PG_READ_IP"])
    assert result.stdout.strip() == "no encryption", result.stdout
    database = stack.exec("postgres", "psql", "-XAt", "-d", "gh_docs", "-U", "postgres", "-c", """
        SELECT count(*) FROM pg_authid
          WHERE rolname IN ('gh_docs_serve', 'gh_docs_sync', 'gh_docs_migrate')
            AND rolpassword LIKE 'SCRAM-SHA-256$%';
        SELECT pg_get_userbyid(nspowner) FROM pg_namespace WHERE nspname='foundation';
        SELECT has_table_privilege('gh_docs_serve', 'foundation.probe', 'INSERT,UPDATE,DELETE');
        SELECT has_schema_privilege('gh_docs_sync', 'foundation', 'CREATE');
    """).stdout.splitlines()
    assert database == ["3", "gh_docs_migrate", "f", "f"], database
    print("A02 TLS and SCRAM, scoped DML, read-only locks and migration separation passed", flush=True)


def stack_configuration(stack):
    expected_networks = {
        "serve": ("ingress", "db_read", "sync_notify"),
        "sync": ("db_write", "feed_control", "sync_notify", "sync_egress"),
        "feed": ("feed_control", "feed_egress"),
        "postgres": ("db_read", "db_write"),
    }
    expected_mounts = {
        "serve": {"/run/secrets": False, "/git-store": False},
        "sync": {"/run/secrets": False, "/git-store": True, "/git-sync-private": True},
        "feed": {"/run/secrets": False},
    }
    expected_secrets = {
        "serve": {"db-ca.crt", "db-password", "wrong-ca.crt", "cursor.key", "sync-serve.key"},
        "sync": {"db-ca.crt", "db-password", "feed-sync.key", "sync-serve.key"},
        "feed": {"feed-sync.key"},
    }
    for role, uid in [("serve", 10001), ("sync", 10002), ("feed", 10003), ("postgres", 70), ("proxy", 101)]:
        info = json.loads(run([*DOCKER, "inspect", stack.ids[role]]).stdout)[0]
        config = info["HostConfig"]
        assert config["ReadonlyRootfs"] and not config["Privileged"] and config["CapDrop"] == ["ALL"], role
        assert not config["CapAdd"] and "no-new-privileges:true" in config["SecurityOpt"], role
        assert config["PidsLimit"] and config["Memory"] and config["NanoCpus"], role
        assert config["Dns"] == ["127.0.0.1"], role
        assert bool(config["PortBindings"]) == (role == "proxy"), role
        if role == "proxy":
            assert config["PortBindings"]["8443/tcp"] == [{"HostIp": "127.0.0.1", "HostPort": stack.env["PROXY_PORT"]}]
        if role in expected_networks:
            networks = info["NetworkSettings"]["Networks"]
            project = info["Config"]["Labels"]["com.docker.compose.project"]
            assert set(networks) == {f"{project}_{name}" for name in expected_networks[role]}, networks.keys()
        if role in expected_mounts:
            mounts = {m["Destination"]: m["RW"] for m in info["Mounts"]}
            assert mounts == expected_mounts[role], mounts
            observed_secrets = set(stack.exec(role, "ls", "/run/secrets").stdout.splitlines())
            assert observed_secrets == expected_secrets[role], observed_secrets
            for name in observed_secrets:
                if name.endswith(".key") or name == "db-password":
                    mode_owner = stack.exec(role, "stat", "-c", "%a %u", f"/run/secrets/{name}").stdout.strip()
                    assert mode_owner == f"400 {uid}", (role, name, mode_owner)
        process = stack.exec(role, "sh", "-c", "id -u; grep -E '^(CapEff|NoNewPrivs|Seccomp):' /proc/1/status").stdout
        assert process.splitlines()[0] == str(uid), process
        assert dict(line.split(":") for line in process.splitlines()[1:]) == {
            "CapEff": "\t0000000000000000", "NoNewPrivs": "\t1", "Seccomp": "\t2",
        }, process
        print(f"{role} image {info['Image']}", flush=True)
    admin = json.loads(run([*DOCKER, "inspect", stack.ids["volume-admin"]]).stdout)[0]["HostConfig"]
    assert admin["NetworkMode"] == "none" and {name.removeprefix("CAP_") for name in admin["CapAdd"]} == {"CHOWN", "FOWNER"}, admin["CapAdd"]

    # Positive and negative mount operations use the service's actual uid.
    for role in ("serve", "sync", "feed"):
        stack.exec(role, "sh", "-ec", "test -w /tmp && ! touch /root-probe")
    stack.exec("serve", "sh", "-ec", "test -r /git-store/fixture.txt && ! touch /git-store/write-probe && test ! -e /git-sync-private")
    stack.exec("sync", "sh", "-ec", "touch /git-store/write-probe /git-sync-private/write-probe")
    stack.exec("feed", "sh", "-ec", "test ! -e /git-store && test ! -e /git-sync-private")
    print("A03 effective users, mounts, caps, seccomp, egress resolver and secret separation passed", flush=True)


class HostSink(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(204)
        self.end_headers()

    def log_message(self, *_):
        pass


@contextmanager
def host_sink(ip):
    with ThreadingHTTPServer((ip, 0), HostSink) as server:
        thread = Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield f"http://{ip}:{server.server_port}/"
        finally:
            server.shutdown()
            thread.join()


def network_tests(stack, firewall_name):
    def request(role, host, port, *, allowed):
        host = f"[{host}]" if ":" in host else host
        result = stack.exec(role, "wget", "-qO-", "-T", "2", f"http://{host}:{port}/", expected=None)
        if allowed:
            assert result.returncode == 0 and result.stdout == "phase0-sink", (role, host, result.stdout)
        else:
            assert result.returncode != 0, (role, host, port, result.stdout)

    for role in ("sync", "feed"):
        for family in ("IP", "V6"):
            host = stack.env[f"SINK_{role.upper()}_{family}"]
            request(role, host, 8080, allowed=True)
            request(role, host, 8081, allowed=False)
    for family in ("IP", "V6"):
        host = stack.env[f"SINK_INGRESS_{family}"]
        request("serve", host, 8080, allowed=False)
        request("serve", host, 8081, allowed=False)
    request("serve", stack.env["SINK_SYNC_IP"], 8080, allowed=False)
    request("sync", stack.env["SERVE_NOTIFY_IP"], 8080, allowed=False)

    try:
        with socket.create_connection((stack.env["SERVE_INGRESS_IP"], 8080), timeout=2):
            raise AssertionError("host bypassed the TLS proxy")
    except OSError:
        pass
    gateway = str(ipaddress.ip_network(stack.env["INGRESS_CIDR"])[1])
    with host_sink(gateway) as url:
        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(url, timeout=3) as response:
            assert response.status == 204
        result = stack.exec("serve", "wget", "-qO-", "-T", "2", url, expected=None)
        assert result.returncode != 0, "serve reached host-only sink"
    for role in ("serve", "feed"):
        denied = stack.exec(role, "timeout", "3", "nslookup", "phase0.invalid", gateway, expected=None)
        assert denied.returncode != 0, (role, denied.stdout)
    for family in ("inet", "bridge"):
        rules = run(["nft", "list", "table", family, firewall_name]).stdout
        assert any("counter packets 0 bytes" not in line for line in rules.splitlines() if line.endswith(" drop")), rules
    print("A03 allowlisted v4/v6 egress, denied adjacent/routed egress and host probes passed", flush=True)


def storage_tests(stack):
    # Hold the read-only bind-mount lock in one container while attempting
    # the conflicting operation in another container on the same inode.
    holder = [*stack.compose_command, "exec", "-T", "serve", "sh", "-ec",
              "exec 9</git-store/fence.lock; flock -n -s 9; echo locked; read -r release"]
    with subprocess.Popen(holder, cwd=ROOT, env=stack.context, text=True,
                          stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT) as process:
        try:
            with DefaultSelector() as selector:
                selector.register(process.stdout, EVENT_READ)
                assert selector.select(8), "read-only lock holder did not respond"
            assert process.stdout.readline().strip() == "locked", "read-only lock holder failed"
            denied = stack.exec("sync", "flock", "-n", "-x", "/git-store/fence.lock", "true", expected=None)
            assert denied.returncode == 1 and not denied.stdout, denied.stdout
        finally:
            process.communicate("release\n", timeout=8)
        assert process.returncode == 0, process.returncode
    stack.exec("sync", "flock", "-n", "-x", "/git-store/fence.lock", "true")
    path = Path(stack.root, "store/write-probe")
    for target in (path, path.parent):
        fd = os.open(target, os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    print("A03 filesystem shared/exclusive flock and file and directory fsync passed", flush=True)


def worker_tests(stack):
    versions = [
        stack.exec("serve", "git", "--version").stdout.strip(),
        stack.exec("sync", "git", "--version").stdout.strip(),
        stack.exec("serve", "rg", "--version").stdout.strip().replace("\n", " | "),
    ]
    assert versions[:2] == ["git version 2.54.1"] * 2, versions
    assert "ripgrep 16.0.0" in versions[2] and "-pcre2" in versions[2], versions
    git_options = stack.exec("serve", "git", "version", "--build-options").stdout
    assert "SHA-1: SHA1_DC" in git_options and "SHA-256: SHA256_BLK" in git_options, git_options
    print("A04 " + " / ".join(versions) + " / SHA1_DC and SHA256_BLK", flush=True)
    for profile in ("git", "search"):
        result = stack.exec("serve", "gh-docs-serve", "--phase0-worker", profile)
        assert "inherited descriptors and denial controls passed" in result.stdout, result.stdout
        assert "phase-zero fixture" in result.stdout, result.stdout
        print(f"A04 {profile}: {result.stdout.splitlines()[0]}", flush=True)
        failed = stack.exec("serve", "gh-docs-serve", "--phase0-worker", profile, "--fail-setup", expected=None)
        assert failed.returncode != 0 and "sandbox_unavailable" in failed.stdout, failed.stdout
        assert "phase-zero fixture" not in failed.stdout, failed.stdout
    git = stack.exec("serve", "env", "-i", "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL=/dev/null",
                     "GIT_NO_REPLACE_OBJECTS=1", "/usr/local/bin/git",
                     "-c", "safe.directory=/git-store/repositories/sha256.git",
                     "--git-dir=/git-store/repositories/sha256.git", "cat-file", "blob", "HEAD:fixture.txt")
    assert git.stdout.encode() == fixtures.VALUE
    print("A04 SHA-256 bare fixture read passed (toolchain probe) and both profiles failed closed", flush=True)


def mcp_tests(stack, node, client_dir):
    port = int(stack.env["PROXY_PORT"])
    ca = Path(stack.root, "ca.crt")
    mcp_wire.probe(port, ca)
    identity = {"user": int(os.environ["SUDO_UID"]), "group": int(os.environ["SUDO_GID"]), "extra_groups": []}
    with ca.open("rb") as cert:
        env = {**os.environ, "NODE_EXTRA_CA_CERTS": f"/proc/self/fd/{cert.fileno()}"}
        result = run([node, client_dir / "probe.mjs", f"https://127.0.0.1:{port}/mcp"],
                     env=env, timeout=15, pass_fds=(cert.fileno(),), **identity)
    print("A01 " + result.stdout.strip(), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--client-dir", type=Path, required=True, help="prepared TypeScript fixture under /tmp")
    parser.add_argument("--node", type=Path, required=True, help="absolute path to the pinned Node runtime")
    parser.add_argument("--skip-build", action="store_true", help="reuse already rebuilt, pinned local images")
    parser.add_argument("--image-tag", default="phase0", help="local image tag when using --skip-build")
    args = parser.parse_args()
    if args.image_tag != "phase0" and not args.skip_build:
        parser.error("--image-tag requires --skip-build")
    if not __debug__:
        parser.error("assertions must be enabled for qualification")
    if os.geteuid() != 0 or int(os.environ.get("SUDO_UID", "0")) == 0:
        parser.error("run with sudo for scoped nftables rules and fixture ownership")
    client_dir = args.client_dir.resolve(strict=True)
    if not client_dir.is_relative_to("/tmp") or not args.node.is_absolute():
        parser.error("client-dir must be under /tmp and node must be an absolute path")
    if not (client_dir / "node_modules/@modelcontextprotocol/client").is_dir():
        parser.error("prepare the pinned client with the fixture lockfile first")
    if not (client_dir / "probe.mjs").is_file():
        parser.error("copy the locked fixture client and its probe into client-dir")
    print(f"{datetime.now(timezone.utc).isoformat()} {platform.platform()} project qualification", flush=True)
    print(run(["git", "-c", f"safe.directory={ROOT}", "rev-parse", "HEAD"]).stdout.strip(), flush=True)
    print(run([*DOCKER, "version", "--format", "{{.Server.Version}} {{.Server.Arch}}"]).stdout.strip(), flush=True)
    print(run([*DOCKER, "compose", "version"]).stdout.strip(), flush=True)
    node_version = run([args.node, "--version"]).stdout.strip()
    assert node_version == "v26.13.1", node_version
    project = "ghdocs0-" + secrets.token_hex(5)
    image_tag = args.image_tag if args.skip_build else project
    table = project.replace("-", "_")
    root = Path(tempfile.mkdtemp(prefix=f"{project}-", dir="/tmp"))
    stack, installed, built_images = None, False, []
    try:
        stack = Stack(root, project, image_tag, mcp_fixture=True)
        if not args.skip_build:
            for role in ("serve", "sync", "feed"):
                image = f"gh-docs-mcp-{role}:{image_tag}"
                build = [
                    *DOCKER, "buildx", "build", "--network", "host", "--platform", "linux/amd64",
                    "--load", "--target", role, "-t", image, ".",
                ]
                print(shlex.join(build), flush=True)
                subprocess.run(build, cwd=ROOT, check=True, timeout=900)
                built_images.append(image)
        stack.create()
        firewall.install(table, stack.env, stack.root)
        installed = True
        stack.start()
        failures = []
        checks = [
            ("A01", mcp_tests, (stack, args.node, client_dir)),
            ("A02", db_tests, (stack,)),
            ("A03 config", stack_configuration, (stack,)),
            ("A03 network", network_tests, (stack, table)),
            ("A03 filesystem", storage_tests, (stack,)),
            ("A04", worker_tests, (stack,)),
        ]
        for gate, check, parameters in checks:
            try:
                check(*parameters)
            except Exception:
                failures.append(gate)
                print(f"{gate} failed", flush=True)
                traceback.print_exc()
        if failures:
            raise RuntimeError(f"failed gates: {', '.join(failures)}")
    except BaseException:
        if stack is not None:
            logs = stack.compose("logs", "--no-color", "--tail", "30", expected=None, timeout=12)
            print(logs.stdout, flush=True)
        raise
    finally:
        if stack is not None:
            try:
                stack.compose("down", "--volumes", "--remove-orphans", timeout=60)
            except (RuntimeError, subprocess.TimeoutExpired):
                print(f"Cleanup failed for {project}; retained firewall table {table} and fixtures at {root}", flush=True)
                raise
        if installed:
            firewall.remove(table)
        shutil.rmtree(root)
        if built_images:
            run([*DOCKER, "image", "rm", *built_images])
    print("A01–A04 passed", flush=True)


def terminate(signum, _frame):
    raise KeyboardInterrupt(f"received signal {signum}")


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, terminate)
    main()
