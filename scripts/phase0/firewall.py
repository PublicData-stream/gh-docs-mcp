"""Rootful Compose traffic policy for one disposable project."""

from ipaddress import ip_address, ip_network
from pathlib import Path
from subprocess import run


def install(name, env, fixture_root, *, sync_fixture=False, sync_https=(), sync_resolver=None,
            control=False, feed_fixture=False, feed_https=(), feed_resolver=None, rustdoc_runner=False):
    # Bridge forwarding covers packets switched within a project bridge even
    # on hosts with br_netfilter disabled. Inet covers routed and host traffic.
    edges = [
        ("PROXY", "SERVE_INGRESS", 8080, ("IP",)),
        ("SERVE_READ", "PG_READ", 5432, ("IP",)),
        ("SYNC_WRITE", "PG_WRITE", 5432, ("IP",)),
        ("MIGRATE", "PG_WRITE", 5432, ("IP",)),
        ("SYNC_EGRESS", "SINK_SYNC", 8080, ("IP", "V6")),
        ("FEED_EGRESS", "SINK_FEED", 8080, ("IP", "V6")),
    ]
    if control:
        edges += [
            ("FEED_CONTROL", "SYNC_CONTROL", 8783, ("IP",)),
            ("SYNC_NOTIFY", "SERVE_NOTIFY", 8784, ("IP",)),
        ]
    bridges = ", ".join(f'"{value}"' for key, value in env.items() if key.endswith("_BRIDGE"))
    inet_scope = f"iifname {{ {bridges} }}"
    outgoing_scope = f"oifname {{ {bridges} }}"
    bridge_scope = f"meta ibrname {{ {bridges} }}"
    neighbor_discovery = "icmpv6 type { nd-neighbor-solicit, nd-neighbor-advert, nd-router-solicit, nd-router-advert }"
    allows = [
        f"{family} saddr {env[f'{src}_{suffix}']} {family} daddr {env[f'{dst}_{suffix}']} tcp dport {port} counter accept"
        for src, dst, port, families in edges
        for suffix, family in (("IP", "ip"), ("V6", "ip6"))
        if suffix in families
    ]
    if sync_fixture:
        for port in (8443, 8088):
            allows.append(f"ip saddr {env['SYNC_EGRESS_IP']} ip daddr {env['UPSTREAM_IP']} tcp dport {port} counter accept")
    if feed_fixture:
        allows.append(f"ip saddr {env['FEED_EGRESS_IP']} ip daddr {env['FEED_UPSTREAM_IP']} tcp dport 8443 counter accept")
    for role, ranges, resolver in (("SYNC", sync_https, sync_resolver), ("FEED", feed_https, feed_resolver)):
        for target in map(ip_network, ranges):
            family, suffix = ("ip", "IP") if target.version == 4 else ("ip6", "V6")
            allows.append(f"{family} saddr {env[f'{role}_EGRESS_{suffix}']} {family} daddr {target} tcp dport 443 counter accept")
        if resolver:
            target = ip_address(resolver)
            family, suffix = ("ip", "IP") if target.version == 4 else ("ip6", "V6")
            allows.append(f"{family} saddr {env[f'{role}_EGRESS_{suffix}']} {family} daddr {target} meta l4proto {{ udp, tcp }} th dport 53 counter accept")
    common = ["ct state established,related accept"]
    routed = [*common, f"{inet_scope} {neighbor_discovery} accept",
              *(f"{inet_scope} {rule}" for rule in allows),
              f"{inet_scope} counter drop", f"{outgoing_scope} counter drop"]
    switched = [*common, f"{bridge_scope} {neighbor_discovery} accept",
                *(f"{bridge_scope} {rule}" for rule in allows),
                f"{bridge_scope} ether type {{ ip, ip6 }} counter drop"]
    host_input = [*common, f"{inet_scope} {neighbor_discovery} accept", f"{inet_scope} counter drop"]
    host_output = [
        *common, f"{outgoing_scope} {neighbor_discovery} accept",
        f"{outgoing_scope} ip daddr {env['PROXY_IP']} tcp dport 8443 counter accept",
    ]
    if rustdoc_runner:
        host_output.append(f'oifname "{env["CONTROL_BRIDGE"]}" ip daddr {env["SYNC_CONTROL_IP"]} tcp dport 8785 counter accept')
    host_output.append(f"{outgoing_scope} counter drop")

    def chain(name, hook, rules):
        return f"chain {name} {{ type filter hook {hook} priority -5; policy accept;\n" + "\n".join(rules) + "\n}"

    config = Path(fixture_root, "firewall.nft")
    config.write_text(
        f"table inet {name} {{\n" + chain("routed", "forward", routed) + "\n"
        + chain("host_in", "input", host_input) + "\n"
        + chain("host_out", "output", host_output) + "\n}\n"
        + f"table bridge {name} {{\n" + chain("switched", "forward", switched) + "\n}\n"
    )
    run(["nft", "-f", str(config)], check=True, timeout=12)


def remove(name):
    for family in ("bridge", "inet"):
        run(["nft", "delete", "table", family, name], check=True, timeout=12)
