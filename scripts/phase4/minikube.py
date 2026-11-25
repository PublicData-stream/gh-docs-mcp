"""Smoke-test Phase 4 exploration with both Git formats in a disposable Minikube Job."""

import argparse
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "phase2"))
from minikube import smoke


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--serve-image", required=True)
    parser.add_argument("--sync-image", required=True)
    parser.add_argument("--upstream-image", required=True)
    parser.add_argument("--postgres-image", required=True)
    parser.add_argument("--bootstrap-cache", type=Path)
    args = parser.parse_args()
    smoke(args, serve=args.serve_image, explore=True)
