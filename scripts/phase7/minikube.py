"""Repeat the local feed-to-MCP smoke and check effective search confinement."""

import argparse
import importlib.util
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("phase6_minikube", SCRIPTS / "phase6/minikube.py")
phase6 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(phase6)


def resources(root, args):
    manifest = phase6.phase6(root, args)
    pod = manifest["items"][-1]["spec"]["template"]["spec"]
    unpack = pod["initContainers"][0]
    unpack["command"][2] += """
      mkdir -p /canonical/role/repositories/selected.git /canonical/role/repositories/unrelated.git
      printf 'synthetic content\\n' > /canonical/role/fixture.txt
      for repo in selected unrelated; do
        printf 'ref: refs/heads/main\\n' > "/canonical/role/repositories/$repo.git/HEAD"
      done
    """
    pod["volumes"].append({"name": "probe-scratch", "emptyDir": {"medium": "Memory", "sizeLimit": "2Mi"}})
    probe = phase6.container(
        "confinement", args.serve_image, 10001, ["/bin/sh", "-ec", """
        gh-docs-search-worker --probe-denials
        test "$(gh-docs-search-worker --fail-setup)" = -
        test ! -e /git-sync-private
        echo 'Framed worker denial and failed setup passed'
        """],
        [phase6.mount("canonical", "/git-store", True, "role"),
         phase6.mount("serve-auth", "/run/secrets", True), phase6.mount("probe-scratch", "/tmp")],
    )
    pod["initContainers"].insert(1, probe)
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for role in ("serve", "sync", "feed", "upstream", "postgres"):
        parser.add_argument(f"--{role}-image", required=True)
    parser.add_argument("--bootstrap-cache", type=Path)
    parser.add_argument("--image-load-timeout", type=int, default=180, help="seconds to import the local image bundle")
    args = parser.parse_args()
    if not 1 <= args.image_load_timeout <= 600:
        parser.error("--image-load-timeout must be between 1 and 600")
    phase6.smoke(args, serve=args.serve_image, control=resources, extra_logs=("confinement",), phase=7)


if __name__ == "__main__":
    main()
