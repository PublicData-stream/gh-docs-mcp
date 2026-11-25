"""Run the Phase 2 HTTPS smoke in a disposable Restricted Minikube profile."""

import argparse
from base64 import b64encode
import json
import os
from pathlib import Path
import secrets
import shlex
import shutil
import subprocess
import tarfile
import tempfile

from smart_fixtures import REPOS, approval, prepare, run


ROOT = Path(__file__).resolve().parents[2]
POSTGRES = "docker.io/library/postgres:19.0-alpine3.24@sha256:cac45fb483caee3668946088a1c0532db89d5285c28dbfeac889b4068296e06f"
NAMESPACE = "gh-docs-phase2"


def as_b64(value):
    return b64encode(value.encode() if isinstance(value, str) else value).decode()


def secret(name, values):
    return {"apiVersion": "v1", "kind": "Secret", "metadata": {"name": name},
            "data": {key: as_b64(value) for key, value in values.items()}}


def db_certificates(root):
    ca = root / "postgres-ca.crt"
    key = root / "postgres-ca.key"
    run("openssl", "req", "-new", "-x509", "-nodes", "-days", "2", "-newkey", "rsa:2048",
        "-subj", "/CN=Synthetic PostgreSQL CA", "-keyout", key, "-out", ca)
    server_key = root / "server.key"
    csr = root / "server.csr"
    certificate = root / "server.crt"
    extensions = root / "postgres-extensions"
    extensions.write_text("subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n")
    run("openssl", "req", "-new", "-nodes", "-newkey", "rsa:2048",
        "-subj", "/CN=localhost", "-keyout", server_key, "-out", csr)
    run("openssl", "x509", "-req", "-in", csr, "-CA", ca, "-CAkey", key,
        "-CAcreateserial", "-out", certificate, "-days", "2", "-extfile", extensions)
    return ca.read_bytes(), certificate.read_bytes(), server_key.read_bytes()


def mount(name, path, read_only=False, subpath=None):
    value = {"name": name, "mountPath": path, "readOnly": read_only}
    if subpath is not None:
        value["subPath"] = subpath
    return value


def container(name, image, uid, command, mounts, memory="256Mi", cpu="1", env=None, sidecar=None):
    spec = {
        "name": name, "image": image, "imagePullPolicy": "Never", "command": command,
        "securityContext": {"runAsUser": uid, "runAsGroup": uid, "allowPrivilegeEscalation": False,
                            "readOnlyRootFilesystem": True, "capabilities": {"drop": ["ALL"]}},
        "volumeMounts": mounts, "resources": {"limits": {"cpu": cpu, "memory": memory},
                                            "requests": {"cpu": "50m", "memory": "64Mi"}},
        "env": [{"name": key, "value": value} for key, value in (env or {}).items()],
    }
    if sidecar is not None:
        spec["restartPolicy"] = "Always"
        spec["startupProbe"] = {"exec": {"command": sidecar}, "periodSeconds": 1, "failureThreshold": 70}
    return spec


def resources(root, sync, upstream, postgres, serve=None, *, explore=False):
    fixture = root / "fixture"
    prepare(fixture)
    archive = root / "fixture.tar.gz"
    with tarfile.open(archive, "w:gz") as bundle:
        for path in ("repositories", "tls", "mode"):
            bundle.add(fixture / path, arcname=path)
    ca, certificate, key = db_certificates(root)
    passwords = {f"{role}-password": secrets.token_hex(24)
                 for role in ("admin", "serve", "migrate", "sync")}
    policies = {fmt: approval(fmt) for fmt in REPOS}
    if explore:
        for policy in policies.values():
            policy["exploration"] = {"version": 1, "metadata_notice": "Synthetic metadata",
                                     "scope": {"kind": "whole", "notice": "Synthetic source bytes"}}
    objects = [
        {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": NAMESPACE, "labels": {
            "pod-security.kubernetes.io/enforce": "restricted"}}},
        secret("fixture-bundle", {"fixture.tar.gz": archive.read_bytes()}),
        secret("postgres", {**passwords, "server.crt": certificate, "server.key": key}),
        secret("migrate", {"db-ca.crt": ca, "db-password": passwords["migrate-password"]}),
        secret("sync", {"db-ca.crt": ca, "db-password": passwords["sync-password"],
                        "upstream-ca.crt": (fixture / "tls/ca.crt").read_bytes()}),
        {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "fixture-config"}, "data": {
            "bootstrap.sh": (ROOT / "scripts/phase0/bootstrap-db.sh").read_text(),
            **{f"{fmt}.json": json.dumps(policy) for fmt, policy in policies.items()},
        }},
    ]
    db_client = {"DB_HOST": "localhost", "DB_CA_FILE": "/run/secrets/db-ca.crt",
                 "DB_PASSWORD_FILE": "/run/secrets/db-password"}
    init = [
        container("unpack", upstream, 10002, ["/bin/sh", "-ec",
            "tar -xzf /fixture-bundle/fixture.tar.gz -C /fixture; "
            "mkdir -m 0755 /canonical/role; mkdir -m 0700 /private/role; "
            "chmod g-s /canonical/role /private/role; chmod 0755 /canonical/role; chmod 0700 /private/role"],
            [mount("fixture-bundle", "/fixture-bundle", True), mount("upstream-data", "/fixture"),
             mount("canonical", "/canonical"), mount("private", "/private")]),
        container("postgres", postgres, 70, ["/bin/sh", "-ec",
            "umask 077; cp /run/secrets/server.key /run/postgresql/server.key; "
            "chmod 600 /run/postgresql/server.key; "
            "exec docker-entrypoint.sh postgres -c listen_addresses=127.0.0.1 "
            "-c ssl=on -c ssl_cert_file=/run/secrets/server.crt -c ssl_key_file=/run/postgresql/server.key"],
            [mount("postgres-auth", "/run/secrets", True), mount("pgdata", "/pgdata"),
             mount("postgres-run", "/run/postgresql"),
             mount("configuration", "/docker-entrypoint-initdb.d/bootstrap.sh", True, "bootstrap.sh")],
            "512Mi", "1", {"PGDATA": "/pgdata/data", "POSTGRES_DB": "gh_docs",
                           "POSTGRES_PASSWORD_FILE": "/run/secrets/admin-password",
                           "POSTGRES_INITDB_ARGS": "--encoding=UTF8 --locale-provider=icu --icu-locale=und --auth-local=trust --auth-host=scram-sha-256"},
            ["pg_isready", "-h", "127.0.0.1", "-d", "gh_docs"]),
        container("upstream", upstream, 10002, ["python3", "-B", "/opt/upstream.py"],
            [mount("upstream-data", "/fixture", True)],
            sidecar=["wget", "-qO", "/dev/null", "http://127.0.0.1:8088/health"]),
        container("registration", sync, 10004, ["/bin/sh", "-ec",
            "gh-docs-sync --migrate; "
            "gh-docs-sync --register-source --approval-file /fixtures/sha1.json; "
            "gh-docs-sync --register-source --approval-file /fixtures/sha256.json"],
            [mount("migration-auth", "/run/secrets", True), mount("configuration", "/fixtures", True)],
            env=db_client),
    ]
    commands = ["gh-docs-phase2-fixture --confinement-probe", *[
        f"gh-docs-phase2-fixture {repo} 127.0.0.1:8443 /run/secrets/upstream-ca.crt"
        for repo, _ in REPOS.values()
    ]]
    commands.append("test \"$(find /git-sync-private/incoming -name manifest.json | wc -l)\" -eq 2")
    commands.append("test -z \"$(ls -A /git-store/repositories)\"")
    sync_container = container("reconcile", sync, 10002, ["/bin/sh", "-ec", "; ".join(commands)],
        [mount("sync-auth", "/run/secrets", True), mount("canonical", "/git-store", subpath="role"),
         mount("private", "/git-sync-private", subpath="role"), mount("scratch", "/git-sync-scratch/1")],
        "3Gi", "2", {**db_client, "GH_DOCS_SYNC_CONCURRENCY": "1"})
    volumes = [
        {"name": name, "secret": {"secretName": credential}}
        for name, credential in [("fixture-bundle", "fixture-bundle"), ("postgres-auth", "postgres"),
                                 ("migration-auth", "migrate"), ("sync-auth", "sync")]
    ]
    volumes.append({"name": "configuration", "configMap": {"name": "fixture-config", "defaultMode": 0o555}})
    containers = [sync_container]
    if serve:
        # This local smoke shares an emptyDir for handoff; Docker Compose
        # remains the role isolation and fault-injection qualification.
        publish = [
            f"gh-docs-sync --{cmd} --repository {repo} {extra}"
            for repo, _ in REPOS.values()
            for cmd, extra in (("prepare", "--generation 0"), ("publish", "--generation 0 --expect-sequence 0"),
                               ("recover", ""))
        ]
        sync_container["command"][2] += "; " + "; ".join([*publish, "touch /handoff/published"])
        sync_container["volumeMounts"].append(mount("handoff", "/handoff"))
        oracle = [json.loads((fixture / f"{fmt}-oracle.json").read_text()) for fmt in REPOS]
        reads = [
            f"gh-docs-phase3-fixture read {repo} active {proof['initial']}"
            for (repo, _), proof in zip(REPOS.values(), oracle, strict=True)
        ]
        serve_command = ["test ! -w /git-store", "test ! -r /git-store/.publication",
                         "test ! -e /git-sync-private", *reads]
        serve_env = db_client.copy()
        credentials = {"db-ca.crt": ca, "db-password": passwords["serve-password"]}
        if explore:
            serve_env.update(EXPLORATION_CURSOR_KEY_FILE="/run/secrets/cursor.key",
                             EXPLORATION_CURSOR_KEY_ID="fixture-v1")
            credentials["cursor.key"] = secrets.token_bytes(48)
            for (fmt, (repo, _)), proof in zip(REPOS.items(), oracle, strict=True):
                context = {"repository_id": repo, "generation_id": "0",
                           "object_format": fmt, "commit_oid": proof["initial"]}
                request = {"tool": "git.read", "arguments": {"context": context,
                    "path": {"encoding": "utf8", "value": "original.bin"},
                    "range": {"start": 0, "end": 32}, "encoding": "base64"}}
                expected = as_b64((fixture / f"{fmt}-oracle.bin").read_bytes()[:32])
                serve_command.append(
                    f"printf '%s' {shlex.quote(json.dumps(request))} | gh-docs-phase4-fixture | "
                    f"grep -F {shlex.quote(json.dumps('data') + ':' + json.dumps(expected))}")
        containers.append(container("read", serve, 10001, ["/bin/sh", "-ec",
            "timeout 75 sh -ec 'while ! test -f /handoff/published; do sleep 1; done'; "
            + "; ".join(serve_command)],
            [mount("serve-auth", "/run/secrets", True), mount("handoff", "/handoff", True),
             mount("canonical", "/git-store", True, "role")], env=serve_env))
        objects.append(secret("serve", credentials))
        volumes.extend([{"name": "serve-auth", "secret": {"secretName": "serve"}},
                        {"name": "handoff", "emptyDir": {}}])
    volumes.extend({"name": name, "emptyDir": {}} for name in
                   ("upstream-data", "pgdata", "postgres-run", "canonical", "private"))
    volumes.append({"name": "scratch", "emptyDir": {"medium": "Memory", "sizeLimit": "1Gi"}})
    job = {"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "private-fetch"},
           "spec": {"backoffLimit": 0, "activeDeadlineSeconds": 230, "template": {"spec": {
               "automountServiceAccountToken": False, "restartPolicy": "Never",
               "securityContext": {"runAsNonRoot": True, "fsGroup": 10002,
                                   "seccompProfile": {"type": "RuntimeDefault"}},
               "initContainers": init, "containers": containers, "volumes": volumes}}}}
    for item in [*objects[1:], job]:
        item["metadata"]["namespace"] = NAMESPACE
    return {"apiVersion": "v1", "kind": "List", "items": [*objects, job]}


def smoke(args, serve=None, *, explore=False):
    phase = 4 if explore else 3 if serve else 2
    profile = f"ghdocs{phase}-{secrets.token_hex(5)}"
    root = Path(tempfile.mkdtemp(prefix=f"{profile}-minikube-", dir="/tmp"))
    environment = {**os.environ, "MINIKUBE_HOME": str(root), "KUBECONFIG": str(root / "kubeconfig"),
                   "TMPDIR": str(root), "DOCKER_CONTEXT": "rootless", "MINIKUBE_IN_STYLE": "false"}
    environment.pop("DOCKER_HOST", None)
    minikube = ["minikube", "-p", profile, "--rootless"]
    kubectl = [*minikube, "kubectl", "--", "--cache-dir", str(root / "kubecache")]
    started = False

    def execute(command, *, timeout=45, check=True, **options):
        print(" ".join(map(str, command)), flush=True)
        return subprocess.run(command, env=environment, check=check, timeout=timeout, **options)

    try:
        if args.bootstrap_cache:
            cache = root / ".minikube/cache"
            cache.mkdir(parents=True)
            for name in ("kic", "linux", "preloaded-tarball"):
                if (args.bootstrap_cache / name).is_dir():
                    shutil.copytree(args.bootstrap_cache / name, cache / name)
        docker = ["docker", "--context", "default"]
        # Keep an explicit local tag for the pinned PostgreSQL image in containerd.
        postgres = args.postgres_image
        bundle = root / "images.tar"
        images = [postgres, args.sync_image, args.upstream_image]
        if serve:
            images.append(serve)
        execute([*docker, "image", "inspect", "--format", "{{.Id}}", *images])
        execute([*docker, "image", "save", "-o", bundle, *images], timeout=150)
        manifest = root / "job.json"
        manifest.write_text(json.dumps(resources(
            root, args.sync_image, args.upstream_image, postgres, serve, explore=explore)))
        started = True
        execute([*minikube, "start", "--driver=docker", "--container-runtime=containerd", "--nodes=1",
                 "--cpus=4", "--memory=6144", "--listen-address=127.0.0.1", "--install-addons=false",
                 "--wait-timeout=180s", "--kubernetes-version=v1.37.0"], timeout=480)
        execute([*minikube, "image", "load", bundle], timeout=180)
        execute([*kubectl, "version", "-o", "json"])
        execute([*kubectl, "create", "-f", manifest])
        scope = [*kubectl, "-n", NAMESPACE]
        wait = execute([*scope, "wait", "--for=condition=complete", "job/private-fetch", "--timeout=240s"],
                       timeout=250, check=False)
        if wait.returncode:
            execute([*scope, "describe", "pod", "-l", "job-name=private-fetch"], check=False)
            for name in ("unpack", "postgres", "upstream", "registration"):
                execute([*scope, "logs", "job/private-fetch", "-c", name], check=False)
        execute([*scope, "logs", "job/private-fetch", "-c", "reconcile"], check=wait.returncode == 0)
        if serve:
            execute([*scope, "logs", "job/private-fetch", "-c", "read"], check=wait.returncode == 0)
        wait.check_returncode()
    finally:
        if started and execute([*minikube, "delete"], timeout=100, check=False).returncode:
            raise RuntimeError(f"cleanup pending for Minikube profile {profile} at {root}")
        shutil.rmtree(root)
    print(f"Phase {phase} Minikube smoke passed", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sync-image", required=True)
    parser.add_argument("--upstream-image", required=True)
    parser.add_argument("--postgres-image", required=True, help=f"local alias of {POSTGRES}")
    parser.add_argument("--bootstrap-cache", type=Path)
    smoke(parser.parse_args())
