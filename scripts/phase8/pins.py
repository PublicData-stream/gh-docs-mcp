"""Read infrastructure image pins from the actual Compose inputs."""

import re

from images import ROOT


def infrastructure():
    base = ROOT / "compose.yml"
    inputs = {
        "postgres": (base, "docker.io/library/postgres"),
        "initializer": (base, "docker.io/library/alpine"),
        "fixture-proxy": (base, "docker.io/library/nginx"),
        "proxy": (ROOT / "deploy/compose.release.yml", "ghcr.io/oxibelt/oxibelt-dataplane-strict"),
    }
    return {
        role: re.search(re.escape(repo) + r":[a-zA-Z0-9_.-]+@sha256:[0-9a-f]{64}", file.read_text())[0]
        for role, (file, repo) in inputs.items()
    }


if __name__ == "__main__":
    print("\n".join(infrastructure().values()))
