"""Provision only synthetic inputs for a disposable rootful Compose project."""

import ipaddress
import os
from pathlib import Path
import secrets
import socket
import subprocess

VALUE = b"phase-zero fixture\n"

NGINX = """pid /tmp/nginx.pid;
error_log stderr warn;
events { worker_connections 64; }
http {
  access_log off;
  client_body_temp_path /tmp/client;
  proxy_temp_path /tmp/proxy;
  fastcgi_temp_path /tmp/fastcgi;
  uwsgi_temp_path /tmp/uwsgi;
  scgi_temp_path /tmp/scgi;
  client_body_timeout 6s;
  client_max_body_size 64k;
  %s
}
"""


def run(*args, **kwargs):
    return subprocess.run(
        args, check=True, timeout=20, stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs,
    ).stdout


def write(path, data, uid=None, mode=0o644):
    path.write_bytes(data if isinstance(data, bytes) else data.encode())
    path.chmod(mode)
    if uid is not None:
        os.chown(path, uid, uid)


def cert(directory, name, sans, uid):
    key = directory / "server.key"
    crt = directory / "server.crt"
    csr = directory / "server.csr"
    ext = directory / "extensions"
    write(ext, f"subjectAltName={sans}\nextendedKeyUsage=serverAuth\n"
          "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n"
          "subjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid:always\n")
    run("openssl", "req", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256",
        "-noenc", "-subj", f"/CN={name}", "-keyout", key, "-out", csr)
    run("openssl", "x509", "-req", "-in", csr, "-CA", directory.parent / "ca.crt",
        "-CAkey", directory.parent / "ca.key", "-set_serial", str(secrets.randbits(96)),
        "-days", "2", "-extfile", ext, "-out", crt)
    os.chmod(key, 0o400)
    os.chown(key, uid, uid)
    csr.unlink()
    ext.unlink()


def make_ca(root, basename):
    run("openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256",
        "-noenc", "-days", "2", "-subj", f"/CN=Phase 0 {basename}",
        "-addext", "basicConstraints=critical,CA:TRUE,pathlen:0",
        "-addext", "keyUsage=critical,keyCertSign,cRLSign",
        "-addext", "subjectKeyIdentifier=hash",
        "-keyout", root / f"{basename}.key", "-out", root / f"{basename}.crt")


def make_git(path, object_format):
    env = {
        "PATH": os.environ["PATH"],
        "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_AUTHOR_NAME": "Phase Zero", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
        "GIT_COMMITTER_NAME": "Phase Zero", "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
        "GIT_AUTHOR_DATE": "2026-01-01T00:00:00Z", "GIT_COMMITTER_DATE": "2026-01-01T00:00:00Z",
    }
    run("git", "init", "--bare", "--template=", "--initial-branch=main",
        f"--object-format={object_format}", path, env=env)

    def git(*args, **kwargs):
        return run("git", "-C", path, *args, env=env, **kwargs).strip().decode()

    blob = git("hash-object", "-w", "--stdin", input=VALUE)
    tree = git("mktree", input=f"100644 blob {blob}\tfixture.txt\n".encode())
    commit = git("commit-tree", tree, "-m", "Synthetic Phase 0 fixture")
    git("update-ref", "refs/heads/main", commit)
    return commit


def addresses(root):
    prefix = f"198.{18 + secrets.randbelow(2)}.{secrets.randbelow(256)}"
    v6prefix = f"fd{1 + secrets.randbelow(254):02x}:{secrets.randbits(16):x}:{secrets.randbits(16):x}"
    roles = [
        ("INGRESS", ["SERVE_INGRESS", "PROXY", "SINK_INGRESS"], True),
        ("READ", ["PG_READ", "SERVE_READ"], False),
        ("WRITE", ["PG_WRITE", "SYNC_WRITE", "MIGRATE"], False),
        ("CONTROL", ["SYNC_CONTROL", "FEED_CONTROL"], False),
        ("NOTIFY", ["SERVE_NOTIFY", "SYNC_NOTIFY"], False),
        ("SYNC", ["SYNC_EGRESS", "SINK_SYNC"], True),
        ("FEED", ["FEED_EGRESS", "SINK_FEED"], True),
    ]
    result = {"FIXTURES": str(root)}
    bridge_prefix = f"ghd{secrets.token_hex(4)}"
    for index, (network, members, ipv6) in enumerate(roles):
        cidr = ipaddress.ip_network(f"{prefix}.{index * 16}/28")
        result[f"{network}_CIDR"] = str(cidr)
        result[f"{network}_BRIDGE"] = f"{bridge_prefix}{index}"
        if ipv6:
            cidr6 = ipaddress.ip_network(f"{v6prefix}:{index:x}::/64")
            result[f"{network}_CIDR_V6"] = str(cidr6)
        for offset, member in enumerate(members, 2):
            result[f"{member}_IP"] = str(cidr[offset])
            if ipv6:
                result[f"{member}_V6"] = str(cidr6[offset])
    with socket.socket() as reserved:
        reserved.bind(("127.0.0.1", 0))
        result["PROXY_PORT"] = str(reserved.getsockname()[1])
    return result


def prepare(root):
    env = addresses(root)
    uids = {"serve": 10001, "sync": 10002, "feed": 10003, "migrate": 10004, "postgres": 70, "proxy": 101}
    for name, uid in uids.items():
        (root / name).mkdir(mode=0o700)
        os.chown(root / name, uid, uid)
    for name in ("store", "staging", "pgdata", "sink"):
        (root / name).mkdir(mode=0o755)
    make_ca(root, "ca")
    make_ca(root, "wrong-ca")
    cert(root / "postgres", "postgres.phase0.test", "DNS:postgres.phase0.test", uids["postgres"])
    cert(root / "proxy", "127.0.0.1", "IP:127.0.0.1", uids["proxy"])
    write(root / "postgres/admin-password", secrets.token_hex(24), uids["postgres"], 0o400)
    for role in ("serve", "sync", "migrate"):
        password = secrets.token_hex(24)
        write(root / role / "db-password", password, uids[role], 0o400)
        write(root / "postgres" / f"{role}-password", password, uids["postgres"], 0o400)
        write(root / role / "db-ca.crt", (root / "ca.crt").read_bytes())
        if role == "serve":
            write(root / role / "wrong-ca.crt", (root / "wrong-ca.crt").read_bytes())
    for name, roles in [("cursor", ["serve"]), ("feed-sync", ["feed", "sync"]), ("sync-serve", ["sync", "serve"])]:
        key = secrets.token_bytes(48)
        for role in roles:
            write(root / role / f"{name}.key", key, uids[role], 0o400)

    hba = [
        "local gh_docs postgres peer",
        "local all postgres peer",
        "local all all reject",
        f"hostssl gh_docs gh_docs_serve {env['SERVE_READ_IP']}/32 scram-sha-256",
        f"hostssl gh_docs gh_docs_sync {env['SYNC_WRITE_IP']}/32 scram-sha-256",
        f"hostssl gh_docs gh_docs_migrate {env['MIGRATE_IP']}/32 scram-sha-256",
        "host all all 0.0.0.0/0 reject",
        "host all all ::/0 reject",
    ]
    write(root / "postgres/pg_hba.conf", "\n".join(hba) + "\n")
    write(root / "proxy/nginx.conf", NGINX % f"""
    server {{
      listen 8443 ssl;
      listen [::]:8443 ssl;
      ssl_certificate /run/secrets/server.crt;
      ssl_certificate_key /run/secrets/server.key;
      location = /mcp {{
        proxy_http_version 1.1;
        proxy_ignore_client_abort off;
        proxy_read_timeout 20s;
        proxy_set_header Host $http_host;
        proxy_set_header Forwarded "";
        proxy_set_header X-Forwarded-For "";
        proxy_set_header X-Forwarded-Host "";
        proxy_set_header X-Forwarded-Proto "";
        proxy_set_header X-Real-IP "";
        proxy_pass http://{env['SERVE_INGRESS_IP']}:8080;
      }}
      location / {{ return 404; }}
    }}
    """)
    write(root / "sink/nginx.conf", NGINX % """
    server {
      listen 8080;
      listen [::]:8080;
      listen 8081;
      listen [::]:8081;
      location / { return 200 "phase0-sink"; }
    }
    """)
    repos = root / "store/repositories"
    repos.mkdir(mode=0o755)
    os.chown(repos, uids["sync"], uids["sync"])
    for name, object_format in [("selected", "sha1"), ("unrelated", "sha1"), ("sha256", "sha256")]:
        commit = make_git(repos / f"{name}.git", object_format)
        print(f"fixture {name} {object_format} {commit}", flush=True)
    write(root / "store/fixture.txt", VALUE)
    write(root / "store/fence.lock", b"", uids["sync"])
    return env
