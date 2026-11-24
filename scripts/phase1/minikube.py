#!/usr/bin/env python3
"""Run the confined native readers in a disposable Minikube Job."""

import argparse
import base64
import json
import os
from pathlib import Path
import secrets
import shlex
import shutil
import signal
import subprocess
import tarfile
import tempfile

from native_fixtures import generate


def job_resources(image, bundle):
    security = {
        "allowPrivilegeEscalation": False,
        "capabilities": {"drop": ["ALL"]},
        "readOnlyRootFilesystem": True,
        "runAsNonRoot": True,
    }

    def container(name, uid, command, mounts):
        return {
            "name": name, "image": image, "imagePullPolicy": "Never", "command": command,
            "securityContext": {**security, "runAsUser": uid, "runAsGroup": uid},
            "resources": {"limits": {"cpu": "1", "memory": "256Mi"}},
            "volumeMounts": mounts,
        }

    return {
        "apiVersion": "v1", "kind": "List",
        "items": [
            {
                "apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "reader-fixture"},
                "binaryData": {"fixture.tar.gz": base64.b64encode(bundle.read_bytes()).decode()},
            },
            {
                "apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": "confined-reader"},
                "spec": {
                    "backoffLimit": 0, "activeDeadlineSeconds": 150,
                    "template": {"spec": {
                        "restartPolicy": "Never", "automountServiceAccountToken": False,
                        "securityContext": {"fsGroup": 10002, "seccompProfile": {"type": "RuntimeDefault"}},
                        "volumes": [
                            {"name": "archive", "configMap": {"name": "reader-fixture"}},
                            {"name": "fixture", "emptyDir": {"sizeLimit": "32Mi"}},
                        ],
                        "initContainers": [container(
                            "unpack", 10002,
                            ["/bin/sh", "-ec", "tar -xzf /archive/fixture.tar.gz -C /data\n"
                             "find /data/store -type d -exec chmod g-s '{}' +"],
                            [{"name": "fixture", "mountPath": "/data"},
                             {"name": "archive", "mountPath": "/archive", "readOnly": True}],
                        )],
                        "containers": [container(
                            "reader", 10001,
                            ["/bin/sh", "-ec",
                             "test ! -e /git-sync-private\n"
                             "gh-docs-phase1-fixture read /phase1-oracle.json\n"
                             "gh-docs-phase1-fixture fail-setup /phase1-oracle.json\n"
                             "echo 'sandbox setup failure denied for both formats'"],
                            [
                                {"name": "fixture", "mountPath": "/git-store",
                                 "subPath": "store", "readOnly": True},
                                {"name": "fixture", "mountPath": "/phase1-oracle.json",
                                 "subPath": "oracle.json", "readOnly": True},
                            ],
                        )],
                    }},
                },
            },
        ],
    }


def run_smoke(args):
    profile = f"ghdocs1-{secrets.token_hex(5)}"
    root = Path(tempfile.mkdtemp(prefix=f"{profile}-minikube-", dir="/tmp"))
    environment = {
        **os.environ, "MINIKUBE_HOME": str(root), "KUBECONFIG": str(root / "kubeconfig"),
        "TMPDIR": str(root), "DOCKER_CONTEXT": args.docker_context, "MINIKUBE_IN_STYLE": "false",
    }
    environment.pop("DOCKER_HOST", None)
    minikube = ["minikube", "-p", profile]
    if args.docker_context == "rootless":
        minikube.append("--rootless")
    kubectl = [*minikube, "kubectl", "--", "--cache-dir", str(root / "kubecache")]
    docker = ["docker", "--context", "default"]

    def run(command, *, timeout=45, check=True):
        print(shlex.join(map(str, command)), flush=True)
        return subprocess.run(command, env=environment, check=check, timeout=timeout)

    started = False
    try:
        if args.bootstrap_cache:
            cache = root / ".minikube/cache"
            cache.mkdir(parents=True)
            for name in ("kic", "linux", "preloaded-tarball"):
                if (args.bootstrap_cache / name).is_dir():
                    shutil.copytree(args.bootstrap_cache / name, cache / name)
        records = generate(root / "store", root / "oracle.json")
        for record in records:
            lock = root / "store/locks" / record["repo"]
            lock.mkdir(parents=True, mode=0o755)
            (lock / "repository.lock").touch()
            (lock / "repository.lock").chmod(0o644)
            lock.chmod(0o755)
        (root / "store/locks").chmod(0o755)
        bundle = root / "fixture.tar.gz"
        with tarfile.open(bundle, "w:gz") as archive:
            archive.add(root / "store", "store")
            archive.add(root / "oracle.json", "oracle.json")
        manifest = root / "job.json"
        manifest.write_text(json.dumps(job_resources(args.serve_image, bundle)))
        image = root / "serve.tar"
        run([*docker, "image", "inspect", "--format", "{{.Id}}", args.serve_image])
        run([*docker, "image", "save", "-o", image, args.serve_image], timeout=120)
        run(["minikube", "version", "--short"])
        started = True
        run([*minikube, "start", "--driver=docker", "--container-runtime=containerd", "--nodes=1",
             "--cpus=2", "--memory=3072", "--listen-address=127.0.0.1", "--install-addons=false",
             "--wait-timeout=180s", f"--kubernetes-version={args.kubernetes_version}"], timeout=480)
        run([*minikube, "image", "load", image], timeout=150)
        run([*kubectl, "version", "-o", "json"])
        run([*kubectl, "apply", "-f", manifest])
        wait = run([*kubectl, "wait", "--for=condition=complete", "job/confined-reader",
                    "--timeout=150s"], check=False, timeout=170)
        if wait.returncode:
            run([*kubectl, "describe", "pod", "-l", "job-name=confined-reader"], check=False)
            run([*kubectl, "logs", "job/confined-reader", "-c", "unpack"], check=False)
        run([*kubectl, "logs", "job/confined-reader", "-c", "reader"], check=wait.returncode == 0)
        wait.check_returncode()
    finally:
        cleaned = not started
        if started:
            try:
                cleaned = run([*minikube, "delete"], timeout=100, check=False).returncode == 0
            except subprocess.TimeoutExpired:
                pass
        if not cleaned:
            raise RuntimeError(f"Minikube cleanup incomplete for {profile}; state retained at {root}")
        shutil.rmtree(root)
    print("Supplemental Minikube confined reader smoke passed", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--serve-image", required=True)
    parser.add_argument("--kubernetes-version", default="v1.37.0")
    parser.add_argument("--docker-context", choices=("default", "rootless"), default="default")
    parser.add_argument("--bootstrap-cache", type=Path, help="read-only source of cached Minikube bootstrap inputs")
    run_smoke(parser.parse_args())


def terminate(signum, _frame):
    raise KeyboardInterrupt(f"received signal {signum}")


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, terminate)
    main()
