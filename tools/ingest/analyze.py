#!/usr/bin/env python3
"""Summarise one ingest.py run directory, with anonymous figures only.

Results stay outside the repository: OUT must be a scratch folder, and no
run output, report or document is committed (the owner's decision). The
documents of an ingestion are often confidential.

  analyze.py RUN_DIR [--warmup-min M]

Reads requests.jsonl, samples.jsonl, docker.log, state.json and cell.json
(when present) and prints:

- the requests by type and size class: statuses, pages, the server total
  and the main stages, and each request's worker peak RSS (the highest
  sample of that role between the request's start and end). LibreOffice
  keeps memory between jobs, so the office peak includes what earlier jobs
  left until the worker is recycled; it is left out for PDF inputs;
- throughput (documents per minute) and the latency of the whole run;
- recycles, kills and crashes by reason from the logs, the cgroup OOM
  kills, the memory and /tmp peaks, and the supervisor RSS trend.

It writes RUN_DIR/per_request.jsonl with the peak RSS joined in. Nothing
it prints names a file: rows carry the type, the size class and an index.
"""

import argparse
import collections
import datetime
import json
from pathlib import Path

CLASSES = [(100 << 10, "S <100 KB"), (1 << 20, "M 0.1-1 MB"), (10 << 20, "L 1-10 MB"),
           (float("inf"), "XL >10 MB")]
OFFICE = {"doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "potx"}


def size_class(n):
    return next(name for limit, name in CLASSES if n < limit)


def pct(values, q):
    values = sorted(v for v in values if v is not None)
    if not values:
        return None
    return values[min(len(values) - 1, max(0, int(round(q * len(values) + 0.5)) - 1))]


def fmt(v, digits=0):
    if v is None:
        return "-"
    return f"{v:.{digits}f}"


def load(run):
    reqs = [json.loads(line) for line in open(run / "requests.jsonl")]
    samples = [json.loads(line) for line in open(run / "samples.jsonl")]
    logs = []
    if (run / "docker.log").exists():
        for line in open(run / "docker.log"):
            try:
                logs.append(json.loads(line))
            except ValueError:
                logs.append({"message": "nonjson", "raw": line[:200]})
    cell = json.loads((run / "cell.json").read_text()) if (run / "cell.json").exists() else {}
    state = json.loads((run / "state.json").read_text()) if (run / "state.json").exists() else {}
    return reqs, samples, logs, cell, state


def join_peaks(reqs, samples):
    ts = [s["t"] for s in samples]
    import bisect
    for r in reqs:
        lo = bisect.bisect_left(ts, r["t0"] - 0.25)
        hi = bisect.bisect_right(ts, r["t1"] + 0.25)
        peak = {"office": 0, "render": 0}
        mem = 0
        for s in samples[lo:hi]:
            mem = max(mem, s.get("mem", 0))
            for _, role, rss in s.get("workers", []):
                if role in peak:
                    peak[role] = max(peak[role], rss)
        # LibreOffice keeps what it allocated, so this is the worker's peak
        # in the window, earlier jobs included; PDF inputs never use it.
        office = peak["office"] if r["ext"] in OFFICE else 0
        r["peak_office_mb"] = round(office / 1024) if office else None
        r["peak_render_mb"] = round(peak["render"] / 1024) if peak["render"] else None
        r["peak_mem_mb"] = round(mem / 2**20) if mem else None
        r["class"] = size_class(r["bytes"])


def table(rows, header):
    out = ["| " + " | ".join(header) + " |", "| " + " | ".join("---" for _ in header) + " |"]
    out += ["| " + " | ".join(str(c) for c in row) + " |" for row in rows]
    return "\n".join(out)


def by_group(reqs, key):
    groups = collections.defaultdict(list)
    for r in reqs:
        groups[key(r)].append(r)
    return dict(sorted(groups.items()))


def leave_events(logs):
    c = collections.Counter()
    for e in logs:
        m = e.get("message")
        if m in ("worker recycled", "worker killed", "worker crashed", "worker exited"):
            c[f"{e.get('pool')}:{m.removeprefix('worker ')}:{e.get('reason', '-')}"] += 1
        elif m in ("document poisoned", "stray child reaped"):
            c[m] += 1
    return dict(c)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("run", type=Path)
    ap.add_argument("--warmup-min", type=float, default=0)
    args = ap.parse_args()
    reqs, samples, logs, cell, state = load(args.run)
    join_peaks(reqs, samples)
    with open(args.run / "per_request.jsonl", "w") as f:
        for r in reqs:
            f.write(json.dumps(r) + "\n")

    t_start = min(r["t0"] for r in reqs)
    t_end = max(r["t1"] for r in reqs)
    minutes = (t_end - t_start) / 60
    print(f"## {args.run.name}")
    print(f"cell {cell}")
    print(f"{len(reqs)} requests in {minutes:.1f} min: {len(reqs) / minutes:.1f} requests/min")
    print("statuses:", dict(collections.Counter(f"{r['status']} {r['code']}" for r in reqs).most_common()))
    unexpected = [r for r in reqs if r.get("expected") is False]
    print("unexpected (corpus checks):", len(unexpected),
          dict(collections.Counter(f"{r['id']} -> {r['status']} {r['code']}" for r in unexpected)))
    print("5xx:", sum(1 for r in reqs if r["status"] >= 500), " client errors:",
          sum(1 for r in reqs if r["status"] == 0))
    ok = [r for r in reqs if r["status"] == 200]
    print(f"wall ms of 200s: p50 {fmt(pct([r['wall_ms'] for r in ok], .5))}, "
          f"p99 {fmt(pct([r['wall_ms'] for r in ok], .99))}, max {fmt(max((r['wall_ms'] for r in ok), default=None))}")
    for output in sorted({r["output"] for r in reqs}):
        sub = [r for r in ok if r["output"] == output]
        span = (max(r["t1"] for r in sub) - min(r["t0"] for r in sub)) / 60 if sub else 0
        print(f"  {output}: {len(sub)} ok, wall p50 {fmt(pct([r['wall_ms'] for r in sub], .5))} "
              f"p99 {fmt(pct([r['wall_ms'] for r in sub], .99))}")

    print("\n### by type and size class (all outputs)")
    rows = []
    for (ext, cls, output), g in by_group(reqs, lambda r: (r["ext"], r["class"], r["output"])).items():
        okg = [r for r in g if r["status"] == 200]
        codes = collections.Counter(r["code"] for r in g if r["status"] != 200)
        rows.append([ext, cls, output, len(g), len(okg), ", ".join(f"{k} {v}" for k, v in codes.items()) or "-",
                     fmt(pct([r.get("pages") for r in okg], .5)), fmt(max((r.get("pages") or 0 for r in okg), default=None)),
                     fmt(pct([r["timing"].get("total") for r in okg if r.get("timing")], .5)),
                     fmt(pct([r["timing"].get("total") for r in okg if r.get("timing")], .99)),
                     fmt(pct([r.get("peak_office_mb") for r in g], .5)), fmt(max((r.get("peak_office_mb") or 0 for r in g), default=None)),
                     fmt(pct([r.get("peak_render_mb") for r in g], .5)), fmt(max((r.get("peak_render_mb") or 0 for r in g), default=None))])
    print(table(rows, ["type", "size", "output", "n", "200", "errors", "pages p50", "pages max",
                       "total ms p50", "total ms p99", "office MB p50", "office MB max",
                       "render MB p50", "render MB max"]))

    print("\n### stages of 200s, ms p50 / p99 (server timing)")
    stages = collections.Counter()
    for r in ok:
        stages.update(r.get("timing", {}).keys())
    rows = []
    for output in sorted({r["output"] for r in ok}):
        for stage, _ in stages.most_common():
            vals = [r["timing"][stage] for r in ok if r["output"] == output and stage in r.get("timing", {})]
            if vals and pct(vals, .99) and pct(vals, .99) >= 1:
                rows.append([output, stage, len(vals), fmt(pct(vals, .5), 1), fmt(pct(vals, .99), 1), fmt(max(vals), 1)])
    print(table(rows, ["output", "stage", "n", "p50", "p99", "max"]))

    print("\n### the 10 highest worker peaks (anonymous)")
    top = sorted(reqs, key=lambda r: max(r.get("peak_office_mb") or 0, r.get("peak_render_mb") or 0), reverse=True)[:10]
    print(table([[r["ext"], r["class"], round(r["bytes"] / 2**20, 1), r["output"], r.get("pages"), r["status"], r["code"],
                  r.get("peak_office_mb"), r.get("peak_render_mb"), round(r["wall_ms"])] for r in top],
                ["type", "size", "MB", "output", "pages", "status", "code", "office peak MB", "render peak MB", "wall ms"]))

    print("\n### workers, memory, OOM")
    print("leave events:", leave_events(logs))
    ooms = [s.get("oom_kill") for s in samples if s.get("oom_kill") is not None]
    print("cgroup oom_kill during the run:", (max(ooms) - min(ooms)) if ooms else None, " end value:", ooms[-1] if ooms else None)
    print("container state: OOMKilled", state.get("OOMKilled"), "running", state.get("Running"), "exit", state.get("ExitCode"))
    mems = [s["mem"] for s in samples if "mem" in s]
    tmps = [s["tmp_kb"] for s in samples if "tmp_kb" in s]
    print(f"cgroup memory peak {max(mems) / 2**20:.0f} MiB, /tmp/convert peak {max(tmps or [0]) / 1024:.0f} MiB" if mems else "no samples")
    peaks = collections.defaultdict(int)
    for s in samples:
        for _, role, rss in s.get("workers", []):
            peaks[role] = max(peaks[role], rss)
    print("worker RSS peak by role, MiB:", {k: round(v / 1024) for k, v in peaks.items()})
    sup = [(s["t"], s["sup_kb"]) for s in samples if "sup_kb" in s and (s["t"] - t_start) / 60 >= args.warmup_min]
    if len(sup) > 2:
        n = len(sup)
        mx = sum(t for t, _ in sup) / n
        my = sum(v for _, v in sup) / n
        slope = sum((t - mx) * (v - my) for t, v in sup) / sum((t - mx) ** 2 for t, _ in sup)
        print(f"supervisor RSS after {args.warmup_min} min: {min(v for _, v in sup) / 1024:.1f}-{max(v for _, v in sup) / 1024:.1f} MiB, "
              f"slope {slope * 3600 / 1024:.1f} MiB/h over {n} samples")
    warn = collections.Counter(e.get("message") for e in logs if e.get("level") in ("WARN", "ERROR"))
    print("WARN/ERROR log lines:", dict(warn))


if __name__ == "__main__":
    main()
