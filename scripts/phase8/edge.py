#!/usr/bin/env python3
"""Render the OxiBelt strict data-plane configuration to stdout."""

import argparse
import ipaddress
import json
import re


def render(host, proxy_ip, serve_ip):
    if not re.fullmatch(r"[a-z0-9](?:[a-z0-9.-]{0,251}[a-z0-9])?", host):
        raise ValueError("expected a lowercase DNS name without a port")
    if any(not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", label)
           for label in host.split(".")):
        raise ValueError("invalid DNS labels")
    proxy = ipaddress.IPv4Address(proxy_ip)
    serve = ipaddress.IPv4Address(serve_ip)
    name = json.dumps(host)
    return f"""[runtime.accept]
workers = 1

[runtime.hardening.seccomp]
expectation = "required"

[listeners]
https_binds = ["{proxy}:8443"]
http_mode = "off"
http1 = true
http2 = true
http3 = false

[tls]
cert_chain = "server.crt"
private_key = "server.key"
server_names = [{name}]
require_sni = true
reject_unknown_sni = true
min_version = "tls1.3"

[proxy.forwarded_headers]
mode = "overwrite"
client_ip_source = "direct_peer"

[proxy.real_ip]
enabled = false

[proxy.retry]
enabled = false

[compression]
enabled = false

[cache]
enabled = false

[metrics]
enabled = true
bind = "127.0.0.1:9090"

[health]
enabled = true
bind = "127.0.0.1:9091"

[[upstreams]]
name = "serve"
origin = "http://{serve}:8080"

[[routes]]
name = "mcp"
hosts = [{name}]
upstream = "serve"
compression = "off"

[routes.match.path]
exact = "/mcp"
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", required=True)
    parser.add_argument("--proxy-ip", required=True)
    parser.add_argument("--serve-ip", required=True)
    args = parser.parse_args()
    print(render(args.host, args.proxy_ip, args.serve_ip), end="")


if __name__ == "__main__":
    main()
