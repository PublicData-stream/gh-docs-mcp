"""Install the existing role firewall with explicit control, DNS and HTTPS edges."""

import argparse
from ipaddress import ip_address, ip_network
from pathlib import Path
import re
import sys


sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "phase0"))
import firewall


def environment(path):
    values = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key, separator, value = line.partition("=")
        if not separator:
            raise ValueError("expected KEY=VALUE in the Compose network file")
        if key.endswith("_BRIDGE"):
            if not re.fullmatch("[a-zA-Z0-9_-]{1,15}", value):
                raise ValueError("invalid bridge name")
            values[key] = value
        elif key.endswith(("_IP", "_V6")):
            values[key] = str(ip_address(value))
    if not any(key.endswith("_BRIDGE") for key in values):
        raise ValueError("the Compose network file has no project bridges")
    return values


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("install", "remove"))
    parser.add_argument("--table", required=True)
    parser.add_argument("--compose-env", type=Path)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--resolver", type=ip_address)
    parser.add_argument("--https", action="append", type=ip_network, default=[])
    parser.add_argument("--control", action="store_true")
    parser.add_argument("--rustdoc-runner", action="store_true")
    parser.add_argument("--feed-resolver", type=ip_address)
    parser.add_argument("--feed-https", action="append", type=ip_network, default=[])
    args = parser.parse_args()
    if not re.fullmatch("[a-zA-Z][a-zA-Z0-9_]{0,31}", args.table):
        parser.error("invalid project nftables table")
    if args.action == "remove":
        firewall.remove(args.table)
        return
    if args.compose_env is None or args.output_dir is None:
        parser.error("install requires --compose-env and --output-dir")
    if bool(args.https) != (args.resolver is not None):
        parser.error("enable sync DNS and HTTPS together")
    if bool(args.feed_https) != (args.feed_resolver is not None):
        parser.error("enable feed DNS and HTTPS together")
    if args.rustdoc_runner and not args.control:
        parser.error("Rustdoc runner access requires the control bridges")
    if args.feed_https and not args.control:
        parser.error("feed HTTPS requires the control links")
    for net in (*args.https, *args.feed_https):
        if net.prefixlen == 0 or not net.network_address.is_global or not net.broadcast_address.is_global:
            parser.error("an HTTPS rule must name a public destination range")
    firewall.install(args.table, environment(args.compose_env), args.output_dir,
                     sync_https=args.https, sync_resolver=args.resolver, control=args.control,
                     feed_https=args.feed_https, feed_resolver=args.feed_resolver, rustdoc_runner=args.rustdoc_runner)


if __name__ == "__main__":
    main()
