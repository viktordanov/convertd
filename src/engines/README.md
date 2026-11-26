<!-- memoria:section id="overview" files="mod.rs" -->
# Engines

`src/engines/` is the supervisor's side of the engines: it starts worker processes, reaps them, and lends them to calls through one pool for each role.

Three duties apply to every change in this folder:

- **No supervisor code waits on a child outside the reaper.** `clippy.toml` bans `Command::output`, `Command::status`, `Child::wait`, `Child::try_wait` and `Child::wait_with_output`. A one-off helper program runs through `Spawner::run_to_completion`.
- **Every worker starts through `Spawner::spawn`.** The spawn runs on the long-lived `convertd-spawner` thread, because on Linux the parent-death signal of a worker follows the thread that started it.
- **The supervisor ends a worker only by SIGKILL of its process group.** The supervisor never asks a worker to shut down, so no LibreOffice teardown runs. The [worker README](../worker/README.md) gives the reason.

`convertd serve` runs two pools: the office pool runs LibreOffice and the render pool runs PDFium. `select.rs` starts them, and `OfficeLok` and `PdfPdfium` implement the engine traits over them. A call that passes `WORKER_KILL_RSS_MB` kills its worker, and poison memory refuses documents that recently hung or crashed one. The soak run is not built yet, and image inputs are out of v1. [Not built yet](#not-built-yet) lists them.

The worker side is in [`src/worker/`](../worker/README.md), and the frames between the two sides are in [`src/ipc/`](../ipc/README.md). `fake.rs` and `fake_markers.rs` hold lane 001's level-A fakes and the `FAKE:` marker parser. This README does not cover them.

## Contents

1. [Concepts](#concepts)
2. [Spawner and reaper](#spawner-and-reaper)
3. [Pool and lease](#pool-and-lease)
4. [Admission queue](#admission-queue)
5. [Recycling](#recycling)
6. [Slot directory](#slot-directory)
7. [Engine adapters](#engine-adapters)
8. [Selection](#selection)
9. [Errors](#errors)
10. [Configuration](#configuration)
11. [Measurement](#measurement)
12. [Not built yet](#not-built-yet)

## Concepts

- **Spawner**: starts each worker in its own process group and registers it. Its **reaper** collects the exit of every registered child.
- **Pool**: the workers of one role, with the task that replaces them. A **lease** gives one idle worker to one call.
- **Admission queue**: the bounded wait for a lease. It gives `QueueFull` or `QueueTimeout` when the pool is busy.
- **Recycling**: the replacement of a worker after a job count, an RSS limit, an age or a suspect engine error.
- **Slot directory**: the private folder and environment of one worker process, made new for each process.
<!-- /memoria:section -->

<!-- memoria:section id="process" files="process.rs" -->
## Spawner and reaper

`Spawner::new(mode)` must run inside a tokio runtime. It listens for SIGCHLD before the first spawn and starts the reaper task. A clone of a `Spawner` shares its registry.

`Spawner::spawn(cmd)` puts the child in its own process group, so its PGID equals its PID. The caller sets the environment, the directory and the stdio. The spawn runs on one process-wide thread, `convertd-spawner`, which never exits. That thread holds the registry lock across the spawn and the insert, so the reaper never sees an exit before the child is registered.

The reaper wakes on each SIGCHLD and every 2 s (`REAP_TICK`) as a safety net. For each registered child, it does these steps:

1. It peeks with `waitid(P_PID, WEXITED | WNOHANG | WNOWAIT)`, which leaves the child unreaped.
2. If the child exited, it sends SIGKILL to the group while the zombie still holds the PGID. Thus the signal cannot reach a process that reused the PID.
3. It reaps the child with `waitpid` and sends the status to every `ChildHandle` of that child.

After that, it sweeps the groups of dead children with `waitid(P_PGID)` until `ECHILD`. The reaper touches only the PIDs that its own `Spawner` registered. Thus two spawners in one test process, or a library that waits on its own child, keep their exit statuses.

`ReaperMode::Owned` is for tests and the `engine` CLI. `ReaperMode::ProcessWide` is for `convertd serve`. It also reaps unknown children and logs each one as `stray child reaped`. This pass peeks each exit with `waitid(P_ALL, WNOWAIT)` first. A worker can exit after its own peek and show up only in this pass, and its group is still killed while its zombie holds the PGID. On Linux it makes the supervisor a child subreaper first, so strays that leave their group reparent to it. In this mode, other code in the process must not wait on a child of its own.

`ChildHandle::kill_group()` sends SIGKILL to the group under the registry lock. It sends nothing and returns `false` once the child is reaped, so a recycled PID never gets the signal. `ChildHandle::wait()` waits for the reaper and is cancel safe. When the last `Spawner` handle is dropped, it kills and reaps every child that is still registered, before it returns.

`Spawner::run_to_completion(cmd, timeout)` runs a helper with stdin from `/dev/null` and captures at most 4 MiB of stdout and of stderr. Past the timeout, it kills the group and returns an error of kind `TimedOut`.

`drain_stderr(fd, worker_id)` reads a worker's stderr until EOF, so a full pipe never blocks the worker. With debug logging on, it logs each line as `worker stderr`, cut to 512 bytes. With debug logging off, it reads the bytes and drops them. EOF does not mean that the worker is gone, because its children can hold the pipe open.

`set_not_dumpable()` marks the supervisor as not dumpable on Linux, so a worker with the same UID cannot read `/proc/<supervisor>/environ`. No code calls it yet.
<!-- /memoria:section -->

<!-- memoria:section id="pool" files="pool.rs conn.rs" -->
## Pool and lease

`Pool::start(spec, spawner)` removes the stale slot directories of this pool, starts a manager task, and returns at once. The manager then starts `spec.size` workers in the background. `PoolSpec` holds the role, the program path, the slot root, the worker environment, the `Init` frame and the limits.

A worker start has four steps. The manager makes the slot directory new, spawns `convertd worker <role>`, sends `Init`, and waits for `Ready` within `start_timeout`. A `Fatal` answer, an exit or the timeout is a failed start. After a failed start, the manager waits 0.5 s, then doubles the wait up to 30 s. A `Ready` resets the wait to 0.5 s.

The pool keeps its idle workers in one list next to a FIFO-fair `tokio::sync::Semaphore`. The number of permits equals the number of idle workers. `Pool::lease(deadline, admission)` takes a permit and pops a worker. It drops a popped worker that has exited or is retired, and tries again. `Pool::try_lease()` takes an idle worker or returns `None`, and never waits.

`Lease::call(op, deadline)` writes one frame and waits for the reply, the exit of the worker, or the deadline. The deadline is an absolute `std::time::Instant`. The results are as follows:

- **Reply**: the reply with the same `id`. The lease stores the worker's job count and RSS from it.
- **Deadline passed**: the lease kills the process group and returns `CallError::Timeout`.
- **EOF or exit**: the lease kills the group and waits at most 100 ms for the exit status. It returns `CallError::Crashed { status }` and logs `worker crashed`.
- **Bad frame**: a wrong `id`, invalid JSON or an oversize frame. The lease kills the worker and returns `CallError::Protocol`.
- **Memory over `kill_rss`**: the call samples the worker's RSS every 200 ms while it waits. At `kill_rss` it kills the process group and returns `CallError::TooBig`, which maps to `LimitExceeded` and is never retried. On a platform where the supervisor cannot read the RSS, the watchdog does not run.
- **SIGKILL from the kernel's OOM killer**: a worker that dies by SIGKILL is taken for the OOM killer in two cases. The first is that the cgroup's `oom_kill` count in `/sys/fs/cgroup/memory.events` rose during the call. The second is that the call's peak RSS (the previous reply's, then each sample) reached `max_rss` or came within 10 % of `kill_rss`. The call is then `TooBig` too: 422 `LIMIT_EXCEEDED`, no crash retry and no poison strike. The log line is `worker killed by SIGKILL near its memory limit`, with `cgroup_oom_kill`, and the restart reason is `rss`. Any other SIGKILL stays a crash.
- **Why the count and the peak**: the kernel can kill a worker under its own limits when all workers together pass the cgroup's limit. A worker that dies also tears down its memory before its exit is seen, so a late sample can read 0. The cgroup's count does not depend on samples, and the peak survives the 0. Without cgroup v2 (no `memory.events`, as on macOS) only the peak decides. The watchdog samples on every tick, even with `kill_rss` off. A failed read is retried on the next tick, and the first failure of a call gets one debug log line.

The `Drop` of a `Lease` decides what happens to the worker. If the call future was dropped before its reply, the worker is killed, because its reply then reaches the next lease. A dead worker, or one that failed a call, goes too. Otherwise the [recycling](#recycling) rules decide.

A lease also reports where its time went. `queued()` is the wait for a permit, and `leased()` is the whole `lease` call. `spawned_on_path()` gives the spawn and bind times when the worker became ready while this lease waited for it. `worker_ref()` names the pool, the slot, the generation and the job number.

`Pool::status()` returns a `PoolStatus`: the target size, the idle, busy and starting workers, the waiting leases, and the flags `ready_once`, `backoff` and `draining`. Each new worker process gets the next generation number of its pool, starting at 1.

`Pool::drain()` makes every new lease `Unavailable` and releases the waiting leases. `Pool::shutdown(grace)` drains, then waits up to `grace` for busy leases to end. Then it kills every worker of the pool, waits for starts that are under way, and removes the slot directories. It removes the slot root too when no other pool has slots in it.

The pool logs these events:

- `worker ready`, with `role`, `pool`, `worker_id`, `pid`, `generation`, `spawn_us`, `bind_us` and `rss_bytes`. `spawn_us` runs from the spawn request to `Ready` and includes `bind_us`. The slot preparation is outside it.
- `worker recycled`, with the reason `jobs`, `rss`, `age` or `error`, and `worker killed`, with the reason `timeout`, `cancelled` or `rss`. A kill over `kill_rss` also logs `worker over its memory limit` with `rss_bytes` and `kill_rss`.
- `worker start failed`, with `error` and `backoff_ms`. `worker started` is a debug event.

The pool sets three metrics: `convert_workers{pool,state}` for `idle`, `busy` and `starting`, `convert_worker_restarts_total{pool,reason}`, and `convert_queue_depth{pool}`. Each lease, each return and each manager event sets `convert_workers` under the idle lock, so the gauges agree with `/readyz`.
<!-- /memoria:section -->

<!-- memoria:section id="contract" files="pool.rs" -->
## Admission queue

A lease waits only when no worker is idle. Then the admission mode sets the limit of the wait:

- **`Admission::Queue`** is for the first engine call of a request. At most `queue_capacity` leases of this mode can wait. The next one gets `QueueFull` at once. A lease waits until the deadline or `queue_wait`, whichever comes first. If `queue_wait` comes first, the result is `QueueTimeout`. Otherwise it is `DeadlineExpired`.
- **`Admission::Continue`** is for a later call of a request that the queue already admitted. It waits until the deadline and does not count against `queue_capacity`.

A lease takes its place in the queue with one atomic increment and then compares the result with the capacity. Thus leases that arrive together cannot pass the bound together. The fair semaphore gives the workers to waiting leases in the order in which they arrived. `status().waiting` counts the waiting leases of both modes.

While the pool has no live worker and its last start failed, a lease is `Unavailable` at once. The pool also wakes the leases that wait at that moment, and each one returns `Unavailable`.
<!-- /memoria:section -->

<!-- memoria:section id="lifecycle" files="pool.rs" -->
## Recycling

When a lease ends, `recycle_decision` uses the numbers in the worker's last reply, not a `/proc` file. It applies the first rule that matches:

1. `Lease::recycle_after()` was called: kill now, reason `error`.
2. The RSS is at least `max_rss`: kill now, reason `rss`.
3. `max_jobs` is 1 or less and the worker served a call: kill now, reason `jobs`.
4. The job count is at least `max_jobs`: replace first, reason `jobs`.
5. The age since `Ready` is at least `max_age`: replace first, reason `age`.

**Kill now** kills the worker, and the manager then starts a new one. **Replace first** keeps the old worker leasable while the manager starts its replacement in a free slot. When the replacement is `Ready`, the old worker is killed, at once if it is idle or else when its lease ends. The manager retires the old worker and places its replacement under the idle lock, the lock under which a returning lease reads `retiring()`. An idle old worker is swapped for its replacement, which takes its permit. A leased one leaves its replacement in `waiting`, and its returning lease releases it. The pool tracks which workers are leased, so an old worker that is neither idle nor leased is known to be gone (discarded, its exit not yet seen). Its replacement then takes a new permit at once. Thus a pool of one worker is never empty during the swap, not even for an instant.

A replacement that is `Ready` while its old worker is still leased waits outside the idle list, without a permit. It becomes leasable when the old worker leaves. A pool thus never runs more jobs at once than its size. The memory of a swap is one extra idle worker, about 250 MB for LibreOffice after its warm-up, never a second worker at its kill limit. The memory rule for a container is therefore (workers × kill) + one idle office worker + 512 MiB.

With `max_jobs` at 1, a process never serves two conversions. A `Ping` and an `Inspect` are not jobs: a render pool inspects before each render, and those calls must not halve its worker's life. `OFFICE_MAX_JOBS` and `RENDER_MAX_JOBS` set each pool's limit and fall back to `WORKER_MAX_JOBS`. Without either variable, the office pool takes 200 jobs and the render pool 50. In the soak an office worker lived 20 s at 50 jobs, and its RSS at recycle did not grow with the job count. `OFFICE_MAX_RSS_MB` still recycles an office worker that grows. The manager also checks the idle workers for `max_age` every `age_check`.
<!-- /memoria:section -->

<!-- memoria:section id="slot" files="slot.rs" -->
## Slot directory

Each worker process gets `<workers_dir>/<pool>-<slot>/` with the subdirectories `profile`, `tmp`, `cache`, `config`, `data` and `run`, all mode 0700. `SlotDir::prepare` removes the slot and creates it again for each new process, so a file that one worker plants never reaches the next. If a profile template is set, `prepare` copies it into `profile/` and skips symbolic links and special files. The slot name uses the pool name, because at level B both pools run the `test` role.

`WorkerEnv` builds the environment of a worker after `env_clear()`, so no service secret such as `API_KEY` reaches a worker. A worker gets exactly these variables:

| Variable | Value |
| --- | --- |
| `UserInstallation` | `file://<slot>/profile` |
| `HOME` | `<slot>` |
| `TMPDIR` | `<slot>/tmp` |
| `XDG_CACHE_HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_RUNTIME_DIR` | `<slot>/cache`, `<slot>/config`, `<slot>/data`, `<slot>/run` |
| `LC_PAPER` | `en_GB.UTF-8` for `DEFAULT_PAPER=A4`, `en_US.UTF-8` for `Letter` |
| `PATH` | `<LO_PROGRAM_DIR>:/usr/bin:/bin` |
| `LANG` | from the supervisor, or `C.UTF-8` |
| `FONTCONFIG_FILE`, `FONTCONFIG_PATH`, `FONTCONFIG_SYSROOT`, `TZ`, `LD_LIBRARY_PATH` | from the supervisor, if set |
| `LC_*` other than `LC_ALL` and `LC_PAPER` | from the supervisor, if set |
| `CONVERTD_SUPERVISOR_PID`, `CONVERTD_WORKER_ID` | the supervisor's PID, and `<pool>-<slot>` |

`WorkerEnv::with_path_prefix` puts `LO_PROGRAM_DIR` first in `PATH`. Without it, the libreofficekit crate adds the directory with a `;` separator and cuts `/usr/bin` off the worker's `PATH`. A worker never gets `LC_ALL`, because it overrides `LC_PAPER`. `WorkerEnv::lc_all_dropped` records that the supervisor had it. LibreOfficeKit 26.8.3 does not take the paper size from `LC_PAPER` or `LANG`: in the dev image, a DOCX with no page size comes out Letter with every setting (measured 2026-11-26). Thus `DEFAULT_PAPER` has no effect yet.
<!-- /memoria:section -->

<!-- memoria:section id="adapters" files="call.rs office_lok.rs pdf_pdfium.rs poison.rs" -->
## Engine adapters

`OfficeLok` implements `OfficeEngine` over the office pool, and `PdfPdfium` implements `PdfEngine` over the render pool. Each call runs these steps through `call::call`:

1. It leases a worker with the request's admission mode and sends one operation, with the request's deadline.
2. It fills `EngineTiming` with the queue, lease, wall and engine times, and with the pre-flight time that the office worker reports (zero for other calls). When the worker started on the request's path, it adds the spawn and bind times.
3. It records `convert_engine_overhead_seconds{pool,op}`, the wall time less the engine and queue times.
4. If the worker crashed and at least 1 s is left, it retries once on a new lease with `Continue`. It never retries after a timeout or an error that the engine reported.
5. It maps the error through `EngineFailure` to the domain's `EngineError`, and logs the worker's message at debug only.

After a load, export, empty-document, limit or internal error, `OfficeLok` recycles the office worker, because LibreOffice's state is suspect. The password errors leave the worker in the pool. `PdfPdfium` recycles a render worker only after an internal error.

`PdfPdfium::render` leases one worker with the request's mode, then takes idle workers with `try_lease` and never waits for them. It splits the pages into contiguous chunks of at least 4 pages, one for each worker. Each chunk gets an equal share of `max_output_bytes`, so the files on disk stay within the job's reservation. The chunks run together. A crashed chunk is retried once on a fresh lease, if at least 1 s is left. A chunk that goes over its share runs again on one lease, with the cap minus the bytes of the finished chunks, if at least 1 s is left. A job thus fails on its cap exactly when one worker fails, and the cap never adds a poison strike. The pages come back in order. `engine` is the longest chunk, plus the rerun. The most severe chunk error wins: a timeout, then a crash, then a password error.

There is no image assembly: image inputs are out of v1 (the owner's decision), and detection refuses them with 415 before any engine call.

Poison memory (`poison.rs`) protects the workers from documents that hang or crash them. It is keyed by the upload's SHA-256 and shared by both pools, and it works like this:

- A call that times out adds one timeout strike. A crash that survived its retry adds two crash strikes. A crash with no retry adds one: there was no time left for it, or the retry got no worker. A protocol error and the RSS kill add none. `poison::strike_for` holds this rule for `convert`, `inspect`, `decrypt` and each render chunk.
- A crash strike counts against every request for the document.
- A timeout strike keeps its attempt: the operation, its work, and the time the worker had from the start of the worker call, so time in the queue does not count. The work is the page count of a render, and for an inspect whether it ran the blank-page probe and which office export made the PDF (`InspectJob.export`, `None` for a PDF upload), so two exports of one upload are different PDFs. A strike counts only against a request for the same operation with no more time and no less work. Times within 500 ms count as the same. A retry with a longer deadline, fewer pages, no probe or another export thus still runs.
- A render strikes once, with the worst strike of its chunks (a crash before a timeout) and the budget of the call that earned it. Between equal strikes the longest budget that still failed counts.
- At two strikes that count against a request, `convert`, `inspect`, `decrypt` and `render` return `Poisoned` before they lease a worker. They check again each time they hold a worker (the first lease, a crash retry, a render rerun), with the time left then: under queue pressure a request with no more time than the timed-out ones is refused instead of killing another worker. A crash whose retry is refused this way still counts one strike. `Poisoned` gives 422 `CONVERSION_FAILED`. Each operation is its own kind of work for the timeout rule, and all four share one entry per upload: `InspectJob` and `DecryptJob` carry the upload's SHA-256 too, also for a converted or decrypted PDF.
- Each strike sets the expiry to now + `POISON_TTL_SECONDS`. An expired entry starts again from zero.
- The list keeps at most 1024 documents and 8 timeout strikes for each. When it is full, the entry that expires first goes. `POISON_TTL_SECONDS=0` turns poison memory off.
<!-- /memoria:section -->

<!-- memoria:section id="selection" files="select.rs" -->
## Selection

`select::start` reads `TEST_ENGINE` and starts the engines that `convertd serve` uses:

- **Unset**: the office pool runs `convertd worker office` and the render pool runs `convertd worker render`. Both warm up before `Ready`.
- **`worker`** (level B, feature `test-hooks`): both pools run `convertd worker test`, behind the same pools, queue and kills.
- **`fake`** (level A): lane 001's in-process fakes, with no pool.

`select::start` removes `$TMP_DIR/workers` once at start. It makes one `ReaperMode::ProcessWide` spawner for both pools, so on Linux the supervisor becomes a child subreaper. The office pool has `OFFICE_WORKERS` workers and a queue of `QUEUE_CAPACITY`. The render pool has `RENDER_WORKERS` workers and a queue of 2 × `RENDER_WORKERS`. A worker has 60 s from spawn to `Ready`.

`PoolReadiness` gives `/readyz` its state from both pools:

- `draining` after `drain`.
- `ready` when each pool has a `Ready` worker, idle or busy.
- `starting` while a pool has never had a `Ready` worker.
- `degraded` when a pool had a `Ready` worker and has none now.

`EngineHandle::drain` stops new leases in both pools at once. `main.rs` calls it only after the requests in flight finish, or after `SHUTDOWN_GRACE_SECONDS` runs out. A request that is still running then, and needs one more engine call, gets `Unavailable`, which is 503: for example, the inspect after an office convert. The level-B test `slow_sigterm_at_level_b_finishes_office_then_render` in `tests/ops.rs` covers a drain with requests in flight. `EngineHandle::shutdown(grace)` waits up to `grace` for the calls in flight, kills every worker and removes `$TMP_DIR/workers`.
<!-- /memoria:section -->

<!-- memoria:section id="failure" files="error.rs" -->
## Errors

`EngineFailure` has the variants of the domain's `EngineError` (`src/domain/engine.rs`), and the adapters convert one into the other with `From`. Its texts are fixed, because worker messages can hold paths. The worker message goes to the log only. The conversions are as follows:

- **Lease errors**: `QueueFull`, `QueueTimeout` and `Unavailable` keep their names. `DeadlineExpired` becomes `Timeout`.
- **Call errors**: `Timeout` stays. `Crashed` and `Protocol` become `Crashed`, because the pool killed the worker in both cases. `TooBig` becomes `LimitExceeded`.
- **Worker errors**: the password errors keep their names. `LoadFailed`, `ExportFailed` and `EmptyDocument` become `Failed`. `Io`, `Unsupported` and `Internal` become `Internal`.
- **Disk errors**: `DiskFull` stays apart from `LimitExceeded`. A full disk comes from other jobs and is worth a retry. `LimitExceeded` is this job's own cap and is final.
- **Test role errors**: `FakeQueueFull` and `FakeUnavailable` become `QueueFull` and `Unavailable`, so level-B fixtures give the same statuses as level A.
<!-- /memoria:section -->

<!-- memoria:section id="setup" files="config.rs" -->
## Configuration

`EngineConfig::from_env` parses the engine variables, and `select.rs` builds the two `PoolSpec`s from them. [The configuration table in src/README.md](../README.md#configuration) lists them with their defaults, next to the supervisor's variables.

The worker counts, the job, RSS and age limits, and `WORKER_KILL_RSS_MB` must be at least 1. The defaults give a 14-CPU machine 7 office and 14 render workers. An office worker holds about 250 MB at `Ready`, so a container with little memory needs explicit values.

Each pool can have its own memory limits. `OFFICE_MAX_RSS_MB` and `OFFICE_KILL_RSS_MB` set the office pool's recycle and kill limits, and `RENDER_MAX_RSS_MB` and `RENDER_KILL_RSS_MB` set the render pool's. A variable that is not set takes the shared `WORKER_MAX_RSS_MB` or `WORKER_KILL_RSS_MB`. LibreOffice needs about 10 times the memory of PDFium: in an ingestion of real documents, office workers needed up to 3281 MiB and render workers stayed under 280 MiB. On a small box, a low render limit leaves the memory to the office pool. The log line `engine pools starting` shows both kill limits.
<!-- /memoria:section -->

<!-- memoria:section id="measurement" files="pool.rs process.rs" -->
## Measurement

The numbers below come from `convertd engine bench`, a hidden command that timed lease, `Ping` and return through a real pool, and spawn to `Ready` through the pool's own start path. The owner removed the `convertd engine` commands on 2026-11-26, so these tables are history. The overhead of the request path is now measured over HTTP: [src/README.md](../README.md#measurement) and [bench/results/](../../bench/results/) give those numbers.

One measurement, made on 2026-11-25 at commit `1eaf917`, on macOS (Darwin 27.3.0), Apple M4 Max with 14 cores. It used a release build with `--features test-hooks`, `--n 10000` and `--spawns 50`. Other jobs kept the load average at 6 to 10, so the tails are noisy:

| µs, three runs, one caller | p50 | p95 | p99 |
| --- | --- | --- | --- |
| lease + `Ping` + return | 7.0 / 11.2 / 9.9 | 15.3 / 114.1 / 17.9 | 23.2 / 592.6 / 26.8 |
| spawn to `Ready` | 1951 / 2146 / 2248 | 2185 / 5844 / 5469 | 2484 / 8984 / 5689 |

The lease alone took 0.1 µs at p50. Spawn to `Ready` is the floor for a process with no engine, because the `test` role binds nothing.

A second measurement, made on 2026-11-26 at commit `1eaf546`, in the dev image `rs-convert-003-dev:arm64`. The image ran in the Docker VM of the same machine, with 14 CPUs and about 4 GB of memory. It used a release build with `--features test-hooks`, `--n 2000`, and corpus files from `corpus/generate.py`:

| Role | Spawn to `Ready`, p50 / max | RSS at `Ready` | One job, runs 1 / 2 / 3 |
| --- | --- | --- | --- |
| `office`, warm-up included | 183 ms / 268 ms | 250 MB | `docx/small.docx` to PDF: 37.3 / 30.9 / 28.3 ms |
| `render`, warm-up included | 2.9 ms / 5.4 ms | 8 MB | `pdf/text3.pdf` page 1 as JPEG at 150 DPI: 9.2 / 8.1 / 8.0 ms |
| `test` | 0.25 ms / 0.37 ms | 4 MB | — |

Lease, `Ping` and return took 3.6 to 3.8 µs at p50 for every role. Over HTTP, the overhead of one DOCX conversion was about 1.3 ms beside 31.7 ms of engine time, with inspect and the probe included. These numbers are not measured under gVisor.

### Soak, 2026-11-26

A soak of 120 minutes ran on 2026-11-26 from 00:41 UTC. It used the image `rs-convert-lead-runtime:wave2b` (`3ccbb545d9fb`), built from main at `1eafd5ee`. The conditions were:

- The Docker VM on the same Apple M4 Max had 14 CPUs and 3.9 GiB of memory.
- The flags were `--read-only`, `--tmpfs /tmp:size=2g`, `--cap-drop ALL` and `--security-opt no-new-privileges`.
- `OFFICE_WORKERS=1` and `RENDER_WORKERS=2`. Every other variable kept its default: `WORKER_MAX_JOBS` 50, `WORKER_MAX_RSS_MB` 1024, `WORKER_KILL_RSS_MB` 2048, `WORKER_MAX_AGE_SECONDS` 3600 and `POISON_TTL_SECONDS` 600.
- Two client threads sent requests over HTTP with no pause, from a driver outside the repository.
- The load picked at random from 96 cases on 42 files. These were the cases of `corpus/expected.json` and an `images` case in JPEG, PNG or WebP for each `bundle` case. The encrypted PPTX and XLSX came with and without the password.
- A sample every 5 minutes read `/proc` in the container, `du` of `/tmp` and `/metrics`. A last sample came 60 s after the load stopped.

The run sent 18076 requests: 6924 `bundle`, 6785 `images` and 4367 `pdf`. Each response had the expected status and code. No response was a 5xx, and no request failed on the client side. The responses were 15280 `200`, 936 `PASSWORD_REQUIRED`, 905 `UNSUPPORTED_FORMAT`, 579 `INVALID_PASSWORD`, 192 `LIMIT_EXCEEDED` and 184 `CONVERSION_FAILED`. Of these, 926 requests used the hostile files and 3031 used the encrypted files.

| Measure, after 15 minutes of warm-up | Result |
| --- | --- |
| Supervisor RSS | 20.1 to 24.8 MiB, slope −53 KiB per hour over 22 samples. The peak RSS stayed at 27.7 MiB from minute 10. |
| Supervisor open descriptors | 21 in each sample, 24 once. 19 at the start and 60 s after the end. |
| Processes in the container | 5, 6 once while a new office worker started beside the old one. No zombie in any sample. |
| `/tmp` | 0.7 to 97 MiB under load. 328 KiB at the start, 656 KiB after the end. The difference is the profile of the office slot, which had the same size after a 1-minute run. |
| Office worker RSS | 238 to 243 MiB at `Ready`, 244 to 391 MiB at recycle, 282 to 572 MiB in the samples. The mean at recycle was 343 MiB in the first half and 345 MiB in the second half. |
| Render worker RSS | 7 MiB at `Ready`, 10 to 51 MiB at recycle. |
| Spawn to `Ready`, p50 / max | Office 159 / 289 ms, render 3.7 / 7.5 ms. |

The pools recycled 994 workers, and the logs show 997 `worker ready` events: the 3 first workers and one replacement for each recycle. `convert_worker_restarts_total` gave the same counts as the logs:

- `render`, reason `jobs`: 615.
- `office`, reason `jobs`: 195.
- `office`, reason `error`: 184.
- No worker crashed or was killed, and no recycle had the reason `rss` or `age`.

Each `error` recycle followed one of the 184 requests for `hostile/xml-entities.docx`, which ended in `CONVERSION_FAILED`. The office adapter recycles a worker after this error, by design. The logs have no `document poisoned`, no `stray child reaped` and no line at `WARN` or `ERROR`. At the end, the container was healthy, and `OOMKilled` was false.

A worker lived 20 s at p50 and 62 s at most, because the job limit replaced it first. The run thus did not exercise the age limit of 1 hour.

The server total of the `200` responses, in ms, after warm-up:

| Family | `pdf` p50 / p99 | `bundle` p50 / p99 | `images` p50 / p99 |
| --- | --- | --- | --- |
| docx | 90 / 5754 | 44 / 5713 | 45 / 5766 |
| pptx | 1002 / 9550 | 285 / 6728 | 306 / 6413 |
| xlsx | 6998 / 12894 | 165 / 9021 | 226 / 8998 |
| odf | — | 16 / 5654 | 27 / 5666 |
| rtf | — | 10 / 5534 | 11 / 3079 |
| pdf | — | 0.8 / 2.5 | 1.8 / 24.7 |
| encrypted PPTX and XLSX | 759 / 6037 | — | — |

The tails come from the single office worker. Of the 757 conversions of files under 100 kB that took more than 3 s, the median spent 99 % of its time in the queue. PDF files go to the render pool, so they never wait for an office job. Their p99 stayed under 25 ms. The p50 and p99 of each family showed no trend over the 24 windows.
<!-- /memoria:section -->

## Not built yet

These parts of plan 002 do not exist yet:

- The fault run of step 13. The soak run of step 13 used HTTP ([Soak, 2026-11-26](#soak-2026-11-26)).

Image assembly (step 11) is out of v1, and the seam for it is removed.
