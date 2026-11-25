"""Smoke-test the Phase 5 MCP role with both Git formats inside Minikube."""

import argparse
from pathlib import Path
import sys

scripts = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(scripts / phase) for phase in ("phase2", "phase4")]
from minikube import smoke

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--serve-image", required=True)
    parser.add_argument("--sync-image", required=True)
    parser.add_argument("--upstream-image", required=True)
    parser.add_argument("--postgres-image", required=True)
    parser.add_argument("--bootstrap-cache", type=Path)
    args = parser.parse_args()
    smoke(args, serve=args.serve_image, wire=True)
