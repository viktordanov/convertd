# Documentation boundaries

This page lists the tracked READMEs, what each one covers, and what each one must stress. Apply each note only to its named README. The [shared guidance](writing.md) covers voice, structure, names and diagrams.

## The map

```text
README.md                  the service, the quick start, the catalog
├── src/README.md          the convertd crate: process model, layers
│   ├── src/engines/README.md   pool, leases, recycling, engine adapters
│   ├── src/worker/README.md    worker processes and the engines in them
│   └── src/ipc/README.md       the supervisor-worker protocol
├── tests/README.md        test levels, fakes, nextest profiles
├── client/README.md       the Go client and its contract tests
├── docker/README.md       the image: engines, fonts, pins, targets
├── deploy/README.md       Kubernetes, and any container platform
├── corpus/README.md       the test corpus and its expectations
├── tools/corpus/README.md corpus-bench
└── docs/README.md         the documentation map
```

The root README links each top-level README, and `src/README.md` links the three worker-side folders, so each link hands its folder to that README.

Generated files and lockfiles are not inputs. `memoria.toml` lists the exact ignores.

Add a nested README only for a folder with its own contract. Link it from its parent, so that the parent hands the folder over.

## Root README

Introduce rs-convert to a user who has never seen it. Lead with what the service does in one sentence, the problem it solves, and what it does not do (no OCR, no HTML, no fidelity guarantee against Microsoft Office). Then give a quick start that works today: convert one file with `curl` and read the headers. Run the published image, and give a second path that builds `convertd` from source and runs it with the fake engine, for a reader without Docker. Then the catalog of boundaries, grouped by reader: using the service, deploying it, contributing to it. End with the documentation map and the license.

Keep the API description short and link to the spec. Keep design history in the spike results, not in the root README. Do not claim performance that the benchmarks have not measured.

## src/README.md

Give a contributor the mental model of the `convertd` crate. Start with the process model: one supervisor, office and render workers, and why every engine runs in a worker process that the supervisor can kill. The supervisor itself runs only bounded detection of the format: the signature, the ZIP central directory and a few small entries. Then the layers (domain, rest, detect, jobs, bundle, engines, worker, ipc) and the dependency rule between them. Then the path of one request as a tree, from the upload to the response. Link to the three worker-side READMEs for their detail, and to the configuration table instead of repeating it.

## src/engines/, src/worker/ and src/ipc/ READMEs

`src/engines/README.md` explains the pool from the supervisor's side: spawning, the reaper, leases, the admission queue, deadlines, recycling and the adapters that implement the engine traits. `src/worker/README.md` explains a worker process from the inside: its start, the engine it loads, the descriptors it moves, pre-flight and the way it leaves. `src/ipc/README.md` gives the frame format and the message types in a few lines. Name the duties that a change must not break where they apply: a worker never drops the LibreOffice instance, `trim_memory` is never called, no code waits on a child outside the reaper, and PDFium is called from one thread only.

## tests/README.md

Explain the test levels: unit tests in `src/`, integration tests in `tests/`, the two fakes (the fake engine replaces the engines in the supervisor, the fake worker runs `convertd worker test` behind the real pool), the `test-hooks` cargo feature, and the contract tests in `client/contract/`. Explain the nextest profiles in `.config/nextest.toml` and the `slow_` name rule. Give the exact commands that CI runs.

## client/README.md

Write for a Go developer who calls the service. Lead with installation and one conversion, then the methods, the errors and their retry rules, and the behavior of streamed uploads and bundles. Say that only `client/convert` is public and that the generated code is internal. Keep `client/contract/` to one short section for contributors.

## docker/README.md

Explain what the image holds and why: LibreOffice with only the parts the service uses, PDFium, the font set, `build-info.json`, the non-root user and the read-only root. Give the build targets and their commands. Explain how to move to a new LibreOffice or PDFium release: the pins, the SHA-256 checks and the signature check. State the image size and the conditions of that measurement.

## deploy/README.md

Write for an operator. Give the Kubernetes setup: probes, termination grace, the temp volume and `TMP_MAX_BYTES`, the request limit and `TIMEOUT_MAX_SECONDS`, the worker memory limits, and the optional gVisor runtime class. Say which of these settings any other container platform needs too. Say what is verified and what is not.

## corpus/README.md and tools/corpus/README.md

Explain what the corpus proves and how its expectations work: generated files, committed fixtures, the private corpus that never enters the repository, and `expected.json`. Explain why most files are generated and how their hashes are pinned. The `tools/corpus` README explains `corpus-bench` and its subcommands, and how its results are compared over time.

## docs/README.md

A short map of the documentation that is not a README: the spike results, the documentation guidance and the architecture diagram. Say which documents are current and which are records of a past decision.
