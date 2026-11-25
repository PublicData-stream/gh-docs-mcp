"""Measure private MCP and publication capacity on disposable Compose repositories."""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import ctypes
from hashlib import shake_256
import importlib.util
import json
import math
import mmap
import os
from pathlib import Path
import signal
from threading import Event, Lock, Thread
import time
import traceback

spec = importlib.util.spec_from_file_location("phase7_integration", Path(__file__).with_name("integration.py"))
suite = importlib.util.module_from_spec(spec)
spec.loader.exec_module(suite)
phase2, phase3, phase5 = suite.phase2, suite.phase3, suite.phase5


PROFILES = {
    "small": ("sha1", 32, 32),
    "medium": ("sha1", 512, 256),
    "large": ("sha1", 4096, 2048),
    "medium-sha256": ("sha256", 512, 256),
}
MIB = 1024 * 1024
SEED = "gh-docs-phase7-v1"
RATE = 48
WARM_SECONDS = 20


def generate(root, fmt, commits, files):
    phase2.prepare(root)
    repo = phase5.repo_path(root, fmt)
    data = bytearray()

    def blob(mark, content):
        data.extend(f"blob\nmark :{mark}\ndata {len(content)}\n".encode() + content + b"\n")

    blob(1, b"phase seven match\n" * 64)
    for number in range(2, files // 2 + 2):
        seed = f"{SEED}/{number}".encode()
        blob(number, shake_256(seed).hexdigest(4096).encode() + b"\n")

    for number in range(commits):
        data.extend(
            b"commit refs/heads/phase7\n"
            + f"committer Capacity <capacity@example.invalid> {1040000000 + number} +0000\n".encode()
            + b"data 17\nsynthetic commit\n\n"
        )
        if number == 0:
            for index in range(files - 2):
                mark = 1 if index % 2 == 0 else 2 + index // 2
                data.extend(f"M 100644 :{mark} data/{index:04d}\n".encode())
            data.extend(b"M 100644 :1 data/note\n")
        note = b"phase seven match\n" + b"similar history\n" * 64 + f"{number:05d}\n".encode()
        data.extend(f"M 100644 inline history/note\ndata {len(note)}\n".encode() + note + b"\n")
    phase5.git(repo, "fast-import", "--quiet", data=data)
    head = phase5.git(repo, "rev-parse", "refs/heads/phase7").decode().strip()
    phase5.git(repo, "update-ref", "refs/heads/main", head)
    phase5.git(repo, "update-ref", "-d", "refs/tags/v1")
    phase5.git(repo, "update-ref", "-d", "refs/heads/approved/release")


def inventory(repo):
    refs = phase5.git(repo, "rev-list", "--objects", "refs/heads/main").splitlines()
    ids = b"".join(row.split(b" ", 1)[0] + b"\n" for row in refs)
    output = phase5.git(repo, "cat-file", "--batch-check=%(objecttype) %(objectsize)", data=ids)
    sizes = Counter()
    kinds = Counter()
    for line in output.decode().splitlines():
        kind, size = line.split()
        kinds[kind] += 1
        sizes[kind] += int(size)
    return {"objects": dict(kinds), "inflated_bytes": dict(sizes)}


def residency(path):
    with path.open("rb") as stream:
        os.posix_fadvise(stream.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
        with mmap.mmap(stream.fileno(), 0, access=mmap.ACCESS_COPY) as mapping:
            pages = (len(mapping) + mmap.PAGESIZE - 1) // mmap.PAGESIZE
            res = (ctypes.c_ubyte * pages)()
            address = ctypes.addressof(ctypes.c_char.from_buffer(mapping))
            libc = ctypes.CDLL(None, use_errno=True)
            if libc.mincore(ctypes.c_void_p(address), ctypes.c_size_t(len(mapping)), res) != 0:
                raise OSError(ctypes.get_errno(), "mincore failed")
            return sum(value & 1 for value in res), pages


def cgroup(stack, role):
    pid = phase2.phase0.run([*phase2.phase0.DOCKER, "inspect", "--format", "{{.State.Pid}}",
                            stack.ids[role]]).stdout.strip()
    line = next(line for line in Path(f"/proc/{pid}/cgroup").read_text().splitlines() if line.startswith("0::"))
    return Path("/sys/fs/cgroup") / line[3:].lstrip("/")


def snapshot(group):
    result = {}
    for key in ("cpu.stat", "memory.peak", "memory.events", "io.stat", "pids.current", "pids.peak"):
        path = group / key
        if path.exists():
            result[key] = path.read_text().strip()
    return result


@contextmanager
def sample_workers(group):
    stop = Event()
    peak = {"processes": Counter(), "aggregate_rss_kib": 0, "single_process_rss_kib": 0}

    def collect():
        while not stop.is_set():
            count = Counter()
            rss = []
            for pid in (group / "cgroup.procs").read_text().splitlines():
                try:
                    proc = Path("/proc") / pid
                    binary = (proc / "exe").readlink().name
                    status = (proc / "status").read_text().splitlines()
                except OSError:
                    continue
                count[binary] += 1
                rss.extend(int(line.split()[1]) for line in status if line.startswith("VmRSS:"))
            for binary, number in count.items():
                peak["processes"][binary] = max(peak["processes"][binary], number)
            peak["aggregate_rss_kib"] = max(peak["aggregate_rss_kib"], sum(rss))
            peak["single_process_rss_kib"] = max(peak["single_process_rss_kib"], max(rss, default=0))
            stop.wait(0.025)

    worker = Thread(target=collect)
    worker.start()
    try:
        yield peak
    finally:
        stop.set()
        worker.join()


def write(out, entry):
    print(json.dumps(entry, sort_keys=True), file=out, flush=True)


def request(wire, name, args):
    start = time.monotonic_ns()
    body = wire.send("tools/call", {"name": name, "arguments": args})["result"]
    elapsed = (time.monotonic_ns() - start) / 1e6
    assert body["resultType"] == "complete", body
    content = body["structuredContent"]
    if body["isError"]:
        code = content["code"]
        outcome = "rejected" if code == "budget_exceeded" else "failed"
    else:
        code = None
        coverage = content.get("coverage", {})
        partial = (content.get("next_cursor") or not content.get("scan_complete", True)
                   or not coverage.get("complete", True))
        outcome = "partial" if partial else "complete"
    return {"ms": elapsed, "outcome": outcome, "code": code}


def measure(out, wire, name, args, count, parallel, group, labels, cold_packs=()):
    before = snapshot(group)

    next_start = time.monotonic()
    rate_guard = Lock()

    def call(_index):
        nonlocal next_start
        cache = {}
        if cold_packs:
            residency_counts = [residency(path) for path in cold_packs]
            cache = {"resident_pages": sum(value[0] for value in residency_counts),
                     "pack_pages": sum(value[1] for value in residency_counts)}
        else:
            with rate_guard:
                if time.monotonic() >= started + WARM_SECONDS:
                    return None
                delay = next_start - time.monotonic()
                if delay > 0:
                    time.sleep(delay)
                next_start = time.monotonic() + 1 / RATE
        return {**request(wire, name, args), **cache}

    started = time.monotonic()
    with sample_workers(group) as peak, ThreadPoolExecutor(max_workers=parallel) as pool:
        records = [record for record in pool.map(call, range(count)) if record is not None]
    seconds = time.monotonic() - started
    for index, item in enumerate(records):
        write(out, {**labels, "type": "sample", "index": index, **item})
        assert item["outcome"] != "failed", (labels, item)
    timings = sorted(item["ms"] for item in records if item["outcome"] in ("complete", "partial")
                     and (not cold_packs or item["resident_pages"] == 0))
    outcomes = Counter(item["outcome"] for item in records)
    assert timings or cold_packs, (labels, outcomes)
    cuts = {f"p{p}": round(timings[math.ceil(len(timings) * p / 100) - 1], 3)
            for p, minimum in ((50, 1), (95, 20), (99, 100)) if len(timings) >= minimum}
    write(out, {**labels, "type": "summary", "seconds": seconds, "outcomes": outcomes,
                "comparable_samples": len(timings), "latency_ms": cuts,
                "unoffered": count - len(records), "offered_per_second": len(records) / seconds,
                "admitted_per_second": (outcomes["complete"] + outcomes["partial"]) / seconds,
                "complete_per_second": outcomes["complete"] / seconds, "before": before,
                "after": snapshot(group), "sampled_process_peaks": peak})


def compare_batch(out, stack, label, repo, fmt, oid):
    lock = f"/git-store/locks/{repo}/repository.lock"
    # Hold the shared fence and use the production confined reader for both
    # paths. The isolated fixture has already been authorized through MCP.
    command = r"""
    reader() { gh-docs-git-reader canonical "$1" "$2" batch; }
    if [ "$4" = batched ]; then
      for _ in $(seq 1 8); do printf 'contents %s\0' "$3"; done | reader "$1" "$2"
    else
      for _ in $(seq 1 8); do printf 'contents %s\0' "$3" | reader "$1" "$2"; done
    fi
    """
    for iteration in range(5):
        expected = None
        for mode in ("single-object", "batched"):
            start = time.monotonic()
            response = stack.exec("serve", "flock", "-s", lock, "sh", "-ec", command,
                                  "phase7-reader", repo, fmt, oid, mode, timeout=20).stdout
            seconds = time.monotonic() - start
            records = response.split("\0")
            assert records[-1] == "" and len(records) == 17, response[:160]
            headers = records[:-1:2]
            assert sum(header.startswith("+") for header in headers) == (1 if mode == "batched" else 8)
            assert all(header.removeprefix("+").startswith(f"{oid} blob ") for header in headers)
            records[:-1:2] = [header.removeprefix("+") for header in headers]
            assert all(record == "phase seven match\n" * 64 for record in records[1:-1:2])
            framed = "\0".join(records)
            assert expected is None or expected == framed
            expected = framed
            write(out, {"profile": label, "type": "reader-comparison", "mode": mode,
                        "iteration": iteration, "objects": 8, "seconds": seconds})


def sync_parallel(out, stack, upstream, config, endpoint):
    # Use two approved repositories and the same paced local transport.
    phase2.register(stack, config, phase5.policy("sha256"))
    for concurrency in (1, 2):
        for fmt in phase5.REPOS:
            origin = phase5.repo_path(upstream, fmt)
            initial = phase5.git(origin, "rev-parse", "refs/heads/main").decode().strip()
            oid, _, _ = phase3.commit(origin, f"sync capacity {fmt} {concurrency}", parent=initial)
            phase5.git(origin, "update-ref", "refs/heads/main", oid)

        def fetch(repo):
            start = time.monotonic()
            result = phase2.fetch(stack, repo, endpoint,
                                  env=("-e", f"GH_DOCS_SYNC_CONCURRENCY={concurrency}"))
            assert result["outcome"] == "verified", result
            return time.monotonic() - start

        start = time.monotonic()
        peak = 0
        queue_peak = 0
        with ThreadPoolExecutor(max_workers=concurrency) as pool:
            jobs = [pool.submit(fetch, repo) for repo, _ in phase5.REPOS.values()]
            while not all(job.done() for job in jobs):
                running, pending = phase2.sql(stack, """
                    SELECT (SELECT count(*) FROM git_catalog.sync_jobs WHERE state='running'),
                           (SELECT count(*) FROM git_catalog.scheduled_jobs WHERE state='queued')
                """).split("|")
                peak = max(peak, int(running))
                queue_peak = max(queue_peak, int(pending))
                time.sleep(0.25)
            durations = [job.result() for job in jobs]
        assert peak == concurrency, (concurrency, peak)
        write(out, {"type": "sync-concurrency", "concurrency": concurrency, "peak_running": peak,
                    "peak_durable_pending": queue_peak, "jobs_seconds": durations,
                    "seconds": time.monotonic() - start})


def profile(out, tag, label, samples, cold_samples):
    fmt, commits, files = PROFILES[label]
    with phase2.stack(tag, publication=True, serve_target="mcp-serve",
                      fixture_setup=lambda root: generate(root, fmt, commits, files)) as (
            stack, upstream, config, endpoint):
        wire = phase5.Wire(stack)
        wire.validate_registry()
        repo, _ = phase5.REPOS[fmt]
        origin = phase5.repo_path(upstream, fmt)
        stats = inventory(origin)
        assert stats["objects"]["commit"] == commits
        policy = phase5.policy(fmt)
        policy["limits"].update(pack_bytes=64 * MIB, response_bytes=96 * MIB, objects=50_000)
        if label == "large":
            # Leave room for one small update to exercise the source-specific
            # object limit without growing this workload to the global ceiling.
            policy["limits"]["objects"] = sum(stats["objects"].values()) + 3
        phase2.register(stack, config, policy)

        sync_group = cgroup(stack, "sync")

        def stage(name, operation):
            start = time.monotonic()
            before = snapshot(sync_group)
            with sample_workers(sync_group) as peak:
                result = operation()
            write(out, {"profile": label, "type": "publication", "stage": name,
                        "seconds": time.monotonic() - start, "before": before,
                        "after": snapshot(sync_group), "sampled_process_peaks": peak})
            return result

        report = stage("fetch", lambda: phase2.fetch(stack, repo, endpoint))
        generation = int(report["generation"])
        stage("prepare", lambda: phase3.command(stack, repo, "--prepare", generation))
        stage("publish", lambda: phase3.publish(stack, repo, generation, 0))
        context = wire.resolve(repo)
        canonical = stack.root / "store/repositories" / repo
        packs = sorted((canonical / "objects/pack").glob("*.pack"))
        index = [pack.with_suffix(".idx") for pack in packs]
        compressed = sum(p.stat().st_size for p in packs)
        allocated = sum(p.lstat().st_blocks * 512 for p in canonical.rglob("*"))
        write(out, {"profile": label, "type": "fixture", "seed": SEED, "commit": context["commit_oid"],
                    "format": fmt, "tip_files": files, "compressed_pack_bytes": compressed,
                    "allocated_store_bytes": allocated, "allocated_to_pack_ratio": allocated / compressed,
                    "approved_object_budget": policy["limits"]["objects"],
                    **stats})

        operations = {
            "git.read": {"context": context, "path": phase5.encoded(b"data/note")},
            "git.ls_tree": {"context": context, "root": phase5.encoded(b"data"), "page": {"limit": 10}},
            "git.rg": {"context": context, "pattern": "phase seven match", "literal": True, "page": {"limit": 10}},
            "git.log": {"context": context, "page": {"limit": 10}},
        }
        assert wire.call("git.read", operations["git.read"])["data"].startswith("phase seven match")
        assert wire.call("git.rg", operations["git.rg"])["matches"]
        oid = phase5.git(origin, "rev-parse", "refs/heads/main:data/note").decode().strip()
        compare_batch(out, stack, label, repo, fmt, oid)
        group = cgroup(stack, "serve")
        for tool, args in operations.items():
            if cold_samples:
                measure(out, wire, tool, args, cold_samples, 1, group,
                        {"profile": label, "tool": tool, "cache": "pack-eviction", "concurrency": 1, "budget": 10},
                        packs + index)
            for _ in range(3):
                assert request(wire, tool, args)["outcome"] in ("complete", "partial")
            for parallel in (1, 2, 4, 8):
                measure(out, wire, tool, args, samples, parallel, group,
                        {"profile": label, "tool": tool, "cache": "warm", "concurrency": parallel, "budget": 10})
            if "page" in args:
                measure(out, wire, tool, {**args, "page": {"limit": 100}}, samples, 1, group,
                        {"profile": label, "tool": tool, "cache": "warm", "concurrency": 1, "budget": 100})
            if tool in ("git.rg", "git.log"):
                scope = {"root": phase5.encoded(b"data")} if tool == "git.rg" else {
                    "path": phase5.encoded(b"data/note"),
                }
                measure(out, wire, tool, {**args, **scope}, samples, 1, group,
                        {"profile": label, "tool": tool, "cache": "warm", "concurrency": 1,
                         "budget": 10, "scope": "data"})

        next_gen, oid, _, _ = stage("incremental_fetch", lambda: phase3.new_generation(
            stack, upstream, repo, endpoint, "phase7 capacity update", fmt=fmt, parent=context["commit_oid"]))
        stage("prepare_update", lambda: phase3.command(stack, repo, "--prepare", next_gen))
        stage("publish_update", lambda: phase3.publish(stack, repo, next_gen, 1))
        assert wire.resolve(repo)["commit_oid"] == oid
        stage("rollback", lambda: phase3.command(stack, repo, "--rollback", generation, seq=2))
        stage("gc", lambda: phase3.command(stack, repo, "--gc"))
        assert wire.resolve(repo)["commit_oid"] == context["commit_oid"]
        if label == "medium":
            sync_parallel(out, stack, upstream, config, endpoint)
        if label == "large":
            surplus, _, _ = phase3.commit(origin, "above approved object budget", parent=oid)
            phase5.git(origin, "update-ref", "refs/heads/main", surplus)
            stage("object_budget_denial", lambda: phase2.fetch(stack, repo, endpoint, reason={"candidate_invalid"}))
            assert wire.resolve(repo)["commit_oid"] == context["commit_oid"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image-tag", required=True)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--profile", action="append", choices=tuple(PROFILES))
    parser.add_argument("--samples", type=int, default=100)
    parser.add_argument("--cold-samples", type=int, default=20)
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions must be enabled for qualification")
    if not args.out.resolve().is_relative_to("/tmp") or not 1 <= args.samples <= 1000 or not 0 <= args.cold_samples <= 100:
        parser.error("use an output file under /tmp and bounded nonnegative sample counts")
    signal.signal(signal.SIGTERM, phase2.phase0.terminate)
    failures = []
    with args.out.open("x") as out:
        for selected in args.profile or PROFILES:
            try:
                profile(out, args.image_tag, selected, args.samples, args.cold_samples)
            except Exception as error:
                write(out, {"profile": selected, "type": "profile-failure", "error": type(error).__name__})
                traceback.print_exc()
                failures.append(selected)
    if failures:
        raise SystemExit("Capacity profiles failed: " + ", ".join(failures))


if __name__ == "__main__":
    main()
