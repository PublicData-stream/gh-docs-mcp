"""Read infrastructure image pins from the actual Compose inputs."""

import re

from images import ROOT


PREVIOUS_POSTGRES = (
    "docker.io/library/postgres:19.0-alpine3.24@sha256:"
    "cac45fb483caee3668946088a1c0532db89d5285c28dbfeac889b4068296e06f"
)


def infrastructure():
    base = ROOT / "compose.yml"
    inputs = {
        "initializer": (base, "docker.io/library/alpine"),
        "fixture-proxy": (base, "docker.io/library/nginx"),
        "proxy": (ROOT / "deploy/compose.release.yml", "ghcr.io/oxibelt/oxibelt-dataplane-strict"),
    }
    return {
        role: re.search(re.escape(repo) + r":[a-zA-Z0-9_.-]+@sha256:[0-9a-f]{64}", file.read_text())[0]
        for role, (file, repo) in inputs.items()
    }


if __name__ == "__main__":
    print("\n".join((*infrastructure().values(), PREVIOUS_POSTGRES)))
