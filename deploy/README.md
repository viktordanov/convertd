# deploy

`deploy/` holds the files that run the rs-convert image as an internal service: a kustomize base with a gVisor overlay for Kubernetes, and one Bicep file for Azure Container Apps.

Know two facts before you use these files. First, the product image does not convert documents yet: convertd wires the real engines in wave two, and the `runtime` image is not built until then. Second, no file here has run against a live cluster or a live Azure environment. The files render and validate offline, and [What is verified](#what-is-verified) lists what that proves.

## Contents

1. [Kubernetes and Container Apps side by side](#kubernetes-and-container-apps-side-by-side)
2. [Disk budget](#disk-budget)
3. [Kubernetes](#kubernetes)
4. [Azure Container Apps](#azure-container-apps)
5. [Shutdown and rollout](#shutdown-and-rollout)
6. [What is verified](#what-is-verified)

<!-- memoria:section id="contract" files="k8s/base/deployment.yaml k8s/base/service.yaml k8s/base/networkpolicy.yaml k8s/overlays/gvisor/kustomization.yaml aca/main.bicep" -->
## Kubernetes and Container Apps side by side

Both setups run one container with 2 CPU and 4 GiB per replica, `OFFICE_WORKERS=1`, `RENDER_WORKERS=2`, `SANDBOX=off` and the API key from a platform secret. They differ where the platforms differ:

| Topic | Kubernetes (`k8s/`) | Azure Container Apps (`aca/main.bicep`) |
| --- | --- | --- |
| Startup probe | `GET /readyz` every 2 s, 60 failures: 120 s | `GET /readyz` after 2 s, every 10 s, 10 failures: about 100 s |
| Readiness probe | `GET /readyz` every 5 s, 2 failures | the same |
| Liveness probe | `GET /healthz` every 10 s, timeout 3 s, 3 failures | the same |
| Termination grace | `preStop` sleep 5 s, then 230 s (5 + 210 + 15) | 225 s (`shutdownGraceSeconds` + 15), no `preStop` |
| Temp volume on `/tmp` | `emptyDir` with `sizeLimit: 4Gi` on node disk | `EmptyDir` on the replica storage. The platform sets its size |
| `TMP_MAX_BYTES` | 3221225472 (3 GiB) | 3221225472 (`tmpMaxBytes`) |
| Request time limit | none from the platform | 240 s on the ingress |
| `TIMEOUT_MAX_SECONDS` | 210 | 210 (`timeoutMaxSeconds`) |
| CPU limit | none, requests only | 2 vCPU per replica |
| Network | ClusterIP Service on port 80. A NetworkPolicy denies all egress, DNS included | internal ingress on port 8080. The environment VNet denies egress, outside this repository |
| Security | non-root 10001, read-only root, no capabilities, `RuntimeDefault` seccomp | the numeric `USER` of the image. The other controls are believed to be unavailable |
| Scaling | 1 replica, no autoscaler | 1 to 5 replicas, a new one at 2 concurrent requests |
| gVisor | optional overlay `k8s/overlays/gvisor` | no option in this template |

The Container Apps values marked "believed" or set by the platform are not verified. [What is verified](#what-is-verified) lists them.

Container Apps is believed to stop a request after 240 s (`PLAN.md` cites the limit). It is not verified whether the upload and the response stream count in it. The template keeps `TIMEOUT_MAX_SECONDS` at 210 until a measurement shows the margin (plan 003 step 9). convertd refuses to start if `TIMEOUT_MAX_SECONDS` is more than 210, or less than `TIMEOUT_DEFAULT_SECONDS` (120 by default). The template does not set `TIMEOUT_DEFAULT_SECONDS`. Thus a `timeoutMaxSeconds` below 120 stops every replica at start.
<!-- /memoria:section -->

<!-- memoria:section id="disk-budget" files="k8s/base/deployment.yaml aca/main.bicep" -->
## Disk budget

convertd reserves space for each upload and job output under `TMP_DIR` (`/tmp/convert`) within the disk budget, `TMP_MAX_BYTES`. If a reservation fails, the request gets status 503, and the pod stays up. The budget does not count the worker slots, the LibreOffice profiles, partial writes and the container logs. These use the headroom of the volume.

`TMP_MAX_BYTES` must stay at least 1 GiB below the size of the `/tmp` volume. On Kubernetes, the volume is 4 GiB and the budget is 3 GiB. If the volume fills, the kubelet evicts the pod and every job on it fails. Change `TMP_MAX_BYTES`, `sizeLimit` and both `ephemeral-storage` values together. The ephemeral-storage request equals the limit, so the scheduler reserves the whole volume.

On Container Apps, the app cannot set the size of the `EmptyDir`. The size is believed to be 8 GiB at 2 vCPU. If the replica storage is less than `tmpMaxBytes` plus 1 GiB, lower `tmpMaxBytes`.
<!-- /memoria:section -->

<!-- memoria:section id="kubernetes" files="k8s/base/kustomization.yaml k8s/base/deployment.yaml k8s/overlays/gvisor/kustomization.yaml k8s/overlays/gvisor/runtimeclass.yaml" -->
## Kubernetes

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

<!-- memoria:section id="aca" files="aca/main.bicep aca/main.example.bicepparam" -->
## Azure Container Apps

`aca/main.bicep` creates one internal container app in a Container Apps environment that already exists. It does not create the environment, the VNet or Log Analytics. Deploy it like this:

1. Make a copy of `aca/main.example.bicepparam` outside the repository, with your `environmentId` and `image`.
2. Put the key in the environment: `export CONVERT_API_KEY='<key>'`. The parameter file reads it, so the key never enters a file.
3. Show the change: `az deployment group what-if -g <rg> -f deploy/aca/main.bicep -p <your>.bicepparam`.
4. Apply it: `az deployment group create -g <rg> -f deploy/aca/main.bicep -p <your>.bicepparam`.
5. Read the `fqdn` output. Callers use `https://<fqdn>`.

For a private registry, set `registryServer` and `registryIdentityId`. The identity is a user-assigned identity with pull rights on that registry. A public image needs neither. `apiKeyPrevious` holds the old key during a rotation.
<!-- /memoria:section -->

<!-- memoria:section id="lifecycle" files="k8s/base/deployment.yaml aca/main.bicep" -->
## Shutdown and rollout

convertd gets `SIGTERM` when the platform stops a replica. The manifests set `SHUTDOWN_GRACE_SECONDS=210`, the same as `TIMEOUT_MAX_SECONDS`, so a job with the longest permitted timeout can finish. With the convertd default of 30 s, each rollout stops every conversion that runs longer than 30 s. The platform grace is 15 s more than `SHUTDOWN_GRACE_SECONDS`, so the platform does not kill the process first.

On Kubernetes, the `preStop` sleep of 5 s comes first, so the Service removes the endpoint before `SIGTERM`. The native `sleep` action needs Kubernetes 1.30 or later. The rollout uses `maxSurge: 1` and `maxUnavailable: 0`. The new pod is ready before the old one stops, so a long grace delays only the exit of the old pod.

On Container Apps, `activeRevisionsMode` is `Single`, and there is no `preStop` hook.
<!-- /memoria:section -->

<!-- memoria:section id="verification" files="k8s/base/deployment.yaml k8s/overlays/gvisor/kustomization.yaml k8s/overlays/gvisor/runtimeclass.yaml aca/main.bicep" -->
## What is verified

These checks ran on 2026-11-25. Plan 003 steps 8 to 10 do the live checks, on the cluster and the Azure subscription of the owner.

| Check | Kubernetes | Azure Container Apps |
| --- | --- | --- |
| The files render or compile | `kubectl kustomize` (v1.33.9) renders the base and the overlay | not run locally. CI runs `az bicep build` |
| Schema | `kubeconform -strict` (v0.8.0) passes for all 7 resources | not checked |
| A live deployment turns ready | not done | not done |
| Probe, grace and storage limits | standard Kubernetes fields. The `preStop` sleep needs 1.30 or later | believed: at most 10 probe failures, at most 600 s grace, 8 GiB storage at 2 vCPU |
| No egress from the pod | depends on the CNI, not tested | depends on the VNet, not tested |
| Request time limit | none from the platform | the 240 s boundary is not measured |
| Read-only root and dropped capabilities | set in the manifest | believed to be unavailable |
| gVisor | not run. Time to ready and worker memory are not measured | not applicable |

A passing render and schema check proves that the files are valid Kubernetes objects. It does not prove that the probes, limits or network policy behave as intended on a real cluster.
<!-- /memoria:section -->
