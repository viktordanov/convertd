# Memoria in this repository

Memoria finds the documents that need a review when their inputs change. A person or an agent reviews the prose. Memoria records that review against the exact files that were read. Start with `memoria onboard` to see the state of this checkout and the next step.

## Contents

1. [What Memoria tracks here](#what-memoria-tracks-here)
2. [Who does what](#who-does-what)
3. [After a merge](#after-a-merge)
4. [CI](#ci)
5. [Guidance](#guidance)

## What Memoria tracks here

Every `README.md` is a tracked document. A README covers the files in its folder and below, until it hands a subfolder to another README with a link. The root README links every boundary README in [the map](boundaries.md), so each boundary owns its folder. `memoria.toml` lists the ignored paths: the ledger, the spikes, the research study, lockfiles and generated code.

`memoria.lock` records each review. Memoria writes it. Commit it, and never edit it by hand.

## Who does what

- **An agent that changes code** in a lane branch also updates the README of that boundary in the same branch. It does not run `memoria ack` and does not touch `memoria.lock`. A worktree is not the main checkout, and an acknowledgement belongs to the merged result.
- **The lead** merges branches one at a time. After each merge, the lead runs the review cycle in the main checkout and pushes only when `memoria check` passes.
- **A reviewer** reads the whole document and the sources that `memoria review` names, against this guidance, and records the result with a note that says what was verified.

## After a merge

1. Run `memoria review --format json` in the main checkout.
2. If the next action is `render`, run `memoria render` for that document, then run the review again.
3. For each ready document, capture an artifact outside the repository with `memoria review <DOCUMENT> --save "$dir"`.
4. Read the document and its sources. If the prose is wrong, correct it, then capture a fresh artifact.
5. Record the result with `memoria ack <DOCUMENT> --packet <file> --reviewer <label> --result <updated|no-update> --note "<what you verified>"`.
6. Run `memoria check`. Push when it passes.

Several reviewers can work at the same time, one document each, in the main checkout. The installed Memoria skill gives the full procedure.

## CI

`.github/workflows/memoria.yml` runs `memoria check` on every push and pull request. It installs a checksum-verified Memoria release and needs read permission only. A failed check means that a document is pending. CI never records a review: review locally, then commit `memoria.lock`.

## Guidance

`memoria.toml` loads [writing](writing.md), [sections](sections.md) and [boundaries](boundaries.md) as guidance for every review. When you change a guidance file, run `memoria guidance --changed` and decide which documents the change affects. Ask the owner before you invalidate documents for a guidance change.
