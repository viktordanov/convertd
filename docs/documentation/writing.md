# Writing convertd documentation

This guidance applies to every document in the repository. [Boundaries](boundaries.md) adds notes for each README, and [section IDs](sections.md) lists the review markers. Memoria gives this guidance to every review, so a reviewer reads the prose against it.

## Contents

1. [Required skills](#required-skills)
2. [Readers and kinds of page](#readers-and-kinds-of-page)
3. [Voice](#voice)
4. [Structure](#structure)
5. [Names](#names)
6. [Facts, numbers and claims](#facts-numbers-and-claims)
7. [No context anchors](#no-context-anchors)
8. [Examples](#examples)
9. [Diagrams](#diagrams)
10. [Architecture maps](#architecture-maps)
11. [Maintenance](#maintenance)

## Required skills

Before you write or review documentation, load the `simple-english` and `i-have-adhd` skills of your agent environment. Load each full `SKILL.md` and the references it names. If a skill is not available, stop before you edit documentation and report the missing skill by name. Do not replace a skill with rules from memory.

Apply `simple-english` in its pragmatic mode. Keep its structural rules: short sentences, one instruction per sentence, the condition before the command, active voice, simple tenses, complete grammar, and one name for each concept. Keep the technical words of this project, and do not force dictionary words such as "operate" in place of "run". Apply `i-have-adhd` to structure: lead with what the reader does next or must know, number steps, and end a procedure with one concrete next step. Keep a list to five items. Split a longer list into what to do now and what can wait. Use American spelling (`simple-english` rule 1.14): behavior, color, license, catalog.

## Readers and kinds of page

convertd has two groups of readers, and each kind of page serves one group first.

- **Users of the service** call the API, run the image or use the Go client. They want to know what the service does, how to start it and what each response means.
- **Contributors** change the code. They want a mental model of one boundary: what it owns, the ideas it uses, where to start reading and what the rest of the project expects from it.

The repository has four kinds of page, and each kind has its own shape:

- **The root README** introduces the service to a user who has never seen it. It leads with the problem it solves and a quick start, then gives a catalog of the boundaries, then links to the guides.
- **Guides** explain one task or idea to a user: the API contract, deployment, the Go client. They explain the reader's problem, show a small example, name the tradeoff and let the reader choose.
- **Reference material** is for lookup: the OpenAPI spec, configuration tables, error codes, `Server-Timing` stages, metric names. It is exact and complete, in short controlled sentences or tables. The spec is generated, so a page links to it and does not copy it.
- **Boundary READMEs** give a contributor the mental model of one folder: `src/`, `client/`, `docker/`, `deploy/`, `corpus/`, `tools/corpus/`, `tests/`. They are not an inventory of the code.

## Voice

Describe the project directly in the third person: "convertd starts one supervisor", "The pool kills a worker that passes its deadline". Address the reader as "you" only in instructions. Write for experienced engineers who know Rust, HTTP, Docker and Kubernetes. Do not explain what a type signature, `cargo doc` or `go doc` already shows, and do not repeat a point in other words.

State what a component does. Do not call it "simple", "robust", "lightweight" or "blazing fast": give the measured number instead. Do not add disclaimers about what a component is not. The one exception is the root README's scope statement, which tells a user what the service does not do. Do not describe a dependency in dismissive words. Describe a confirmed defect as current behavior, next to the usage that it affects, and do not fix code in a documentation change.

Write each paragraph on one line in the Markdown source. One line per paragraph keeps diffs and Memoria hunks small, and a reviewer reflows an older document when it next changes. Use a list for parallel items, such as concepts, steps or limits. Use prose for explanation that connects ideas. Do not turn connected explanation into a list, and do not pack a list into a paragraph.

## Structure

Give a page the structure that its length needs. A page with more than four sections gets a contents list after its opening paragraph: vertical, numbered, one linked section per line. A short page has no contents list.

Start every README with one plain sentence that says what the thing is. Then put the facts that a reader must know before anything else: a shutdown duty, a required setting, a limit that causes data loss. Put each duty where the reader meets the operation that needs it, not in a closing "notes" section.

Name sections after the things that the reader works with: "Pool", "Job directory", "Workers". Do not name them after instructions such as "Configure the pool". If a boundary is built from a small set of concepts, list the concepts first under a "Concepts" heading, then give each concept a section in the same order. Do not add a subheading that repeats its parent.

Use a table when the reader chooses between alternatives or looks up values: configuration variables, error codes, output kinds. Do not use a table to map one set of concepts onto another. Do not list every function or route: rustdoc, `go doc` and the spec give the API.

## Names

Use one name for each concept, and use it on every page. The project's names are:

- **convertd**: the project, its binary and its image. **The service**: a running deployment.
- **Supervisor**: the `convertd serve` process. It owns the HTTP API, the queue and the workers, and it never opens a document in an engine.
- **Worker**: a `convertd worker` process. An **office worker** runs LibreOffice through LibreOfficeKit. A **render worker** runs PDFium.
- **Engine**: LibreOffice or PDFium. **Engine time**: the time a worker reports from just before it opens a document to just after the last engine call.
- **Pool**: the set of workers of one role. **Lease**: one worker given to one call. **Recycling**: the replacement of a worker after a job count, a memory limit or an age.
- **Job directory**: the per-request folder under `TMP_DIR`. **Disk budget**: `TMP_MAX_BYTES`.
- **Pre-flight**: the scan of an office file in the office worker before LibreOffice loads it. **Detection**: the supervisor's identification of a format from the bytes.
- **Overhead**: total time minus queue time minus engine time. **Strict overhead** also counts the service's own PDF checks. Use these two terms only with these meanings.
- **Fake engine** and **fake worker**: the two test fakes, `TEST_ENGINE=fake` (in the supervisor, no worker process) and `TEST_ENGINE=worker` (a real worker process with a scripted engine). Contributor pages only. Identifiers such as `levelb_test.go` keep their names.
- **Warning**: one of the quality signals in the manifest, such as `FONT_SUBSTITUTED`. Do not use "warning" for log lines.

## Facts, numbers and claims

Keep every command, flag, path, identifier, environment variable, error code and quoted message exact. Check each default and limit against the code or the configuration parser, not against a plan or a memory.

Give every performance number its conditions: what was measured, on which machine, with which build and input. Say whether it is a measurement or an estimate. A number from `docs/m0-spikes.md` or a benchmark result keeps its source next to it. Do not state an unmeasured speed or size as fact.

Say plainly what a passing check proves and what it does not prove. For example, the contract tests prove that the server and the spec agree. They do not prove that LibreOffice output matches Microsoft Office.

## No context anchors

A context anchor is a word that makes sense only to someone who followed how the project was built. A reader of the repository never saw that history, so documentation and code comments never use one. Context anchors include:

- Milestone, phase, wave or round names: "M0", "the spikes", "wave 2b", "round 3 of the review".
- Planning labels: item, effort or finding numbers ("item 006", "S4", "P8"), plan steps and lane names.
- Process events: a review, a colleague, a soak, an ingestion or a bench named as an event ("the 2026-11-26 ingestion", "the benchmark of record").
- Pointers into private or transient state: CI run IDs, commit hashes, branch names, local image tags and scratch scripts.

State the fact and its conditions, and link the file that holds the evidence. Write "A warm LibreOfficeKit instance converted small files 3 to 6 times faster than a cold `soffice` process, on an Apple-silicon laptop with a synthetic corpus ([docs/m0-spikes.md](../m0-spikes.md))", not "In the M0 spikes, a warm instance was 3 to 6 times faster". The link is the source. The history name is not.

A name that the code or the repository really uses is not an anchor: a file name such as `docs/m0-spikes.md`, a dated record in `bench/results/`, a dependency version, an environment variable, a test name. Refer to it by what it holds where the reader needs the content, for example "the spike measurements". A contributor term from [Names](#names) belongs on contributor pages, never in the root README or a user guide.

A code comment says why the code is as it is in terms of the code: the constraint, the failure it prevents, the measured cost. It does not say which review found the problem, which item asked for it or which round changed it. Git history holds that.

## Examples

An example uses the real API, the real environment variable names and the real file layout. If a page shows a request, it shows the `curl` command or the Go client call, and the response that comes back. Keep examples short. Link to a test or a runnable program for a long example.

Go examples in the client live in `example_test.go` files with a checked `// Output:` block when they can run offline. Rust examples live in doc comments or in `examples/`. The README points to them instead of copying them.

## Diagrams

Add a diagram only when it shows something that the prose and lists do not. Boxes that restate a list are not a diagram.

Show a folder layout or a call path as a compact tree in a `text` code block. Keep annotations to a few words, and keep the tree narrow enough to read without scrolling. Use GitHub Mermaid for a real flow between components, such as a request through the supervisor and the workers. Give each diagram one claim, top-to-bottom direction, default rendering with no theme or color overrides, and an accessible title and description. Check the Mermaid syntax before acknowledgement.

## Architecture maps

When implementation detail would crowd a README, put it in an `ARCHITECTURE.md` beside it. Its reader is a contributor or an agent about to change or debug the boundary. It is a map, not a summary: it says which code to open and records what no single file shows. It never replaces reading the code that you change.

Open it with one sentence that says how to use it and which dependency versions it describes, such as LibreOffice 26.8.3 or PDFium 8179. Then give the call path of the main operation as a tree with file and function anchors, the known caveats as a list, where to change what, and a map of the tests. Memoria does not watch dependencies outside the repository, so name each pinned version and say that those sections need a new check when the pin changes.

## Maintenance

Follow the installed Memoria skill and [Memoria in this repository](memoria.md) for review mechanics. Use the section IDs in [sections.md](sections.md). A section mapping is a reading hint, never proof that other prose is unaffected. Use an import only for text that must stay identical in two places, such as a summary that the root README shows. Use an ordinary link for everything else.

A README's local guidance, in a `README.memoria.toml` beside it, adds to this guidance. Memoria appends guidance and does not resolve conflicts, so keep the two consistent.
