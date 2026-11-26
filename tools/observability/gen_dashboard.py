"""Generates deploy/observability/grafana-dashboard.json (ledger item 010).

Usage: python3 -I tools/observability/gen_dashboard.py deploy/observability/grafana-dashboard.json

Edit this file, not the JSON, then run it. tests/observability.rs checks every
metric name in the output against the /metrics of a running convertd.
"""
import json
import sys

S = 'namespace=~"$namespace", pod=~"$pod"'
# The family filter, for the metrics with a family label. A request refused before
# detection (429, 503 at admission) has family "unknown".
SF = S + ', family=~"$family"'
RI = "$__rate_interval"
DS = {"type": "prometheus", "uid": "${datasource}"}

panels = []
next_id = [1]
y = [0]


def pid():
    i = next_id[0]
    next_id[0] += 1
    return i


def row(title):
    panels.append({
        "type": "row", "id": pid(), "title": title, "collapsed": False,
        "gridPos": {"h": 1, "w": 24, "x": 0, "y": y[0]}, "panels": [],
    })
    y[0] += 1


def target(expr, legend, ref):
    # interval is the Min step: $__rate_interval is then at least 4 x $scrape_interval,
    # whatever scrape interval the shared data source has.
    return {"datasource": DS, "expr": expr, "legendFormat": legend, "refId": ref, "range": True,
            "interval": "$scrape_interval"}


def ts(title, desc, targets, unit, x, w, h=8, stack=False, bars=False, minv=0):
    custom = {
        "drawStyle": "bars" if bars else "line",
        "lineWidth": 1,
        "fillOpacity": 30 if (stack or bars) else 10,
        "showPoints": "never",
        "spanNulls": False,
        "stacking": {"mode": "normal" if stack else "none", "group": "A"},
    }
    p = {
        "type": "timeseries", "id": pid(), "title": title, "description": desc,
        "datasource": DS,
        "gridPos": {"h": h, "w": w, "x": x, "y": y[0]},
        "fieldConfig": {"defaults": {"unit": unit, "min": minv, "custom": custom}, "overrides": []},
        "options": {
            "legend": {"displayMode": "table", "placement": "bottom", "calcs": ["max", "mean"], "showLegend": True},
            "tooltip": {"mode": "multi", "sort": "desc"},
        },
        "targets": [target(e, l, chr(ord("A") + i)) for i, (e, l) in enumerate(targets)],
    }
    if minv is None:
        del p["fieldConfig"]["defaults"]["min"]
    panels.append(p)
    return p


def stat(title, desc, expr, unit, x, w, h=4, thresholds=None, decimals=None):
    steps = thresholds or [{"color": "green", "value": None}]
    p = {
        "type": "stat", "id": pid(), "title": title, "description": desc, "datasource": DS,
        "gridPos": {"h": h, "w": w, "x": x, "y": y[0]},
        "fieldConfig": {"defaults": {"unit": unit, "thresholds": {"mode": "absolute", "steps": steps}}, "overrides": []},
        "options": {"reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False},
                    "colorMode": "value", "graphMode": "none", "textMode": "auto", "justifyMode": "auto"},
        # An instant query: a pod that is no longer scraped shows no data, not its last value.
        "targets": [dict(target(expr, "", "A"), instant=True, range=False)],
    }
    if decimals is not None:
        p["fieldConfig"]["defaults"]["decimals"] = decimals
    panels.append(p)


def text(title, content, x, w, h):
    panels.append({
        "type": "text", "id": pid(), "title": title,
        "gridPos": {"h": h, "w": w, "x": x, "y": y[0]},
        "options": {"mode": "markdown", "content": content},
    })


def q(quant, metric, by, extra=""):
    sel = S + (", " + extra if extra else "")
    return f'histogram_quantile({quant}, sum by (le, {by}) (rate({metric}_bucket{{{sel}}}[{RI}])))'


# Status per code, from the table in src/rest/response.rs (the metric has no status label).
STATUS = [
    ("200", "OK"),
    ("400", "BAD_REQUEST"),
    ("413", "FILE_TOO_LARGE"),
    ("415", "UNSUPPORTED_FORMAT"),
    ("422", "VALIDATION_ERROR|PASSWORD_REQUIRED|INVALID_PASSWORD|LIMIT_EXCEEDED|CONVERSION_FAILED"),
    ("429", "TOO_MANY_REQUESTS"),
    ("500", "INTERNAL_ERROR"),
    ("503", "SERVICE_UNAVAILABLE"),
    ("504", "CONVERSION_TIMEOUT"),
]
ALL_CODES = "|".join(c for _, c in STATUS)

REQ = "convert_requests_total"
STAGE = "convert_stage_duration_seconds"
# Stages in request order. engine, total and overhead are sums of other stages.
PARTS = "receive|detect|queue|preflight|convert|inspect|render|respond"

# ---- Requests
row("Requests")
stat("Requests / s", "POST /v1/convert outcomes that the handler records. A 401 from the auth layer and a failure to create the job directory are not counted.",
     f"sum(rate({REQ}{{{SF}}}[{RI}]))", "reqps", 0, 4)
stat("Error share", "Share of requests with a code other than OK.",
     f'(sum(rate({REQ}{{{SF}, code!="OK"}}[{RI}])) or vector(0)) / sum(rate({REQ}{{{SF}}}[{RI}]))', "percentunit", 4, 4,
     thresholds=[{"color": "green", "value": None}, {"color": "orange", "value": 0.05}, {"color": "red", "value": 0.2}])
stat("429 share (all families)", "Share of requests refused with 429 TOO_MANY_REQUESTS: the queue, the queue wait or MAX_CONCURRENT_REQUESTS. "
     "It ignores the family filter, because a refusal before detection has family \"unknown\".",
     f'(sum(rate({REQ}{{{S}, code="TOO_MANY_REQUESTS"}}[{RI}])) or vector(0)) / sum(rate({REQ}{{{S}}}[{RI}]))', "percentunit", 8, 4,
     thresholds=[{"color": "green", "value": None}, {"color": "orange", "value": 0.01}, {"color": "red", "value": 0.05}])
stat("Conversions in flight (estimate)",
     "The rate of the summed receive and total stage times (Little's law): from the start of the handler to the response head. "
     "The download of the body is not counted, and neither is an upload that fails (it records no stage). Each request adds its time when it finishes, "
     "so under steady load this is the mean number of conversions in progress. A conversion longer than the rate window counts in the window where it ends.",
     f'sum(rate({STAGE}_sum{{{SF}, stage=~"receive|total"}}[{RI}]))', "short", 12, 4, decimals=2)
stat("Busy workers (mean)",
     "Mean of the busy worker count of all selected pods and pools together, over the rate window, sampled every $scrape_interval. A worker is busy from its lease to its return.",
     f'avg_over_time(sum(convert_workers{{{S}, state="busy"}})[{RI}:$scrape_interval])', "short", 16, 4, decimals=2)
stat("Disk budget used", "Highest share of TMP_MAX_BYTES in use on one pod, at the end of the time range.",
     f'max(convert_disk_budget_bytes{{{S}, state="used"}} / ignoring(state) (convert_disk_budget_bytes{{{S}, state="max"}} > 0))',
     "percentunit", 20, 4,
     thresholds=[{"color": "green", "value": None}, {"color": "orange", "value": 0.75}, {"color": "red", "value": 0.9}])
y[0] += 4

ts("Requests by HTTP status",
   "Outcomes of POST /v1/convert. convert_requests_total has a code label, not a status label: each series maps codes to the status from src/rest/response.rs. "
   "A 401 from the auth layer, a 404 for an unknown route and a failure to create the job directory are not counted. \"other\" catches a code that this map does not know.",
   [(f'sum(rate({REQ}{{{SF}, code=~"{codes}"}}[{RI}]))', status) for status, codes in STATUS]
   + [(f'sum(rate({REQ}{{{SF}, code!~"{ALL_CODES}"}}[{RI}]))', "other")],
   "reqps", 0, 12, stack=True)
ts("Errors by code", "Every request with a code other than OK, by error code.",
   [(f'sum by (code) (rate({REQ}{{{SF}, code!="OK"}}[{RI}]))', "{{code}}")], "reqps", 12, 6, stack=True)
ts("Errors since the process started",
   "The counter itself, per pod and code. A series starts at its first request with the value it has at the first scrape, "
   "so a rate does not show a first burst that ends between two scrapes; this panel does.",
   [(f'sum by (namespace, pod, code) ({REQ}{{{SF}, code!="OK"}})', "{{code}} {{namespace}}/{{pod}}")], "short", 18, 6)
y[0] += 8
ts("Successful requests by output and family", "200 responses by requested output and detected input family.",
   [(f'sum by (output, family) (rate({REQ}{{{SF}, code="OK"}}[{RI}]))', "{{output}} {{family}}")], "reqps", 0, 12, stack=True)
ts("Responses cut after the head, since the process started",
   "200 responses that convertd cut: RESPONSE_STALLED, RESPONSE_TOO_SLOW and RESPONSE_ABORTED. convert_requests_total counted them as OK at the head. "
   "A client that closes the connection itself is not counted. The counter starts again when the convertd process restarts. "
   "The counter itself, per pod: its series starts at the first cut with the value 1, so a rate would not show an isolated cut.",
   [(f'sum by (namespace, pod, code) (convert_responses_cut_total{{{S}}})', "{{code}} {{namespace}}/{{pod}}")], "short", 12, 12)
y[0] += 8

# ---- Latency per stage
row("Latency per stage")
ts("p50 per stage", "Median duration of each stage of a request (Server-Timing stages). Short rate windows keep bursts visible.",
   [(q(0.5, STAGE, "stage", f'stage=~"{PARTS}|engine|total", family=~"$family"'), "{{stage}}")], "s", 0, 12)
ts("p99 per stage", "99th percentile of each stage. A histogram quantile is an estimate inside its bucket.",
   [(q(0.99, STAGE, "stage", f'stage=~"{PARTS}|engine|total", family=~"$family"'), "{{stage}}")], "s", 12, 12)
y[0] += 8
ts("Time in each stage (seconds per second)",
   "Rate of the summed time per stage, counted when each request finishes. Under steady load it estimates the requests in that stage at once. "
   "The stages overlap (an office preflight is inside convert, and receive is outside total), so the series do not add up and the panel does not stack them.",
   [(f'sum by (stage) (rate({STAGE}_sum{{{S}, stage=~"{PARTS}|engine|total", family=~"$family"}}[{RI}]))', "{{stage}}")],
   "short", 0, 12)
ts("Total and overhead by family, p50 and p99",
   "The total stage (from the end of the upload to the response head; the upload and the body download are outside it) and the overhead (total minus queue minus engine time) for each input family.",
   [(q(0.5, STAGE, "family", f'stage="total", family=~"$family"'), "p50 total {{family}}"),
    (q(0.99, STAGE, "family", f'stage="total", family=~"$family"'), "p99 total {{family}}"),
    (q(0.5, STAGE, "family", f'stage="overhead", family=~"$family"'), "p50 overhead {{family}}"),
    (q(0.99, STAGE, "family", f'stage="overhead", family=~"$family"'), "p99 overhead {{family}}")],
   "s", 12, 12)
y[0] += 8
ts("Engine overhead per pool and op, p50 and p99",
   "Wall time of an engine call minus its queue and engine time: IPC, spawn and lease costs, per pool and operation.",
   [(q(0.5, "convert_engine_overhead_seconds", "pool, op"), "p50 {{pool}} {{op}}"),
    (q(0.99, "convert_engine_overhead_seconds", "pool, op"), "p99 {{pool}} {{op}}")], "s", 0, 12)
ts("Input size, p50 and p99", "Upload size per input family.",
   [(q(0.5, "convert_input_bytes", "family", 'family=~"$family"'), "p50 {{family}}"),
    (q(0.99, "convert_input_bytes", "family", 'family=~"$family"'), "p99 {{family}}")], "bytes", 12, 12)
y[0] += 8

# ---- Pools
row("Pools")
ts("Queue depth per pool (max over the trailing rate window)",
   "Leases that wait for a worker. Each point is the maximum of the scraped samples in the trailing $__rate_interval (at least four scrapes), "
   "so a short queue stays visible when you zoom out, and one high sample shows in several points. A pool that never waited shows 0. "
   "The depth also counts the later engine calls of admitted requests. Only first calls count against QUEUE_CAPACITY (office: 2 x OFFICE_WORKERS) or 2 x RENDER_WORKERS, past which a request gets 429.",
   [(f'max by (namespace, pod, pool) (max_over_time(convert_queue_depth{{{S}}}[{RI}])) or 0 * max by (namespace, pod, pool) (convert_workers{{{S}}})', "{{pool}} {{namespace}}/{{pod}}")], "short", 0, 12)
ts("Workers per pool and state", "Workers that are idle, busy or starting, summed over the selected pods.",
   [(f'sum by (pool, state) (convert_workers{{{S}}})', "{{pool}} {{state}}")], "short", 12, 12, stack=True)
y[0] += 8
ts("Busy workers per pool and pod (max over the trailing rate window)",
   "Each point is the highest busy count of a pod over its scraped samples in the trailing $__rate_interval, so one high sample shows in several points.",
   [(f'max by (namespace, pod, pool) (max_over_time(convert_workers{{{S}, state="busy"}}[{RI}]))', "{{pool}} {{namespace}}/{{pod}}")], "short", 0, 8)
ts("Worker restarts by reason",
   "Workers that left the pool, by pool and reason: jobs, rss, age, timeout, crash, cancelled, error, start_failed. "
   "No series exists until the first restart.",
   [(f'sum by (pool, reason) (increase(convert_worker_restarts_total{{{S}}}[{RI}]))', "{{pool}} {{reason}}")],
   "short", 8, 8, bars=True)
ts("Worker restarts since the process started",
   "The counter itself. Its series starts at the first restart with the value 1, so increase() shows no isolated first restart; this panel does.",
   [(f'sum by (namespace, pod, pool, reason) (convert_worker_restarts_total{{{S}}})', "{{pool}} {{reason}} {{namespace}}/{{pod}}")],
   "short", 16, 8)
y[0] += 8

# ---- Supervisor and disk
row("Supervisor and disk")
ts("Supervisor RSS and peak", "Resident memory of the convertd serve process (VmRSS and VmHWM). Linux only.",
   [(f'max by (namespace, pod) (convert_supervisor_rss_bytes{{{S}}})', "rss {{namespace}}/{{pod}}"),
    (f'max by (namespace, pod) (convert_supervisor_rss_peak_bytes{{{S}}})', "peak {{namespace}}/{{pod}}")], "bytes", 0, 8)
ts("Supervisor threads and open descriptors", "Threads and open file descriptors of the supervisor. Linux only.",
   [(f'max by (namespace, pod) (convert_supervisor_threads{{{S}}})', "threads {{namespace}}/{{pod}}"),
    (f'max by (namespace, pod) (convert_supervisor_open_fds{{{S}}})', "fds {{namespace}}/{{pod}}")], "short", 8, 8)
ts("Disk budget used versus the limit", "Bytes reserved under TMP_DIR against TMP_MAX_BYTES. A failed reservation gives 503.",
   [(f'max by (namespace, pod) (convert_disk_budget_bytes{{{S}, state="used"}})', "used {{namespace}}/{{pod}}"),
    (f'max by (namespace, pod) (convert_disk_budget_bytes{{{S}, state="max"}})', "limit {{namespace}}/{{pod}}")], "bytes", 16, 8)
y[0] += 8

# ---- Quality
row("Quality")
ts("Warnings by code", "Manifest warnings per second, such as FONT_SUBSTITUTED or RENDER_CLAMPED.",
   [(f'sum by (code) (rate(convert_warnings_total{{{S}}}[{RI}]))', "{{code}}")], "short", 0, 6, stack=True)
ts("Warnings since the process started",
   "The counter itself, per pod. A code's series starts at its first warning with the value 1, so a rate does not show an isolated warning; this panel does.",
   [(f'sum by (namespace, pod, code) (convert_warnings_total{{{S}}})', "{{code}} {{namespace}}/{{pod}}")], "short", 6, 6)
ts("Skipped checks by check and reason", "Verification checks that did not run. blank_page counts pages.",
   [(f'sum by (check, reason) (rate(convert_verify_skipped_total{{{S}}}[{RI}]))', "{{check}} {{reason}}")],
   "short", 12, 6, stack=True)
ts("Pages rendered by format", "Image pages rendered per second.",
   [(f'sum by (format) (rate(convert_pages_rendered_total{{{S}}}[{RI}]))', "{{format}}")], "short", 18, 6, stack=True)
y[0] += 8

# ---- CPU per job (effort O2: convert_worker_cpu_seconds{pool,op}, user plus system time per worker call)
CPU = "convert_worker_cpu_seconds"
CPU_NOTE = ("A call adds all of its CPU time when its reply arrives, so a call longer than the rate window shows as idle while it runs "
            "and above its real cores when it ends. A call that times out, crashes or passes its kill RSS sends no reply and records nothing.")
row("CPU per job")
ts("Cores per pool",
   "CPU seconds per second of the finished worker calls: the cores each pool used. " + CPU_NOTE + " The 5 m series smooths long calls.",
   [(f'sum by (pool) (rate({CPU}_sum{{{S}}}[{RI}]))', "{{pool}}"),
    (f'sum by (pool) (rate({CPU}_sum{{{S}}}[5m]))', "{{pool}} (5 m)")],
   "short", 0, 8)
ts("CPU per call, p50 and p99",
   "CPU time (user plus system) of one worker call, per pool and operation. A conversion makes one or more calls (convert, inspect, render). " + CPU_NOTE,
   [(q(0.5, CPU, "pool, op"), "p50 {{pool}} {{op}}"),
    (q(0.99, CPU, "pool, op"), "p99 {{pool}} {{op}}")],
   "s", 8, 8)
ts("Mean CPU per call",
   "The rate of the CPU time over the rate of the calls, per pool and operation: the mean CPU seconds of one call in the window.",
   [(f'sum by (pool, op) (rate({CPU}_sum{{{S}}}[{RI}])) / sum by (pool, op) (rate({CPU}_count{{{S}}}[{RI}]))', "{{pool}} {{op}}")],
   "s", 16, 8)
y[0] += 8


def var_query(name, label, query, multi=True):
    return {
        "name": name, "label": label, "type": "query", "datasource": DS,
        "query": {"query": query, "refId": f"{name}-var", "qryType": 1},
        "definition": query, "refresh": 2, "sort": 1,
        "includeAll": True, "allValue": ".*", "multi": multi,
        "current": {"selected": True, "text": ["All"], "value": ["$__all"]},
        "options": [], "hide": 0, "regex": "",
    }


dashboard = {
    "__inputs": [],
    "__requires": [
        {"type": "grafana", "id": "grafana", "name": "Grafana", "version": "11.0.0"},
        {"type": "datasource", "id": "prometheus", "name": "Prometheus", "version": "1.0.0"},
    ],
    "uid": "convertd-overview",
    "title": "convertd",
    "description": "convertd load per request, per stage and per pool, from the /metrics families in src/observability/metrics.rs.",
    "tags": ["convertd", "rs-convert"],
    "editable": True,
    "graphTooltip": 1,
    "refresh": "30s",
    "time": {"from": "now-3h", "to": "now"},
    "timepicker": {},
    "timezone": "browser",
    "schemaVersion": 39,
    "version": 1,
    "annotations": {"list": [{
        "builtIn": 1, "datasource": {"type": "grafana", "uid": "-- Grafana --"}, "enable": True, "hide": True,
        "iconColor": "rgba(0, 211, 255, 1)", "name": "Annotations & Alerts", "type": "dashboard",
    }]},
    "links": [],
    "templating": {"list": [
        {"name": "datasource", "label": "Prometheus", "type": "datasource", "query": "prometheus",
         "current": {}, "hide": 0, "includeAll": False, "multi": False, "options": [], "refresh": 1, "regex": ""},
        {"name": "scrape_interval", "label": "Scrape interval", "type": "textbox",
         "description": "The scrape interval of convertd, as a Prometheus duration such as 15s or 5m. Every query uses it as its min step, so $__rate_interval covers at least four scrapes.",
         "query": "15s", "hide": 0,
         "current": {"selected": False, "text": "15s", "value": "15s"},
         "options": [{"selected": True, "text": "15s", "value": "15s"}]},
        var_query("namespace", "Namespace", "label_values(convert_disk_budget_bytes, namespace)"),
        var_query("pod", "Pod", 'label_values(convert_disk_budget_bytes{namespace=~"$namespace"}, pod)'),
        var_query("family", "Input family", 'label_values(convert_requests_total{namespace=~"$namespace", pod=~"$pod"}, family)'),
    ]},
    "panels": panels,
}

with open(sys.argv[1], "w") as f:
    json.dump(dashboard, f, indent=2)
    f.write("\n")
