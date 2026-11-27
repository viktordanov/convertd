# deploy

`deploy/` holds the Kubernetes files that run the rs-convert image as an internal service: a kustomize base and a gVisor overlay.

The image runs on any container platform with the settings that these files give: a read-only root, a writable `/tmp`, the probes on `/readyz` and `/healthz`, and a termination grace longer than `SHUTDOWN_GRACE_SECONDS`. The repository keeps only the Kubernetes manifests.

Know two facts before you use these files. First, the `runtime` image is proved to convert documents under Docker on arm64 and amd64 ([docker/README.md](../docker/README.md#what-is-verified)). Second, no file here has run against a live cluster. The files render and pass a schema check offline, and [What is verified](#what-is-verified) lists what that proves.

## Contents

1. [Pod](#pod)
2. [Disk budget](#disk-budget)
3. [Memory budget](#memory-budget)
4. [Deployment](#deployment)
5. [Shutdown and rollout](#shutdown-and-rollout)
6. [Worker isolation](#worker-isolation)
7. [Observability](#observability)
8. [What is verified](#what-is-verified)

<!-- memoria:section id="contract" files="k8s/base/deployment.yaml k8s/base/service.yaml k8s/base/networkpolicy.yaml k8s/overlays/gvisor/kustomization.yaml" -->
## Pod

The base runs one container with 2 CPU and 4 GiB, `OFFICE_WORKERS=2`, `RENDER_WORKERS=2` and the API key from a Secret. Its settings:

| Topic | Value in `k8s/base/deployment.yaml` |
| --- | --- |
| Startup probe | `GET /readyz` every 2 s, 60 failures: 120 s |
| Readiness probe | `GET /readyz` every 5 s, 2 failures |
| Liveness probe | `GET /healthz` every 10 s, timeout 3 s, 3 failures |
| Termination grace | 230 s in total (5 s `preStop` sleep + 210 + 15) |
| Temp volume on `/tmp` | `emptyDir` with `sizeLimit: 4Gi` on node disk |
| `TMP_MAX_BYTES` | 3221225472 (3 GiB) |
| `OFFICE_KILL_RSS_MB`, `OFFICE_MAX_RSS_MB` | 1152, 896 (MiB) |
| `RENDER_KILL_RSS_MB`, `RENDER_MAX_RSS_MB` | 320, 256 (MiB) |
| `TIMEOUT_MAX_SECONDS`, `TIMEOUT_DEFAULT_SECONDS` | 210, 120 |
| CPU limit | none, requests only |
| Network | ClusterIP Service on port 80. A NetworkPolicy denies all egress, DNS included |
| Security | non-root 10001, read-only root, no capabilities, `RuntimeDefault` seccomp |
| Scaling | 1 replica, no autoscaler |
| gVisor | optional overlay `k8s/overlays/gvisor` |

convertd refuses to start if `TIMEOUT_MAX_SECONDS` is more than 210, or less than `TIMEOUT_DEFAULT_SECONDS`. Kustomize cannot compute a minimum. If an overlay lowers `TIMEOUT_MAX_SECONDS` below 120, lower `TIMEOUT_DEFAULT_SECONDS` in the same overlay.
<!-- /memoria:section -->

<!-- memoria:section id="disk-budget" files="k8s/base/deployment.yaml" -->
## Disk budget

convertd reserves space for each upload and job output under `TMP_DIR` (`/tmp/convert`) within the disk budget, `TMP_MAX_BYTES`. If a reservation fails, the request gets status 503, and the pod stays up. The budget does not count the worker slots, the LibreOffice profiles, partial writes and the container logs. These use the headroom of the volume.

`TMP_MAX_BYTES` must stay at least 1 GiB below the size of the `/tmp` volume. The volume is 4 GiB and the budget is 3 GiB. If the volume fills, the kubelet evicts the pod and every job on it fails. Change `TMP_MAX_BYTES`, `sizeLimit` and both `ephemeral-storage` values together. The ephemeral-storage request equals the limit, so the scheduler reserves the whole volume.
<!-- /memoria:section -->

<!-- memoria:section id="memory-budget" files="k8s/base/deployment.yaml" -->
## Memory budget

Each worker can grow to its pool's kill limit before the watchdog kills it, and convertd has no limit for all workers together. `OFFICE_KILL_RSS_MB` and `RENDER_KILL_RSS_MB` set the kill limit of each pool. A pool without its own value takes `WORKER_KILL_RSS_MB`. When workers are due for a job count or an age, the manager replaces them one at a time per pool. So at most one replacement, with about 256 MiB after its warm-up, waits beside the office workers. A worker that died is replaced as soon as its process is gone, which needs no extra memory. A killed worker counts against the one overlap until its exit is seen, because it holds its memory until then. A replacement can start its first job while the kernel still frees its old worker's memory, for a few milliseconds after the kill. That old worker is below its `MAX_RSS_MB`, and the replacement is still at its warm size, so the pair stays within the budget. The memory limit of the container must hold all of this:

```text
OFFICE_WORKERS × OFFICE_KILL_RSS_MB + 256 MiB + RENDER_WORKERS × RENDER_KILL_RSS_MB + 512 MiB ≤ limits.memory
2 × 1152 MiB + 256 MiB + 2 × 320 MiB + 512 MiB = 3712 MiB ≤ 4096 MiB
```

The rule holds one replacement for each pool, not one for each worker. With 2 office and 2 render workers in 4 GiB, the rule leaves 384 MiB free. That is the 256 MiB that one overlap per pool saves, and 128 MiB that the old rule already left unused. It lets `OFFICE_KILL_RSS_MB` rise to 1344 MiB: 2 × 1344 + 256 + 2 × 320 + 512 = 4096 MiB. The base manifest keeps 1152 MiB until a soak measures the higher limit. A render replacement is small, about 8 MiB when it is ready, and the 512 MiB covers it.

The 512 MiB holds the supervisor, the page cache and a margin. On cgroup v2 with Kubernetes 1.28 or later, a container that passes its memory limit loses every process. Then every job on the pod fails. This is not verified on the cluster of the owner. Change the worker counts, the RSS values and the memory limit together. `OFFICE_MAX_RSS_MB` and `RENDER_MAX_RSS_MB` recycle a worker after a job that leaves it above that size.

`/tmp` here is an `emptyDir` on node disk. An `emptyDir` with `medium: Memory`, or a tmpfs, adds its use to the memory limit. Then add `TMP_MAX_BYTES` to the left side.

### Sizing for 2 CPUs and 4 GiB

The values come from an ingestion of 276 real documents on 2026-11-26: 128 DOCX, 93 PPTX, 30 PDF, 23 XLSX and 2 PPT, of up to 43 MB. It ran in arm64 Docker with 2 CPUs, 3584 MiB, no swap and `/tmp` on disk. The figures:

- LibreOffice is the bottleneck and needs the memory. One office worker needed 462 MiB at p50, 1546 MiB at p99, and up to 3281 MiB for heavy decks of 84 to 127 slides.
- PDFium needed at most 279 MiB, at 150 and 300 DPI.
- The second office worker raised the throughput by 50 to 61 %. A second render worker added 12 to 20 %.
- Two concurrent requests filled the two CPUs. Four doubled the p99 and gave no more throughput.
- With the watchdog on, no ingestion run had an OOM kill. A worker over its limit was killed, and its request got 422 `LIMIT_EXCEEDED`.

Pick by the documents:

| Profile | Settings | Trade-off |
| --- | --- | --- |
| Throughput (the base) | `OFFICE_WORKERS=2`, `OFFICE_KILL_RSS_MB=1152`, `OFFICE_MAX_RSS_MB=896`, `RENDER_WORKERS=2`, `RENDER_KILL_RSS_MB=320`, `RENDER_MAX_RSS_MB=256` | About 57 to 74 documents a minute on a mixed set. Documents that need more than 1152 MiB get 422. In the test set that was between 4 % (at 1536 MiB) and 13 % (at 768 MiB) of the files. |
| Coverage | `OFFICE_WORKERS=1`, `OFFICE_KILL_RSS_MB=2688`, `OFFICE_MAX_RSS_MB=1024`, `RENDER_WORKERS=2`, `RENDER_KILL_RSS_MB=320`, `RENDER_MAX_RSS_MB=256` | About 30 documents a minute. Only the decks over 2688 MiB get 422: 3 of the 276 test files. 1 × 2688 + 256 + 2 × 320 + 512 = 4096 MiB. |

Both profiles are projections from the measured peaks. The throughput figures come from cells with one shared kill limit, and no cell ran with these per-pool values. With one office worker, a heavy deck holds it for 20 to 55 s, so the next request waits `QUEUE_WAIT_SECONDS` (10 s) and gets 429. Clients must retry 429.

The watchdog samples each worker's RSS every 200 ms, so a fast burst of memory can pass the kill limit before a sample sees it. Then the kernel's OOM killer is the real backstop. convertd reads the kill from the cgroup and answers 422 `LIMIT_EXCEEDED`, as for a watchdog kill. A 35-minute soak on 2026-11-27 measured this. It ran the same 276 documents with 2 clients in 3584 MiB. The settings were 1 office worker (`OFFICE_KILL_RSS_MB=2176`, `OFFICE_MAX_RSS_MB=1024`) and 2 render workers (320 and 256 MiB). The kernel killed the office worker 3 times in 311 office jobs. 9 of the 484 requests got 422 `LIMIT_EXCEEDED`, these 3 included. For fewer kernel kills, lower `OFFICE_KILL_RSS_MB` or raise the memory limit.
<!-- /memoria:section -->

<!-- memoria:section id="kubernetes" files="k8s/base/kustomization.yaml k8s/base/deployment.yaml k8s/overlays/gvisor/kustomization.yaml k8s/overlays/gvisor/runtimeclass.yaml" -->
## Deployment

The base names its image `rs-convert`. Your own overlay, outside this repository, sets the real image. Deploy into a namespace that you choose:

1. Create the key once: `kubectl create secret generic convertd-api-key --from-literal=API_KEY='<key>'`.
2. Write an overlay with `resources: [<path>/deploy/k8s/base]` and `images: [{name: rs-convert, newName: <registry>/rs-convert, newTag: v<x.y.z>}]`.
3. Make sure that the overlay renders: `kubectl kustomize <overlay>`.
4. Apply it: `kubectl apply -k <overlay>`.
5. Wait for the pod: `kubectl rollout status deployment/convertd`.

To rotate the key, add the old key to the same Secret as `API_KEY_PREVIOUS`. Set the new key as `API_KEY`. Then roll the pods. The pod also starts without `API_KEY_PREVIOUS`, because that Secret key is optional.

The NetworkPolicy allows ingress on port 8080 from every source and denies all egress. An overlay can narrow the ingress with `from:` selectors. Its `podSelector` names the convertd pods explicitly, because an empty selector cuts egress for the whole namespace. The policy has an effect only if the CNI of the cluster enforces NetworkPolicy. The base has no Ingress object, and a proxy in front of the Service must allow requests longer than 240 s.

The `gvisor` overlay adds `runtimeClassName: gvisor` to the pod. To use it, point your overlay at `k8s/overlays/gvisor` instead of the base. The cluster needs a RuntimeClass named `gvisor`. If it has none, apply `k8s/overlays/gvisor/runtimeclass.yaml` once, with a `handler` that matches the runsc runtime of your nodes. The overlay does not include that file, because the class is cluster-wide. Its `overhead` is empty until the sandbox memory is measured (plan 003 step 10).

The base leaves out a HorizontalPodAutoscaler, because the useful scale signal is concurrency, not CPU. It leaves out a PodDisruptionBudget, because with one replica it blocks node drains.
<!-- /memoria:section -->

<!-- memoria:section id="lifecycle" files="k8s/base/deployment.yaml" -->
## Shutdown and rollout

convertd gets `SIGTERM` when Kubernetes stops the pod. The manifest sets `SHUTDOWN_GRACE_SECONDS=210`, the same as `TIMEOUT_MAX_SECONDS`, so a job with the longest permitted timeout can finish. With the convertd default of 30 s, each rollout stops every conversion that runs longer than 30 s. After `SIGTERM`, Kubernetes waits 15 s more than `SHUTDOWN_GRACE_SECONDS`, so it does not kill the process first.

The `preStop` sleep of 5 s comes first, so the Service removes the endpoint before `SIGTERM`. The native `sleep` action needs Kubernetes 1.30 or later. The rollout uses `maxSurge: 1` and `maxUnavailable: 0`. The new pod is ready before the old one stops, so a long grace delays only the exit of the old pod.
<!-- /memoria:section -->

<!-- memoria:section id="isolation" files="k8s/base/networkpolicy.yaml k8s/base/deployment.yaml k8s/overlays/gvisor/kustomization.yaml" -->
## Worker isolation

The supervisor and all office and render workers run in one container, as one user, UID 10001. They share the file system, `/tmp` included, and the network namespace. Thus a worker that a hostile document controls can read the job directories of other requests. It can also connect to everything that the pod can reach. Isolation between the workers, and between a worker and the supervisor, is item 006, the kernel sandbox. The owner deferred it, so the image runs with `SANDBOX=off`.

The service needs no outbound connection, not even DNS. The deployment must block egress, because the image does not:

- **Kubernetes:** the NetworkPolicy denies all egress, DNS included. It has an effect only if the CNI of the cluster enforces NetworkPolicy. The optional `gvisor` overlay also puts the whole pod behind a user-space kernel.
- **Plain Docker:** a container with a published port is on a network with egress. Block its egress with a host firewall, or put it on an internal network behind a proxy that you control.

The pod security settings limit what a worker can do on the node: a non-root user, a read-only root, no capabilities, no privilege escalation, `RuntimeDefault` seccomp and no service-account token. They do not separate the workers from each other.
<!-- /memoria:section -->

<!-- memoria:section id="observability" files="observability/grafana-dashboard.json observability/prometheusrule.yaml" -->
## Observability

convertd serves Prometheus metrics on `GET /metrics` on the `http` port, without the API key. `deploy/observability/` holds two examples that read them:

- **`grafana-dashboard.json`**: a Grafana dashboard, made by `tools/observability/gen_dashboard.py`. It shows the `/v1/convert` requests by status and error code, and p50 and p99 per stage. It also shows the queue, workers, restarts and CPU cores of each pool, the CPU time per call, supervisor memory, the disk budget and the warnings.
- **`prometheusrule.yaml`**: a `PrometheusRule` for the Prometheus Operator with four alerts. They fire on 429, on 422 `LIMIT_EXCEEDED`, on a queue at its capacity and on a disk budget that is 90 % reserved. A comment above each alert gives the reason for its threshold.

The scrape configuration belongs to the deployment, not to this repository. With the Prometheus Operator, a `ServiceMonitor` like this one selects the Service of the base:

```yaml
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: convertd
  labels:
    release: kube-prometheus-stack   # must match the serviceMonitorSelector of your Prometheus
spec:
  selector:
    matchLabels:
      app.kubernetes.io/name: convertd
  endpoints:
    - port: http
      path: /metrics
      interval: 15s
```

To use the examples:

1. Apply the `ServiceMonitor` in the namespace of convertd.
2. In Grafana, open **Dashboards**, then **New**, then **Import**. Upload `grafana-dashboard.json`.
3. Open the dashboard and select the Prometheus data source of convertd in the **Prometheus** list.
4. If your scrape interval is not 15 s, type it in the **Scrape interval** box.
5. Adapt `prometheusrule.yaml` with the list that follows. Then apply it in the same namespace.

Adapt the rules to the deployment:

- Set the `release` label to the value that the `ruleSelector` of your Prometheus selects.
- If an overlay changes `OFFICE_WORKERS`, `RENDER_WORKERS` or `QUEUE_CAPACITY`, change the queue thresholds. If `QUEUE_CAPACITY` is 0, remove the office branch of the queue alert.
- If deployments with other capacities share one Prometheus, add a `namespace` matcher to each queue selector.
- If your scrape interval is longer than 1 minute, widen the 5 m and 15 m rate windows to at least four scrape intervals.

If Prometheus runs in another namespace, its `serviceMonitorNamespaceSelector` and `ruleNamespaceSelector` must select the namespace of convertd. Otherwise the Operator does not load the `ServiceMonitor` or the rules.

### Dashboard

To change the dashboard, edit `tools/observability/gen_dashboard.py`, not the JSON. Then run `python3 -I tools/observability/gen_dashboard.py deploy/observability/grafana-dashboard.json` and commit both files.

The dashboard computes rates over `$__rate_interval`, which is at least four scrape intervals. Each query takes its minimum step from the `scrape_interval` variable, and Grafana uses that step as the scrape interval. With 15 s, a rate window can be as short as 1 minute. The dashboard tells processes apart by their `namespace` and `pod` labels, which a `ServiceMonitor` adds. A scrape without them merges the counters of all processes.

The "Conversions in flight" and "Time in each stage" panels estimate the concurrency from the stage time sums. Thus a burst between two scrapes still counts. Each request adds its time when it finishes, so the estimate is exact only under steady load. An upload that fails records no stage time. A stage and family series starts at its first request, so the first burst of a new pod or family has no rate.

The "CPU per job" row reads `convert_worker_cpu_seconds`, the user and system CPU time of each worker call. "Cores per pool" is the rate of its sum, the cores that each pool used. Each call adds all of its CPU time when its reply arrives. Thus a call longer than the rate window shows as idle while it runs, and above its real cores when it ends. The panel also shows a series over the `cpu_window` variable, 5 m by default, which smooths long calls. Keep `cpu_window` at least four scrape intervals. The series averages that rate over each graph step, so a step longer than `cpu_window` skips no call. A "CPU seconds since the process started" panel shows the first call of a new pod or operation, which a rate misses. A call that times out, crashes or passes its kill RSS records nothing.

### Alerts

convertd does not export the queue capacity, so the queue alert has a fixed threshold of 4 for each pool. The queue gauge also counts the later engine calls of admitted requests, which the capacity does not limit. Thus the queue alert shows queue pressure, and the 429 alert shows the refusals. The 429 and 422 alerts also need 5 such responses in their window, so one refusal on an idle pod does not fire them.

### Name check

`tests/observability.rs` keeps the files and the code in sync. It starts convertd at level B, sends requests until the metric families exist, and reads `/metrics`. It compares two kinds of name with the samples of the scrape: each `convert_*` name in the files, and each metric that a query selects. A missing name fails the test, and so does a misspelled prefix such as `covert_workers`. The test runs in the CI test job, with `test-hooks`.

Two names are exceptions. `tests/convert_pdf.rs` makes a stalled response and asserts `convert_responses_cut_total` on its own scrape. On a host that is not Linux, the test looks for the supervisor gauges in `src/observability/metrics.rs`. The test also limits the form of the rules file: each `expr` is a block or a plain value on its own `expr:` line.

The test checks metric names only. It does not check label values, a Grafana import or a load by the Prometheus Operator.
<!-- /memoria:section -->

<!-- memoria:section id="verification" files="k8s/base/deployment.yaml k8s/overlays/gvisor/kustomization.yaml k8s/overlays/gvisor/runtimeclass.yaml" -->
## What is verified

Plan 003 step 8 (Kubernetes) and step 10 (gVisor) do the live checks, on the cluster of the owner. Until then, these checks have run:

- **Render:** `kubectl kustomize` (v1.33.9) renders the base and the `gvisor` overlay.
- **Schema:** `kubeconform -strict` (v0.8.0) passed for all 7 resources on 2026-11-26, after the base got `TIMEOUT_DEFAULT_SECONDS`.
- **Observability examples:** on 2026-11-26, by hand, the dashboard JSON and the rules YAML parsed, and all 50 queries parsed with the PromQL parser of Prometheus v0.54.1. The 6 CPU queries that came later did not run through that parser. No Grafana import, `promtool` check or Prometheus Operator load has run. Two Grafana behaviors are not checked live: a query Min step replaces the scrape interval of the data source in `$__rate_interval`, and Min step accepts the `$scrape_interval` variable.
- **Not done:** a live deployment and a gVisor run. Also not checked are the native `preStop` sleep on the Kubernetes version of the owner and egress denial by the CNI of the cluster. Time to ready and worker memory under gVisor are not measured.

A passing render and schema check proves that the files are valid Kubernetes objects. It does not prove that the probes, limits or network policy behave as intended on a real cluster.
<!-- /memoria:section -->
