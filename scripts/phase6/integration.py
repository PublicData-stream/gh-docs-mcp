"""Qualify the authenticated scheduler and outbox on disposable rootful Compose."""

import argparse
import base64
from datetime import datetime, timezone
import hmac
import http.client
import importlib.util
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts/phase5"))
spec = importlib.util.spec_from_file_location("phase5_integration", ROOT / "scripts/phase5/integration.py")
previous = importlib.util.module_from_spec(spec)
spec.loader.exec_module(previous)
phase2 = previous.phase2
REPOS = previous.REPOS
VERSION = "GITMCP-HMAC-SHA384"


def wait(check, *, seconds=45):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        if value := check():
            return value
        time.sleep(0.25)
    raise AssertionError("timed out waiting for the expected state")


def b64(raw):
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def mac(key, lines, body):
    return b64(hmac.digest(key, ("\n".join(lines) + "\n\n").encode() + body, "sha384"))


class Link:
    def __init__(self, stack, sender="feed", *, key=None, key_id="v1"):
        self.stack = stack
        self.sender = sender
        if sender == "feed":
            prefix, target, self.port = "feed-sync", "SYNC_CONTROL", 8783
            self.receiver = "sync"
        else:
            prefix, target, self.port = "sync-serve", "SERVE_NOTIFY", 8784
            self.receiver = "serve"
        self.address = stack.env[f"{target}_IP"]
        self.key = key or (stack.root / sender / f"{prefix}.key").read_bytes()
        self.key_id = key_id
        self.epoch = "bootstrap"

    def build(self, path, body, *, nonce=None, timestamp=None, epoch=None, audience=None):
        nonce = nonce or b64(secrets.token_bytes(32))
        timestamp = str(int(time.time()) if timestamp is None else timestamp)
        epoch = epoch or self.epoch
        caller, audience = f"gh-docs-{self.sender}", audience or f"gh-docs-{self.receiver}"
        length = str(len(body))
        tag = mac(self.key, [f"{VERSION}-V1", self.key_id, caller, audience, epoch, timestamp,
                             nonce, "POST", path, "application/json", length], body)
        fields = {
            "Host": f"{self.address}:{self.port}", "Connection": "close", "Content-Type": "application/json",
            "Content-Length": length, "X-Git-MCP-Key-Id": self.key_id, "X-Git-MCP-Caller": caller,
            "X-Git-MCP-Audience": audience, "X-Git-MCP-Epoch": epoch, "X-Git-MCP-Timestamp": timestamp,
            "X-Git-MCP-Nonce": nonce, "X-Git-MCP-Signature": tag,
        }
        head = f"POST {path} HTTP/1.1\r\n" + "".join(f"{key}: {value}\r\n" for key, value in fields.items())
        return head.encode() + b"\r\n" + body, nonce

    def raw(self, packet, *, role=None, address=None, port=None):
        command = [*self.stack.compose_command, "exec", "-T", role or self.sender, "nc", "-w", "2",
                   address or self.address, str(port or self.port)]
        result = subprocess.run(command, input=packet, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                env=self.stack.context, timeout=12, check=False)
        return result.stdout

    def exchange(self, path, body, *, status=200, **options):
        packet, nonce = self.build(path, body, **options)
        reply = self.raw(packet)
        if status is None:
            assert not reply, reply[:80]
            return None
        head, response = reply.split(b"\r\n\r\n", 1)
        status_line, *headers = head.decode().split("\r\n")
        assert int(status_line.split()[1]) == status, status_line
        if status != 200:
            return None
        headers = dict(line.split(": ", 1) for line in headers)
        receiver, caller = f"gh-docs-{self.receiver}", f"gh-docs-{self.sender}"
        assert headers["X-Git-MCP-Request-Nonce"] == nonce
        assert headers["X-Git-MCP-Receiver"] == receiver
        assert headers["Content-Length"] == str(len(response))
        expected = mac(self.key, [f"{VERSION}-RESPONSE-V1", self.key_id, receiver, caller,
                                  nonce, "200", "application/json", str(len(response))], response)
        assert hmac.compare_digest(headers["X-Git-MCP-Response-Signature"], expected)
        return json.loads(response)

    def identify(self):
        self.epoch = self.exchange("/internal/v1/identify", b"{}", epoch="bootstrap")["epoch"]
        return self.epoch

    def hint(self, event, *, status=200):
        result = self.exchange("/internal/v1/sync-requested", json.dumps(event).encode(), status=status)
        if status == 200:
            assert result == {"event_id": event["event_id"], "outcome": "accepted"}, result

    def notify(self, event, *, outcome, status=200):
        reply = self.exchange("/internal/v1/generation-committed", json.dumps(event).encode(), status=status)
        if status == 200:
            assert reply == {"event_id": event["event_id"], "outcome": outcome}, reply


def sequence(stack, repo):
    return int(phase2.sql(stack, f"SELECT coalesce((SELECT publication_sequence FROM git_catalog.active_generations "
                                 f"WHERE repository_id = '{repo}'), 0)"))


def event_at(stack, repo, seq):
    raw = phase2.sql(stack, f"SELECT event_id, generation_id, publication_sequence FROM git_catalog.notification_outbox "
                            f"WHERE repository_id = '{repo}' AND publication_sequence = {seq}")
    event, generation, seq = raw.split("|")
    return {"event_id": event, "repository_id": repo, "generation_id": generation, "publication_sequence": seq}


def delivered(stack, event):
    return phase2.sql(stack, f"SELECT delivered_at IS NOT NULL FROM git_catalog.notification_outbox "
                             f"WHERE event_id = '{event['event_id']}'") == "t"


def new_hint(repo, token=None):
    return {"event_id": str(uuid.uuid7()), "repository_id": repo,
            "observation_digest": token or secrets.token_hex(32), "observed_at": int(time.time()), "reason": "feed_refs"}


def fault(stack, role, point=""):
    (stack.root / role / "fault").write_text(point)


def exited(stack, role):
    return phase2.phase0.run([*phase2.phase0.DOCKER, "wait", stack.ids[role]], timeout=55).stdout.strip()


def ready(stack, role):
    wait(lambda: stack.exec(role, "wget", "-qO", "/dev/null", "http://127.0.0.1:8081/healthz",
                            expected=None, timeout=5).returncode == 0, seconds=20)


def operator(stack, command, repo, *args, **kwargs):
    return stack.exec("sync", "gh-docs-sync", command, "--repository", repo, *args, **kwargs)


def advance(upstream, label):
    repo = previous.repo_path(upstream, "sha1")
    parent = previous.git(repo, "rev-parse", "refs/heads/main").strip().decode()
    tree = previous.git(repo, "rev-parse", f"{parent}^{{tree}}").strip().decode()
    oid = previous.git(repo, "commit-tree", tree, "-p", parent, "-m", label).strip().decode()
    previous.git(repo, "update-ref", "refs/heads/main", oid)
    return oid


def migration(stack, config, endpoint, repo):
    phase2.register(stack, config, previous.policy("sha1"))
    report = phase2.fetch(stack, repo, endpoint)
    previous.phase4.prepare_publish(stack, repo, report["generation"], 0)
    assert sequence(stack, repo) == 1
    stack.compose("stop", "sync")
    first = event_at(stack, repo, 1)
    phase2.restore_v6(stack)
    assert phase2.sql(stack, "SELECT max(version) FROM git_catalog.migrations") == "6"
    stack.one_shot("migrate")
    stack.one_shot("migrate")
    assert phase2.sql(stack, "SELECT max(version) FROM git_catalog.migrations") == "7"
    assert sequence(stack, repo) == 1 and event_at(stack, repo, 1) == first
    assert phase2.sql(stack, """SELECT
        has_table_privilege('gh_docs_serve','git_catalog.notification_outbox','SELECT'),
        has_table_privilege('gh_docs_serve','git_catalog.notification_outbox','UPDATE'),
        has_table_privilege('gh_docs_serve','git_catalog.scheduled_jobs','SELECT'),
        has_function_privilege('gh_docs_serve','git_catalog.enqueue_hint(uuid,uuid,text,bigint)','EXECUTE'),
        has_function_privilege('gh_docs_serve','git_catalog.observe_notification(uuid,uuid,bigint,bigint)','EXECUTE'),
        has_table_privilege('gh_docs_sync','git_catalog.notification_outbox','UPDATE')""") == "f|f|f|f|t|f"
    print("A19 Phase 6 migration preserved the v6 publication and grants", flush=True)
    return first


def rotation(stack):
    directory = stack.root / "serve"
    active = directory / "sync-serve.key"
    previous_key = directory / "sync-serve.previous.key"
    old, new = active.read_bytes(), secrets.token_bytes(48)
    active.write_bytes(new)
    previous_key.write_bytes(old)
    previous_key.chmod(0o400)
    os.chown(previous_key, 10001, 10001)
    expires = int(time.time()) + 15
    stack.context.update(GH_DOCS_SYNC_SERVE_KEY_ID="v2", GH_DOCS_SYNC_SERVE_PREVIOUS_KEY_ID="v1",
                         GH_DOCS_SYNC_SERVE_PREVIOUS_KEY_FILE="/run/secrets/sync-serve.previous.key",
                         GH_DOCS_SYNC_SERVE_PREVIOUS_KEY_EXPIRES=str(expires))
    stack.compose("up", "--no-build", "--pull", "never", "--force-recreate", "--no-deps", "serve")
    ready(stack, "serve")
    old_sender = Link(stack, "sync", key=old, key_id="v1")
    new_sender = Link(stack, "sync", key=new, key_id="v2")
    assert old_sender.identify() == new_sender.identify()
    time.sleep(max(0, expires - time.time()) + 0.25)
    old_sender.exchange("/internal/v1/identify", b"{}", status=401)
    new_sender.identify()
    active.write_bytes(old)
    previous_key.unlink()
    for suffix in ("KEY_ID", "PREVIOUS_KEY_ID", "PREVIOUS_KEY_FILE", "PREVIOUS_KEY_EXPIRES"):
        stack.context.pop("GH_DOCS_SYNC_SERVE_" + suffix)
    stack.compose("up", "--no-build", "--pull", "never", "--force-recreate", "--no-deps", "serve")
    print("A18 receiver-first rotation and previous-key expiry passed", flush=True)


def protocol(stack, link):
    path = "/internal/v1/identify"
    body = b"{}"
    packet, _ = link.build(path, body, epoch="bootstrap")
    assert link.raw(packet).startswith(b"HTTP/1.1 200 ")
    assert link.raw(packet).startswith(b"HTTP/1.1 409 ")
    assert link.raw(packet.replace(b"Content-Length: 2", b"Content-Length: 2\r\nContent-Length: 2")).startswith(b"HTTP/1.1 400 ")
    assert link.raw(packet.replace(b"Content-Length: 2", b"Content-Length: 02")).startswith(b"HTTP/1.1 400 ")
    assert link.raw(packet.replace(b"Content-Length: 2", b"Transfer-Encoding: chunked\r\nContent-Length: 2")).startswith(b"HTTP/1.1 400 ")
    assert link.raw(packet.replace(b"Connection: close", b"Connection: close\r\nx-git-mcp-key-id: v1")).startswith(b"HTTP/1.1 400 ")
    assert link.raw(packet.replace(b"POST ", b"GET ", 1)).startswith(b"HTTP/1.1 400 ")
    assert link.raw(packet.replace(b"identify ", b"identify?encoded=1 ", 1)).startswith(b"HTTP/1.1 400 ")
    link.exchange(path, body, status=401, audience="gh-docs-serve")
    link.exchange(path, body, status=401, timestamp=int(time.time()) - 61)
    bad = Link(stack, key=secrets.token_bytes(48))
    bad.exchange(path, body, status=401)
    packet, _ = link.build(path, body, epoch="bootstrap")
    assert link.raw(packet[:-2] + b"[]").startswith(b"HTTP/1.1 401 ")
    assert not link.raw(packet, role="serve")
    assert not link.raw(packet, address=stack.env["SYNC_WRITE_IP"])
    stack.exec("feed", "sh", "-ec",
               "test ! -e /git-store && test ! -e /git-sync-private && test ! -e /run/secrets/db-password "
               "&& test ! -e /run/secrets/sync-serve.key && test ! -e /run/secrets/cursor.key")
    assert not link.raw(b"", address=stack.env["PG_READ_IP"], port=5432)
    client = http.client.HTTPSConnection("127.0.0.1", int(stack.env["PROXY_PORT"]),
                                         context=previous.Wire(stack).ssl, timeout=5)
    try:
        client.request("POST", path, body)
        assert client.getresponse().status == 404
    finally:
        client.close()
    print("A18 raw framing, signed acknowledgments and private routing passed", flush=True)


def run(stack, upstream, config, endpoint):
    sha1 = REPOS["sha1"][0]
    sha256 = REPOS["sha256"][0]
    first = migration(stack, config, endpoint, sha1)
    phase2.register(stack, config, previous.policy("sha256"))
    (stack.root / "sync/roster.json").write_text(json.dumps([sha1, sha256]))
    stack.compose("start", "sync")
    wait(lambda: sequence(stack, sha256) == 1, seconds=60)
    wait(lambda: delivered(stack, first))
    wait(lambda: phase2.sql(stack, "SELECT count(*) FROM git_catalog.scheduled_jobs WHERE state <> 'finished'") == "0")
    second_owner = stack.exec("sync", "gh-docs-sync", expected=None)
    assert second_owner.returncode and "scheduler is already owned" in second_owner.stdout, second_owner.stdout

    link = Link(stack)
    link.identify()
    protocol(stack, link)
    link.hint(new_hint(str(uuid.uuid4())), status=409)
    link.hint(new_hint(sha1) | {"observed_at": int(time.time()) - 86401}, status=409)
    advance(upstream, "hint before acknowledgment")
    hint = new_hint(sha1)
    fault(stack, "sync", "after_hint_commit")
    link.hint(hint, status=None)
    assert exited(stack, "sync") == "86"
    assert phase2.sql(stack, f"SELECT count(*) FROM git_catalog.hint_receipts WHERE event_id = '{hint['event_id']}'") == "1"
    fault(stack, "sync", "after_fetch_commit")
    stack.compose("start", "sync")
    ready(stack, "sync")
    link.hint(hint, status=409)  # stale boot epoch
    link.identify()
    link.hint(hint)
    link.hint(hint | {"observation_digest": secrets.token_hex(32)}, status=409)
    assert exited(stack, "sync") == "86"
    assert sequence(stack, sha1) == 1
    fault(stack, "sync", "after_publish")
    stack.compose("start", "sync")
    assert exited(stack, "sync") in ("137", "128")
    assert sequence(stack, sha1) == 2
    second = event_at(stack, sha1, 2)
    assert not delivered(stack, second)
    fault(stack, "serve", "after_notification_observed")
    fault(stack, "sync")
    stack.compose("start", "sync")
    assert exited(stack, "serve") == "86"
    assert not delivered(stack, second)
    fault(stack, "serve")
    stack.compose("start", "serve")
    wait(lambda: delivered(stack, second))
    print("A19 hint, fetch, publication and delivery interruption boundaries passed", flush=True)

    notify = Link(stack, "sync")
    notify.identify()
    notify.notify(second, outcome="observed")
    notify.notify(first, outcome="superseded")
    notify.notify(second | {"publication_sequence": "3"}, outcome="", status=409)
    notify.notify(second | {"generation_id": "00"}, outcome="", status=400)
    wire = previous.Wire(stack)
    wire.validate_registry()

    # Keep MCP live while its independent receiver cannot authenticate sync.
    key_path = stack.root / "serve/sync-serve.key"
    original_key = key_path.read_bytes()
    key_path.write_bytes(secrets.token_bytes(48))
    stack.compose("restart", "serve")
    ready(stack, "serve")
    operator(stack, "--rollback", sha1, "--generation", "0", "--expect-sequence", "2")
    third = event_at(stack, sha1, 3)
    assert not delivered(stack, third)
    assert wire.resolve(sha1)["generation_id"] == "0"
    assert json.loads(operator(stack, "--schedule-status", sha1).stdout)["paused"]
    assert operator(stack, "--resume", sha1, "--expect-sequence", "2", expected=None).returncode
    operator(stack, "--resume", sha1, "--expect-sequence", "3")
    wait(lambda: phase2.sql(stack, f"SELECT outcome FROM git_catalog.scheduled_jobs "
             f"WHERE repository_id = '{sha1}' AND reason = 'resume' ORDER BY job_id DESC LIMIT 1") == "no_change")
    assert sequence(stack, sha1) == 3
    key_path.write_bytes(original_key)
    stack.compose("restart", "serve")
    wait(lambda: delivered(stack, third))
    print("A19 rollback pause, explicit resume, stale delivery and MCP independence passed", flush=True)

    (upstream / "mode").write_text("delay")
    fourth_oid = advance(upstream, "new verified snapshot after rollback")
    link.identify()
    link.hint(new_hint(sha1))
    wait(lambda: phase2.sql(stack, f"SELECT count(*) FROM git_catalog.sync_jobs "
             f"WHERE repository_id = '{sha1}' AND state = 'running'") == "1")
    for _ in range(4):
        link.hint(new_hint(sha1))
    assert phase2.sql(stack, f"SELECT count(*) FROM git_catalog.scheduled_jobs "
                            f"WHERE repository_id = '{sha1}' AND state <> 'finished'") == "2"
    assert phase2.sql(stack, "SELECT pg_terminate_backend(pid) FROM pg_locks WHERE locktype = 'advisory' "
                            "AND classid = '4294967295'::oid AND objid = '4294967293'::oid AND granted") == "t"
    assert exited(stack, "sync") == "1"
    assert sequence(stack, sha1) == 3
    (upstream / "mode").write_text("ok")
    stack.compose("start", "sync")
    wait(lambda: sequence(stack, sha1) == 4, seconds=85)
    assert wire.resolve(sha1)["commit_oid"] == fourth_oid
    wait(lambda: phase2.sql(stack, "SELECT count(*) FROM git_catalog.scheduled_jobs WHERE state <> 'finished'") == "0")
    assert sequence(stack, sha1) == 4

    (upstream / "mode").write_text("unavailable")
    link.identify()
    link.hint(new_hint(sha1))
    wait(lambda: phase2.sql(stack, f"SELECT count(*) FROM git_catalog.scheduled_jobs WHERE repository_id = '{sha1}' "
             "AND state = 'active' AND outcome = 'upstream_unavailable' AND attempts = 2 "
             "AND next_attempt_at > clock_timestamp()") == "1")
    (upstream / "mode").write_text("ok")
    wait(lambda: phase2.sql(stack, f"SELECT count(*) FROM git_catalog.scheduled_jobs WHERE repository_id = '{sha1}' "
             "AND state <> 'finished'") == "0")
    assert sequence(stack, sha1) == 4
    print("A19 transient fetch failure persisted a bounded retry and recovered", flush=True)

    feed_oid = advance(upstream, "actual bounded feed discovery")
    (stack.root / "feed/roster.json").write_text(json.dumps([previous.policy("sha1")]))
    stack.compose("restart", "feed")
    wait(lambda: sequence(stack, sha1) == 5, seconds=80)
    fifth = event_at(stack, sha1, 5)
    wait(lambda: delivered(stack, fifth))
    final = wire.resolve(sha1)
    assert final["commit_oid"] == feed_oid
    assert wire.call("git.read", {"context": final, "path": previous.encoded(previous.PATH)})["data"].encode() == previous.NEW
    print("A19 dual ownership, bounded coalescing, interrupted fetch recovery and real feed publication passed", flush=True)
    rotation(stack)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image-tag", required=True)
    args = parser.parse_args()
    print(f"{datetime.now(timezone.utc).isoformat()} Phase 6 Compose qualification", flush=True)
    with phase2.stack(args.image_tag, fixture_setup=previous.prepare, control=True) as (stack, upstream, config, endpoint):
        try:
            run(stack, upstream, config, endpoint)
        except BaseException:
            print(stack.compose("logs", "--no-color", "--tail", "40", "sync", "serve", "feed", "postgres",
                                expected=None).stdout, flush=True)
            raise


if __name__ == "__main__":
    main()
