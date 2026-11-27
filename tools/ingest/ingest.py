#!/usr/bin/env python3
"""Load driver for a running convertd container: long load runs and runs on real documents.

Results stay outside the repository: OUT must be a scratch folder, and no
run output, report or document is committed. Real
documents are often confidential.

Standard library only. It sends documents to POST /v1/convert from client
threads, records one JSON line per request, and samples the container every
--sample-ms (the cgroup memory, the OOM kills, each worker's RSS, the
supervisor's RSS and the /tmp use) through one long `docker exec` loop.

Inputs, one or both:
  --dir DIR          every file under DIR (real documents). Files are sent
                     under anonymous names (doc0001.docx and so on); the map
                     from index to path goes to OUT/index.json only.
  --corpus DIR       the generated corpus (corpus/generate.py output) with
  --expected FILE    corpus/expected.json: its cases, checked against their
                     expected status and code.
  --encrypted DIR    extra encrypted files with the password "secret".

Selection of --dir files: all, largest:N, or subset:N (stratified by type
and size, deterministic).

Modes: --passes N sends each task N times in order; --minutes M picks tasks
at random until M minutes have passed. --threads sets the client threads;
with --threads equal to the task count every task starts at once.

Output (OUT): requests.jsonl, samples.jsonl, index.json (private),
run.json (the arguments). tools/ingest/analyze.py reads them.
"""

import argparse
import json
import os
import queue
import random
import re
import subprocess
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

OFFICE_EXT = {"doc", "docx", "docm", "dot", "dotx", "odt", "rtf", "xls", "xlsx",
              "xlsm", "ods", "ppt", "pptx", "pptm", "pps", "ppsx", "odp", "potx"}

# One awk per tick reads every worker's status and the cgroup files; the
# roles of new worker PIDs come from their cmdline once. Every 10th tick adds
# `du` of /tmp/convert (df would report the whole disk of a volume). Each
# tick ends with "E", which the host stamps.
SAMPLER = r'''
roles=" "
n=0
while :; do
  files="/proc/1/status"
  for d in /proc/[0-9]*; do
    p=${d#/proc/}
    [ "$p" = 1 ] && continue
    read -r comm < "$d/comm" 2>/dev/null || continue
    [ "$comm" = convertd ] || continue
    case "$roles" in
      *" $p:"*) ;;
      *) r=$(tr '\0' ' ' < "$d/cmdline" 2>/dev/null); r=${r% }; r=${r##* }
         roles="$roles$p:$r " ;;
    esac
    files="$files $d/status"
  done
  echo "L$roles"
  awk 'FILENAME ~ /status$/ && /^VmRSS/ { split(FILENAME, a, "/"); print "R", a[3], $2 }
       FILENAME ~ /memory.current$/ { print "M", $1 }
       FILENAME ~ /memory.events$/ && $1 == "oom_kill" { print "O", $2 }' \
      $files /sys/fs/cgroup/memory.current /sys/fs/cgroup/memory.events 2>/dev/null
  n=$((n + 1))
  if [ $((n % 10)) = 1 ]; then du -sk /tmp/convert 2>/dev/null | awk '{ print "D", $1 }'; fi
  echo E
  sleep __SLEEP__
done
'''


def parse_args():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", required=True, help="for example http://127.0.0.1:18082")
    ap.add_argument("--key-env", default="CONVERT_API_KEY", help="variable that holds the API key")
    ap.add_argument("--container", required=True)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--dir", type=Path)
    ap.add_argument("--select", default="all")
    ap.add_argument("--corpus", type=Path)
    ap.add_argument("--expected", type=Path)
    ap.add_argument("--encrypted", type=Path)
    ap.add_argument("--outputs", default="pdf,bundle")
    ap.add_argument("--format", default="jpeg")
    ap.add_argument("--dpi", type=int, default=150)
    ap.add_argument("--threads", type=int, default=1)
    ap.add_argument("--passes", type=int, default=1)
    ap.add_argument("--minutes", type=float)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--timeout-seconds", type=int, help="the options' timeout_seconds")
    ap.add_argument("--sample-ms", type=int, default=200)
    ap.add_argument("--settle-seconds", type=float, default=5, help="sampling after the last request")
    return ap.parse_args()


def real_files(root, select):
    files = sorted(p for p in root.rglob("*") if p.is_file())
    if select == "all":
        return files
    kind, _, n = select.partition(":")
    n = int(n)
    if kind == "largest":
        return sorted(files, key=lambda p: p.stat().st_size, reverse=True)[:n]
    if kind == "subset":
        ordered = sorted(files, key=lambda p: (p.suffix.lower(), p.stat().st_size))
        step = len(ordered) / n
        return [ordered[int(i * step + step / 2)] for i in range(n)]
    raise SystemExit(f"unknown --select {select}")


def options_for(output, args):
    opts = {"output": output}
    if output != "pdf":
        opts["images"] = {"format": args.format, "dpi": args.dpi}
    if args.timeout_seconds:
        opts["timeout_seconds"] = args.timeout_seconds
    return opts


def build_tasks(args):
    tasks, index = [], {}
    outputs = [o for o in args.outputs.split(",") if o]
    if args.dir:
        for i, path in enumerate(real_files(args.dir, args.select), 1):
            ext = path.suffix.lower().lstrip(".") or "bin"
            index[i] = str(path)
            for output in outputs:
                tasks.append({"id": f"doc{i:04d}", "path": path, "name": f"doc{i:04d}.{ext}",
                              "ext": ext, "source": "real", "options": options_for(output, args),
                              "statuses": None, "codes": None})
    if args.corpus and args.expected:
        expected = json.loads(args.expected.read_text())
        for f in expected["files"]:
            rel = f["path"].removeprefix("out/")
            for c in f.get("cases", []):
                status = c["expect"].get("status")
                statuses = set(status) if isinstance(status, list) else {status}
                code = c["expect"].get("code")
                codes = None if code is None and statuses != {200} else (
                    {"OK"} if code is None else set(code) if isinstance(code, list) else {code})
                path = args.corpus / rel
                tasks.append({"id": f"{f['id']}/{c['name']}", "path": path, "name": path.name,
                              "ext": path.suffix.lower().lstrip("."), "source": rel.split("/")[0],
                              "options": c.get("options") or {}, "statuses": statuses, "codes": codes})
    if args.encrypted:
        for path in sorted(args.encrypted.iterdir()):
            for opts, statuses, codes in (({"output": "pdf", "password": "secret"}, {200}, {"OK"}),
                                          ({}, {422}, {"PASSWORD_REQUIRED"})):
                tasks.append({"id": f"encrypted/{path.name}/{'pw' if opts else 'none'}", "path": path,
                              "name": path.name, "ext": path.suffix.lower().lstrip("."),
                              "source": "encrypted", "options": opts, "statuses": statuses,
                              "codes": codes})
    missing = [t for t in tasks if not t["path"].is_file()]
    if missing:
        print(json.dumps({"skipped_missing_files": len(missing),
                          "ids": sorted({t["id"] for t in missing if t["source"] != "real"})}))
    return [t for t in tasks if t["path"].is_file()], index


def multipart(options, filename, data):
    boundary = "convertdingest" + str(random.getrandbits(64))
    parts = []
    if options:
        parts.append((f'--{boundary}\r\nContent-Disposition: form-data; name="options"\r\n'
                      f'Content-Type: application/json\r\n\r\n{json.dumps(options)}\r\n').encode())
    parts.append((f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="{filename}"\r\n'
                  f'Content-Type: application/octet-stream\r\n\r\n').encode() + data + b"\r\n")
    parts.append(f"--{boundary}--\r\n".encode())
    return b"".join(parts), f"multipart/form-data; boundary={boundary}"


def timing(header):
    return {m.group(1): float(m.group(2))
            for m in re.finditer(r"([a-z_]+);dur=([0-9.]+)", header or "")}


def send(args, key, task):
    data = task["path"].read_bytes()
    body, ctype = multipart(task["options"], task["name"], data)
    req = urllib.request.Request(args.base + "/v1/convert", data=body, method="POST",
                                 headers={"Authorization": f"Bearer {key}", "Content-Type": ctype})
    t0 = time.time()
    rec = {"t0": round(t0, 3), "id": task["id"], "ext": task["ext"], "source": task["source"],
           "bytes": len(data), "output": task["options"].get("output", "pdf")}
    try:
        with urllib.request.urlopen(req, timeout=600) as resp:
            rec["status"], rec["code"] = resp.status, "OK"
            rec["pages"] = int(resp.headers.get("convert-page-count") or 0) or None
            rec["timing"] = timing(resp.headers.get("Server-Timing"))
            rec["resp_bytes"] = len(resp.read())
    except urllib.error.HTTPError as err:
        rec["status"] = err.code
        rec["timing"] = timing(err.headers.get("Server-Timing"))
        try:
            rec["code"] = json.loads(err.read()).get("code")
        except Exception:
            rec["code"] = "?"
    except Exception as err:
        rec["status"], rec["code"] = 0, f"client:{type(err).__name__}"
    rec["t1"] = round(time.time(), 3)
    rec["wall_ms"] = round((rec["t1"] - t0) * 1000, 1)
    if task["statuses"] is not None:
        rec["expected"] = rec["status"] in task["statuses"] and (
            task["codes"] is None or rec["code"] in task["codes"])
    return rec


def sampler(args, out, stop):
    script = SAMPLER.replace("__SLEEP__", f"{args.sample_ms / 1000:.3f}")
    proc = subprocess.Popen(["docker", "exec", args.container, "sh", "-c", script],
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    roles, cur = {}, {"workers": []}
    for line in proc.stdout:
        kind, _, rest = line.rstrip("\n").partition(" ")
        if line.startswith("L"):
            for pair in line[1:].split():
                pid, _, role = pair.partition(":")
                roles[int(pid)] = role
        elif kind == "R":
            pid, rss = rest.split()
            if pid == "1":
                cur["sup_kb"] = int(rss)
            else:
                cur["workers"].append([int(pid), roles.get(int(pid), "?"), int(rss)])
        elif kind == "M":
            cur["mem"] = int(rest)
        elif kind == "O":
            cur["oom_kill"] = int(rest)
        elif kind == "D":
            cur["tmp_kb"] = int(rest)
        elif kind == "E":
            cur["t"] = round(time.time(), 3)
            out.write(json.dumps(cur) + "\n")
            cur = {"workers": []}
            if stop.is_set():
                break
    proc.terminate()
    proc.wait(timeout=10)


def main():
    args = parse_args()
    key = os.environ[args.key_env]
    args.out.mkdir(parents=True, exist_ok=True)
    tasks, index = build_tasks(args)
    if not tasks:
        raise SystemExit("no tasks")
    (args.out / "index.json").write_text(json.dumps(index, indent=0))
    run = {k: (str(v) if isinstance(v, Path) else v) for k, v in vars(args).items()}
    run.update(tasks=len(tasks), started=time.time())
    (args.out / "run.json").write_text(json.dumps(run, indent=1))

    stop = threading.Event()
    samples = open(args.out / "samples.jsonl", "a", buffering=1)
    sampling = threading.Thread(target=sampler, args=(args, samples, stop), daemon=True)
    sampling.start()
    requests = open(args.out / "requests.jsonl", "a", buffering=1)
    lock = threading.Lock()
    work = queue.Queue()
    if args.minutes is None:
        for _ in range(args.passes):
            for task in tasks:
                work.put(task)
    deadline = time.time() + (args.minutes or 0) * 60
    rng = random.Random(args.seed)

    def client():
        while True:
            if args.minutes is not None:
                if time.time() >= deadline:
                    return
                with lock:
                    task = rng.choice(tasks)
            else:
                try:
                    task = work.get_nowait()
                except queue.Empty:
                    return
            try:
                rec = send(args, key, task)
            except Exception as err:  # a client error must not end the run
                rec = {"t0": round(time.time(), 3), "t1": round(time.time(), 3), "id": task["id"],
                       "ext": task["ext"], "source": task["source"], "bytes": 0,
                       "output": task["options"].get("output", "pdf"), "status": 0,
                       "code": f"client:{type(err).__name__}", "wall_ms": 0}
            with lock:
                requests.write(json.dumps(rec) + "\n")

    started = time.time()
    clients = [threading.Thread(target=client) for _ in range(args.threads)]
    for c in clients:
        c.start()
    for c in clients:
        c.join()
    elapsed = time.time() - started
    time.sleep(args.settle_seconds)
    stop.set()
    sampling.join(timeout=30)
    print(json.dumps({"tasks": len(tasks), "elapsed_s": round(elapsed, 1)}))


if __name__ == "__main__":
    main()
