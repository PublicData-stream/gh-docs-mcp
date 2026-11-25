"""Qualify the production search and history MCP service through disposable TLS ingress."""

import argparse
from base64 import urlsafe_b64decode, urlsafe_b64encode
from contextlib import closing
from datetime import datetime, timezone
import hmac
import http.client
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import ssl
import sys
import time

from jsonschema import Draft202012Validator

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts/phase4"))
spec = importlib.util.spec_from_file_location("phase4_integration", ROOT / "scripts/phase4/integration.py")
phase4 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(phase4)
phase2 = phase4.phase2
from git_fixtures import NEW, OLD, PATH, PHRASE, policy, prepare
from mcp_wire import VERSION, message
from object_fixtures import decoded, encoded
from smart_fixtures import REPOS, git, repo_path


class Wire:
    def __init__(self, stack):
        self.port = int(stack.env["PROXY_PORT"])
        self.ca = Path(stack.root, "ca.crt")
        self.ssl = ssl.create_default_context(cafile=self.ca)
        self.stack = stack
        self.tools = {}

    def start(self, method, args, *, changes=None, request_id=1):
        packet = message(method, **args)
        packet["id"] = request_id
        headers = {
            "Accept": "application/json, text/event-stream",
            "Content-Type": "application/json",
            "Mcp-Protocol-Version": VERSION,
            "Mcp-Method": method,
        }
        if method == "tools/call":
            headers["Mcp-Name"] = args["name"]
        headers.update(changes or {})
        client = http.client.HTTPSConnection("127.0.0.1", self.port, context=self.ssl, timeout=22)
        client.request("POST", "/mcp", json.dumps(packet).encode(), headers)
        return client

    def send(self, method, args, *, status=200, request_id=1, **options):
        with closing(self.start(method, args, request_id=request_id, **options)) as client:
            response = client.getresponse()
            body = response.read()
            assert response.status == status, (response.status, body[:512])
            assert len(body) <= 256 * 1024
            assert response.getheader("Mcp-Session-Id") is None
            if status != 200 or not body:
                return {}
            result = json.loads(body)
            assert result["id"] == request_id
            return result

    def call(self, name, arguments, *, error=None, validate=True, request_id=1):
        tool = self.tools[name]
        if validate:
            Draft202012Validator(tool["inputSchema"]).validate(arguments)
        result = self.send("tools/call", {"name": name, "arguments": arguments}, request_id=request_id)["result"]
        assert result["resultType"] == "complete", result
        content = result["structuredContent"]
        assert json.loads(result["content"][0]["text"]) == content
        Draft202012Validator(tool["outputSchema"]).validate(content)
        assert result["isError"] is bool(error), result
        if error:
            assert content["code"] == error, result
        return content

    def validate_registry(self):
        assert self.send("server/discover", {})["result"]["supportedVersions"] == [VERSION]
        registry = self.send("tools/list", {})["result"]["tools"]
        self.tools = {tool["name"]: tool for tool in registry}
        assert len(self.tools) == len(registry) == 12
        snapshot = json.loads((ROOT / "apps/schemas/tools.json").read_text())
        assert registry == snapshot
        assert json.loads(self.stack.exec("serve", "gh-docs-serve", "--tool-schemas").stdout) == snapshot
        for tool in registry:
            assert tool["annotations"]["readOnlyHint"]
            Draft202012Validator.check_schema(tool["inputSchema"])
            Draft202012Validator.check_schema(tool["outputSchema"])
        self.send("tools/list", {}, status=403, changes={"Origin": "https://invalid.test"})
        self.send("tools/list", {}, status=403, changes={"Host": "invalid.test"})
        assert "error" in self.send("tools/call", {"name": "git.fetch", "arguments": {}})
        self.call("git.log", {"arbitrary": "FLAGS"}, error="invalid_argument", validate=False)

    def resolve(self, repo, oid=None, generation=None):
        args = {"repository_id": repo, "selector": phase4.by_oid(oid) if oid else {"kind": "default"}}
        if generation is not None:
            args["generation_id"] = str(generation)
        return self.call("git.resolve", args)["context"]


def publish(stack, config, endpoint, fmt, version=1, seq=0, enabled=True):
    phase2.register(stack, config, policy(fmt, version, enabled=enabled))
    repo = REPOS[fmt][0]
    generation = int(phase2.fetch(stack, repo, endpoint)["generation"])
    phase4.prepare_publish(stack, repo, generation, seq)
    return generation


def migration_and_legacy(wire, config, endpoint):
    stack = wire.stack
    repo = REPOS["sha1"][0]
    publish(stack, config, endpoint, "sha1", enabled=False)
    old = wire.resolve(repo)
    assert wire.call("git.read", {"context": old, "path": encoded(PATH)})["data"].encode() == NEW
    wire.call("git.rg", {"context": old, "pattern": PHRASE}, error="policy_denied")
    wire.call("git.log", {"context": old}, error="policy_denied")
    before = wire.call("repository.info", {"repository_id": repo})
    phase2.restore_v6(stack)
    ddl = (ROOT / "migrations/0005_exploration.sql").read_text().split("INSERT INTO")[0]
    ddl = ddl.replace("CREATE VIEW", "CREATE OR REPLACE VIEW")
    phase2.sql(stack, f"BEGIN; {ddl} DELETE FROM git_catalog.migrations WHERE version = 6; COMMIT;")
    assert phase2.sql(stack, "SELECT max(version) FROM git_catalog.migrations") == "5"
    stack.one_shot("migrate")
    stack.one_shot("migrate")
    assert phase2.sql(stack, "SELECT max(version) FROM git_catalog.migrations") == "7"
    assert wire.call("repository.info", {"repository_id": repo}) == before
    assert phase2.sql(stack, """SELECT has_table_privilege('gh_docs_serve',
        'git_catalog.exploration_generations', 'SELECT'),
        has_table_privilege('gh_docs_serve','git_catalog.exploration_generations','UPDATE'),
        has_table_privilege('gh_docs_serve','git_catalog.source_approvals','SELECT')""") == "t|f|f"
    publish(stack, config, endpoint, "sha1", version=2, seq=1)
    wire.call("git.log", {"context": old}, error="generation_unavailable")


def grep_oracle(stack):
    result = stack.exec("serve", "/usr/local/bin/rg", "--json", "--text",
                       "--fixed-strings", "-e", PHRASE, "-", input=NEW.decode())
    record = next(line["data"] for line in map(json.loads, result.stdout.splitlines()) if line["type"] == "match")
    return [(record["line_number"], record["absolute_offset"] + hit["start"],
             record["absolute_offset"] + hit["end"]) for hit in record["submatches"]]


def search(wire, ctx):
    args = {"context": ctx, "pattern": PHRASE, "literal": True, "context_lines": 1, "page": {"limit": 1}}
    first = wire.call("git.rg", args, request_id="\u0001" * 2000)
    token = first["next_cursor"]
    assert not first["scan_complete"] and token
    second = wire.call("git.rg", {**args, "page": {"limit": 1, "cursor": token}})
    assert second["scan_complete"] and second["next_cursor"] is None
    records = first["matches"] + second["matches"]
    assert [decoded(hit["path"]) for hit in records] == [PATH, b"data/same-\xff"]
    assert records[0]["oid"] == records[1]["oid"]
    expected = grep_oracle(wire.stack)
    for hit in records:
        span = hit["match_range"]
        assert [(hit["line"], span["start"], span["end"])] == expected
        assert decoded(hit["excerpt"]) == NEW
        assert hit["notice"]
    assert first["coverage"]["omitted"]["binary"] == 1
    wire.call("git.rg", {"context": ctx, "pattern": "["}, error="invalid_regex")
    wire.call("git.rg", {"context": ctx, "pattern": "(?=a)"}, error="invalid_regex")
    wire.call("git.rg", {"context": ctx, "pattern": ".{1000000}"}, error="invalid_regex")
    wire.call("git.rg", {**args, "pattern": "different", "page": {"limit": 1, "cursor": token}}, error="cursor_invalid")
    wire.call("git.rg", {**args, "page": {"limit": 1, "cursor": ("A" if token[0] != "A" else "B") + token[1:]}}, error="cursor_invalid")
    signed = urlsafe_b64decode(token + "=" * (-len(token) % 4))
    body = json.loads(signed[:-48])
    body["expiry"] = int(time.time()) - 1
    encoded_body = json.dumps(body, separators=(",", ":")).encode()
    key = Path(wire.stack.root, "serve/cursor.key").read_bytes()
    tag = hmac.digest(key, b"gh-docs-exploration-cursor-v1\0" + encoded_body, "sha384")
    expired = urlsafe_b64encode(encoded_body + tag).rstrip(b"=").decode()
    wire.call("git.rg", {**args, "page": {"limit": 1, "cursor": expired}}, error="cursor_expired")
    strict = wire.call("git.rg", {"context": ctx, "pattern": "資料 MATCH", "ignore_case": True,
                                  "path_glob": encoded(b"data/same-?")})
    assert [decoded(hit["path"]) for hit in strict["matches"]] == [b"data/same-\xff"]
    return token


def history(wire, repo, ctx, proof, git_repo, fmt):
    base = wire.resolve(repo, proof["base"])
    assert git(git_repo, "rev-list", "--count", ctx["commit_oid"]).strip() == b"4"
    result = wire.call("git.log", {"context": ctx})
    assert [entry["oid"] for entry in result["entries"]] == [
        proof["head"], *sorted([proof["left"], proof["right"]]), proof["base"],
    ]
    assert result["entries"][0]["parents"] == [proof["left"], proof["right"]]
    filtered = wire.call("git.log", {"context": ctx, "path": encoded(PATH)})
    assert [entry["oid"] for entry in filtered["entries"]] == [proof["left"], proof["base"]]
    assert "omitted synthetic message" not in json.dumps(result)
    assert decoded(result["entries"][0]["author"]["email"])
    root = wire.call("git.show", {"context": base, "path": encoded(PATH)})
    assert root["parent"] is None and root["entries"][0]["old"] is None
    one = wire.call("git.show", {"context": ctx})
    assert one["parent"] == proof["left"]
    assert any(entry["kind"] == "binary" for entry in one["entries"])
    two = wire.call("git.show", {"context": ctx, "parent": 2, "path": encoded(PATH)})
    assert two["parent"] == proof["right"]
    assert two["entries"][0]["kind"] == "hunk"
    patch = git(git_repo, "diff", "--no-ext-diff", "--no-textconv", proof["base"], proof["head"], "--", PATH)
    assert b"-old\r\n+new\r\n" in patch
    diff = wire.call("git.diff", {"base": base, "head": ctx, "path": encoded(PATH)})
    hunk = diff["entries"][0]["hunk"]
    assert decoded(hunk["old"]["bytes"]) == OLD and decoded(hunk["new"]["bytes"]) == NEW
    assert hunk["changes"] == [{"old": [1, 2], "new": [1, 2]}]
    blame = wire.call("git.blame", {"context": ctx, "path": encoded(PATH), "page": {"limit": 2}})
    last = wire.call("git.blame", {"context": ctx, "path": encoded(PATH),
                                  "page": {"limit": 2, "cursor": blame["next_cursor"]}})
    observed = [entry["origin"]["commit"]["oid"] for entry in blame["entries"] + last["entries"]]
    expected = [line.split()[0].decode() for line in git(git_repo, "blame", "--first-parent", "--root",
                "--line-porcelain", ctx["commit_oid"], "--", PATH).splitlines()
                if len(line.split()) in (3, 4) and len(line.split()[0]) == len(ctx["commit_oid"])
                and all(field.isdigit() for field in line.split()[1:])]
    assert observed == expected == [proof["left"], proof["base"], proof["base"]]
    assert wire.call("git.show", {"context": ctx, "path": encoded(b"data/:(glob)*")})["entries"] == []
    renamed = wire.resolve(repo, proof["renamed"])
    attribution = wire.call("git.blame", {"context": renamed, "path": encoded("data/renamed")})
    assert all(entry["origin"]["commit"]["oid"] == proof["renamed"] for entry in attribution["entries"])
    rename_changes = wire.call("git.diff", {"base": ctx, "head": renamed})["entries"]
    assert [(decoded(change["path"]), change["old"] is None, change["new"] is None) for change in rename_changes] == [
        (b"data/note", False, True), (b"data/renamed", True, False),
    ]
    wire.call("git.show", {"context": ctx, "parent": 99}, error="invalid_argument")
    wire.call("git.diff", {"base": {**base, "generation_id": "999"}, "head": ctx}, error="invalid_argument")
    wire.call("git.log", {"context": {**ctx, "commit_oid": proof["hidden"]}}, error="object_unavailable")
    if fmt == "sha256":
        assert all(decoded(entry["path"]).startswith(b"data/") for entry in one["entries"])
        wire.call("git.blame", {"context": ctx, "path": encoded("private")}, error="object_unavailable")
        assert wire.call("git.rg", {"context": ctx, "pattern": "hidden fixture"})["matches"] == []


def budgets(wire, repo, proof):
    partial = wire.resolve(repo, proof["partial"])
    args = {"context": partial, "pattern": "needle", "literal": True}
    first = wire.call("git.rg", args)
    assert not first["scan_complete"] and not first["matches"] and first["next_cursor"]
    second = wire.call("git.rg", {**args, "page": {"cursor": first["next_cursor"]}})
    assert second["scan_complete"] and [decoded(hit["path"]) for hit in second["matches"]] == [b"corpus/last"]
    oversized = wire.resolve(repo, proof["oversized"])
    skipped = wire.call("git.rg", {"context": oversized, "pattern": "needle"})
    assert not skipped["scan_complete"] and skipped["coverage"]["omitted"]["oversized_blob"] == 1
    assert skipped["matches"] == [] and skipped["next_cursor"] is None
    ctx = wire.resolve(repo)
    escaped = {"context": ctx, "path": encoded("data/wide")}
    chunks = []
    cursor = None
    offset = 0
    for _ in range(10):
        page = wire.call("git.read", {**escaped, "cursor": cursor}, request_id="\u0001" * 2000)
        assert page["returned_range"]["start"] == offset
        offset = page["returned_range"]["end"]
        chunks.append(page["data"])
        cursor = page["next_cursor"]
        if cursor is None:
            break
    assert 1 < len(chunks) < 10 and cursor is None
    assert "".join(chunks) == "\u0001" * 32768
    proof_command = wire.stack.exec("serve", "gh-docs-search-worker", "--probe-denials")
    assert "inherited descriptors and denial controls passed" in proof_command.stdout
    assert wire.stack.exec("serve", "gh-docs-search-worker", "--fail-setup").stdout == "-"

    slow = wire.resolve(repo, proof["slow"])
    request = {"name": "git.rg", "arguments": {"context": slow, "pattern": "z{512}"}}
    clients = []
    pids = []
    try:
        for _ in range(2):
            clients.append(wire.start("tools/call", request))
            command = """for _ in $(seq 1 100); do
              for pid in $(pgrep -f '^/usr/local/bin/gh-docs-search-worker$'); do
                if grep -q '^State:[[:space:]]*T' "/proc/$pid/status"; then continue; fi
                if kill -STOP "$pid" 2>/dev/null; then echo "$pid"; exit; fi
              done
              sleep .02
            done
            exit 1"""
            pids.append(int(wire.stack.exec("serve", "sh", "-ec", command).stdout.strip()))
        assert len(set(pids)) == 2
        wire.call("git.rg", request["arguments"], error="budget_exceeded")
        cancelled_at = time.monotonic()
        clients[0].sock.shutdown(socket.SHUT_RDWR)
        clients[0].close()
        for _ in range(40):
            result = wire.stack.exec("serve", "test", "!", "-e", f"/proc/{pids[0]}", expected=None)
            if result.returncode == 0:
                break
            time.sleep(0.1)
        else:
            raise AssertionError("search worker remained after proxy disconnect")
        print(f"A17 proxy disconnect reaped the held worker in {time.monotonic() - cancelled_at:.3f}s", flush=True)
        response = clients[1].getresponse()
        assert response.status == 200
        timed_out = json.loads(response.read())["result"]
        assert timed_out["isError"] and timed_out["structuredContent"]["code"] == "budget_exceeded"
        wire.stack.exec("serve", "test", "!", "-e", f"/proc/{pids[1]}")
        assert wire.call("git.rg", args)["next_cursor"]
    finally:
        for client in clients:
            client.close()


def retirement(wire, upstream, endpoint, ctx, token):
    stack = wire.stack
    repo = ctx["repository_id"]
    first = int(ctx["generation_id"])
    args = {"context": ctx, "pattern": PHRASE, "literal": True, "context_lines": 1,
            "page": {"limit": 1, "cursor": token}}
    phase3 = phase4.phase3
    parent = ctx["commit_oid"]
    for seq in range(2, 7):
        generation, parent, _, _ = phase3.new_generation(stack, upstream, repo, endpoint,
                                         f"Phase 5 retention {seq}", parent=parent)
        phase4.prepare_publish(stack, repo, generation, seq)
    assert wire.call("git.rg", args)["matches"]
    phase2.sql(stack, f"""UPDATE git_catalog.repository_generations
        SET demoted_at = clock_timestamp() - interval '25 hours'
        WHERE repository_id = '{repo}' AND generation_id = {first}""")
    phase3.command(stack, repo, "--gc", fault="after_retire")
    assert phase3.lifecycle(stack, repo, first) == "retired"
    wire.call("git.rg", args, error="generation_unavailable")
    wire.call("git.log", {"context": ctx}, error="generation_unavailable")
    phase3.command(stack, repo, "--recover")


def sdk(wire, node, client_dir, repo):
    script = client_dir / "phase5-sdk.mjs"
    shutil.copyfile(ROOT / "scripts/phase5/sdk.mjs", script)
    os.chown(script, int(os.environ["SUDO_UID"]), int(os.environ["SUDO_GID"]))
    try:
        with wire.ca.open("rb") as ca:
            env = {**os.environ, "NODE_EXTRA_CA_CERTS": f"/proc/self/fd/{ca.fileno()}"}
            result = phase2.phase0.run([node, script, f"https://127.0.0.1:{wire.port}/mcp", repo],
                env=env, pass_fds=(ca.fileno(),), user=int(os.environ["SUDO_UID"]),
                group=int(os.environ["SUDO_GID"]), extra_groups=[], timeout=25)
            print(result.stdout.strip(), flush=True)
    finally:
        script.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image-tag", required=True)
    parser.add_argument("--node", required=True)
    parser.add_argument("--client-dir", type=Path, required=True)
    args = parser.parse_args()
    print(f"{datetime.now(timezone.utc).isoformat()} Phase 5 Compose qualification", flush=True)
    with phase2.stack(args.image_tag, publication=True, fixture_setup=prepare, serve_target="mcp-serve") as (
            stack, upstream, config, endpoint):
        wire = Wire(stack)
        wire.validate_registry()
        print("A01/A13 normal MCP registry, schemas and proxy boundaries passed", flush=True)
        migration_and_legacy(wire, config, endpoint)
        print("A14 catalog migration, legacy output and policy supersession passed", flush=True)
        publish(stack, config, endpoint, "sha256")
        for fmt, (repo, _) in REPOS.items():
            proof = json.loads((upstream / f"{fmt}-oracle.json").read_text())
            ctx = wire.resolve(repo)
            assert ctx["commit_oid"] == proof["head"]
            token = search(wire, ctx)
            history(wire, repo, ctx, proof, repo_path(upstream, fmt), fmt)
            print(f"A15/A16 {fmt} search and history oracles passed", flush=True)
            if fmt == "sha1":
                budgets(wire, repo, proof)
                print("A17 coverage, wire cap, confinement, overload and teardown passed", flush=True)
                sdk(wire, args.node, args.client_dir, repo)
                retirement(wire, upstream, endpoint, ctx, token)
                print("A13 retained cursor and retirement denial passed", flush=True)
            else:
                stack.compose("run", "--rm", "--no-deps", "migrate", "--revoke-source",
                              "--source", REPOS[fmt][1])
                wire.call("git.rg", {"context": ctx, "pattern": PHRASE, "literal": True, "context_lines": 1,
                          "page": {"limit": 1, "cursor": token}}, error="generation_unavailable")
                wire.call("git.log", {"context": ctx}, error="generation_unavailable")
                print("A14 revoked search cursor and history denied", flush=True)
    print("Phase 5 Compose qualification passed", flush=True)


if __name__ == "__main__":
    main()
