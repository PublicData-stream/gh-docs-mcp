"""Run one Phase 6 hint-to-MCP smoke in disposable rootless Minikube."""

import argparse
import json
from pathlib import Path
import secrets
import sys

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(SCRIPTS / phase) for phase in ("phase2", "phase4", "phase5")]
from minikube import NAMESPACE, as_b64, container, mount, resources, secret, smoke, wire_commands
from git_fixtures import PHRASE, policy
from smart_fixtures import REPOS


def phase6(root, args):
    manifest = resources(root, args.sync_image, args.upstream_image, args.postgres_image,
                         args.serve_image, wire=True)
    items = manifest["items"]
    job = items[-1]
    pod = job["spec"]["template"]["spec"]
    sync, serve = pod["containers"]
    repo = REPOS["sha1"][0]
    oracle = json.loads((root / "fixture/sha1-oracle.json").read_text())
    config = next(item for item in items if item["metadata"]["name"] == "fixture-config")["data"]
    config["sync-roster.json"] = json.dumps([repo])
    config["feed-roster.json"] = json.dumps([policy("sha1")])

    feed_key, notify_key = secrets.token_bytes(48), secrets.token_bytes(48)
    def attach_key(role, name, key):
        entry = next(item for item in items if item["metadata"]["name"] == role)
        entry["data"][name] = as_b64(key)
    attach_key("serve", "sync-serve.key", notify_key)
    attach_key("sync", "sync-serve.key", notify_key)
    attach_key("sync", "feed-sync.key", feed_key)
    feed_secret = secret("feed", {"feed-sync.key": feed_key,
                                  "upstream-ca.crt": (root / "fixture/tls/ca.crt").read_bytes()})
    feed_secret["metadata"]["namespace"] = NAMESPACE
    items.insert(-1, feed_secret)
    for volume in pod["volumes"]:
        if volume["name"] in ("serve-auth", "sync-auth"):
            volume["secret"]["defaultMode"] = 0o440
    pod["volumes"].append({"name": "feed-auth", "secret": {"secretName": "feed", "defaultMode": 0o440}})

    # Delay periodic reconciliation so only the real feed hint admits the job.
    statement = (f"INSERT INTO git_catalog.sync_schedule(repository_id,next_run_at) "
                 f"VALUES ('{repo}',clock_timestamp() + interval '1 day')")
    seed = container("schedule", args.postgres_image, 70,
                     ["psql", "-X", "-v", "ON_ERROR_STOP=1", "-h", "/run/postgresql", "-U", "postgres",
                      "-d", "gh_docs", "-c", statement], [mount("postgres-run", "/run/postgresql", True)])

    def add_env(spec, **settings):
        spec["env"].extend({"name": key, "value": value} for key, value in settings.items())

    ready = lambda port: ["wget", "-qO", "/dev/null", f"http://127.0.0.1:{port}/healthz"]
    serve.update(name="serve", command=["gh-docs-serve"], restartPolicy="Always",
                 startupProbe={"exec": {"command": ready(8081)}, "periodSeconds": 1, "failureThreshold": 35})
    add_env(serve, GH_DOCS_CONTROL_ENABLED="1", GH_DOCS_SERVE_NOTIFY_BIND="127.0.0.1:8784",
            GH_DOCS_SYNC_SERVE_KEY_ID="v1", GH_DOCS_SYNC_SERVE_KEY_FILE="/run/secrets/sync-serve.key")
    # Compose supplies an init process. Keep the daemon below PID 1 here as
    # well so the existing Git parent-death check can detect an orphan.
    supervise = """gh-docs-sync & child=$!
trap 'kill -TERM "$child"; wait "$child" || true' TERM INT
wait "$child"
"""
    sync.update(name="sync", command=["/bin/sh", "-ec", supervise], restartPolicy="Always",
                startupProbe={"exec": {"command": ready(8082)}, "periodSeconds": 1, "failureThreshold": 35})
    sync["volumeMounts"].append(mount("configuration", "/fixtures", True))
    add_env(sync, HEALTH_BIND="127.0.0.1:8082", GH_DOCS_CONTROL_ENABLED="1",
            GH_DOCS_SYNC_CONTROL_BIND="127.0.0.1:8783", GH_DOCS_SYNC_SERVE_ADDR="127.0.0.1:8784",
            GH_DOCS_FEED_SYNC_KEY_ID="v1", GH_DOCS_FEED_SYNC_KEY_FILE="/run/secrets/feed-sync.key",
            GH_DOCS_SYNC_SERVE_KEY_ID="v1", GH_DOCS_SYNC_SERVE_KEY_FILE="/run/secrets/sync-serve.key",
            GH_DOCS_SYNC_ROSTER_FILE="/fixtures/sync-roster.json",
            GH_DOCS_PHASE6_UPSTREAM="127.0.0.1:8443", GH_DOCS_PHASE6_UPSTREAM_CA="/run/secrets/upstream-ca.crt")
    feed = container("feed", args.feed_image, 10003, ["gh-docs-feed"],
        [mount("feed-auth", "/run/secrets", True), mount("configuration", "/fixtures", True)],
        env={"HEALTH_BIND": "127.0.0.1:8083", "GH_DOCS_CONTROL_ENABLED": "1",
             "GH_DOCS_FEED_SYNC_ADDR": "127.0.0.1:8783", "GH_DOCS_FEED_SYNC_KEY_ID": "v1",
             "GH_DOCS_FEED_SYNC_KEY_FILE": "/run/secrets/feed-sync.key", "GH_DOCS_FEED_ROSTER_FILE": "/fixtures/feed-roster.json",
             "GH_DOCS_PHASE6_UPSTREAM": "127.0.0.1:8443", "GH_DOCS_PHASE6_UPSTREAM_CA": "/run/secrets/upstream-ca.crt"},
        sidecar=ready(8083))
    pod["initContainers"].extend([seed, serve, sync, feed])
    context = {"repository_id": repo, "generation_id": "0", "object_format": "sha1", "commit_oid": oracle["head"]}
    commands = [
        f"""timeout 100 sh -ec 'until gh-docs-sync --status --repository {repo} | grep -q "delivered.*true"; do sleep 1; done'""",
        *wire_commands("sha1", context, PHRASE),
        "echo 'Phase 6 feed hint, scheduled publication, authenticated acknowledgment and MCP read passed'",
    ]
    status_env = {field["name"]: field["value"] for field in sync["env"] if field["name"].startswith("DB_")}
    pod["containers"] = [container("control-smoke", args.sync_image, 10002, ["/bin/sh", "-ec", "; ".join(commands)],
                                   [mount("sync-auth", "/run/secrets", True)], env=status_env)]
    return manifest


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for role in ("serve", "sync", "feed", "upstream", "postgres"):
        parser.add_argument(f"--{role}-image", required=True)
    parser.add_argument("--bootstrap-cache", type=Path)
    args = parser.parse_args()
    smoke(args, serve=args.serve_image, control=phase6)
