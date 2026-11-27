# Corpus

The corpus is the set of test files for rs-convert, and `expected.json` records what the service must answer for each file.

Know these facts before you change anything here:

- The repository commits the generator, the expectations and three small fixtures, not the test files. `corpus/generate.py` writes the files into `corpus/out/`, which git ignores.
- `expected.json` pins a SHA-256 for 45 of the 48 files that `generate.py` writes. Linux arm64, macOS arm64 and Linux amd64 (the `corpus` workflow on `ubuntu-24.04`) gave the same bytes for the first 39. The hashes of `pptx-hidden3`, `odp-hidden` and the three `docx-glyphs-*` files come from macOS arm64, and the `corpus` workflow checks them on its next run.
- Private documents never enter the repository. Git ignores `corpus/private/`.

## Contents

1. [What the corpus proves](#what-the-corpus-proves)
2. [Kinds of file](#kinds-of-file)
3. [expected.json](#expectedjson)
4. [Deterministic bytes](#deterministic-bytes)
5. [Generation](#generation)
6. [Changes to the corpus](#changes-to-the-corpus)

<!-- memoria:section id="overview" files="generate.py expected.json" -->
## What the corpus proves

Each file in the corpus exercises one path through the service. The public corpus has one file for each engine path and container, except the legacy `doc`, `xls` and `ppt` formats that step 7 adds. It also has a positive and a negative case for each warning, an encrypted DOCX, deck and sheet, three macro files with a VBA project, six hostile files and three network canaries. Two DOCX files carry a data descriptor on `word/document.xml`, as Go's `archive/zip` writes it. LibreOffice 26.8.3.1 cannot load the stored one, so `docx-stored-descriptor` expects `422 CONVERSION_FAILED` until a fix lands. `docx-deflated-descriptor` marks the boundary and expects 200. The image's engines confirmed both answers on 2026-11-26.

`corpus-bench check` runs every case against the service. A pass of every case will prove these facts:

- The service routes each format to the correct engine.
- Each case gets the expected status, error code, page count and warnings.
- The service rejects each hostile file within its limits.
- LibreOffice makes no network connection.

On 2026-11-26, `check` ran against the real engines (LibreOffice 26.8.3.1 and PDFium 8179 in the first arm64 `runtime` image of 2026-11-26), and 47 of 55 cases passed. The expectations come from the warning rules. Only the page counts that depend on layout were copied from that run. Five of the eight failures changed their expectations to what LibreOffice does. Image inputs are out of v1, so the three image cases (`png`, `jpeg` and `png-bomb`) now expect `415 UNSUPPORTED_FORMAT`, which the service gives for every image input. `png-bomb` stays as a detection case. Against the arm64 `runtime` image `sha256:3ccbb545d9fb`, all 57 cases passed on 2026-11-26.

A pass does not prove that LibreOffice output matches Microsoft Office. The files are synthetic: Python libraries and hand-written XML make them, not Office. The private corpus carries the evidence from real documents.

[`tools/corpus/README.md`](../tools/corpus/README.md) describes `corpus-bench`, the harness that reads `expected.json`.
<!-- /memoria:section -->

<!-- memoria:section id="kinds" files="generate.py expected.json" -->
## Kinds of file

The `origin` field of each entry in `expected.json` names its kind:

| `origin` | Made by | In git | Hash |
| --- | --- | --- | --- |
| `generated` | `generate.py`, from python-docx, python-pptx, openpyxl, msoffcrypto, Pillow and hand-written XML, RTF and PDF | No, written to `out/` | Pinned, except the three encrypted files |
| `hostile` | `generate.py --hostile`: the zip bomb, the XML entity bomb, the PNG pixel bomb, a truncated DOCX, HTML named `.doc` and a DOCX with two links to local files | No, written to `out/hostile/` | Pinned |
| `fixture` | Committed bytes that no tool here can make, under `fixtures/` | Yes | Required, with a `licence` record |
| `derived` | LibreOffice, from a generated file (`doc`, `xls`, `ppt`) | No | A hash or an `unhashed` reason, as for generated files |
| `private` | The owner, outside the repository | Never | Not required |

The three network canaries are `generated` files that `--hostile` also writes, into `out/canary/`. Each one asks LibreOffice to connect to the canary listener at `127.0.0.1:47123`: a Basic macro on load, an external image link and a `WEBSERVICE` formula. The service must not bind that address.

`file-links.docx` (`docx-file-links`) has an `INCLUDETEXT` field linked to `file:///etc/os-release` and a picture linked to `file:///opt/libreoffice/program/intro.png`. Both files exist in the service image. Its case expects the cached field text in the PDF, no text of `os-release` and no image. LibreOffice 26.8.3.1 follows neither link with its defaults, so the case guards the lock-down of `docker/libreoffice/rs-convert-security.xcd` against a change. It cannot fail against an image without that file, and docker/README.md gives the run that shows the lock-down itself.

The hostile files stay small on disk. `zip-ratio.docx` is about 2 MB and declares a 2 GiB entry. `png-bomb.png` is about 1.2 MB and declares 100,000 x 100,000 pixels. The generator never decompresses or parses them.

The corpus has three fixtures: `fixtures/vbaProject-word.bin`, `vbaProject-xl.bin` and `vbaProject-ppt.bin`. They are the VBA projects of the Apache POI macro test files, under Apache-2.0. [`fixtures/SOURCES.md`](fixtures/SOURCES.md) records their upstream commits, both SHA-256 values, the NOTICE text and what each macro does. `generate.py` puts each project into its host file: `docm-vba`, `xlsm-vba` and `pptm-vba`. It refuses a fixture whose SHA-256 differs from `expected.json`.

The corpus has no derived file at this time. `doc`, `xls` and `ppt` files are planned, and their entries will enter `expected.json` with their files.

The private corpus lives in `CORPUS_PRIVATE_DIR`, by default `corpus/private/`, with its own `expected.json` in the same schema. No private URL, file name, render or result goes into the repository. The tooling that fills it and checks it is not built yet.
<!-- /memoria:section -->

<!-- memoria:section id="contract" files="expected.json expected.schema.json" -->
## expected.json

`expected.json` lists every file with its path, origin, hash, tags, pre-flight facts and cases. `expected.schema.json` is its JSON Schema (draft 2020-12, schema version 1), and `corpus-bench validate` applies it.

- **`preflight`** gives what the scanners must read from the bytes alone: format, encryption, the declared slide count, the requested Latin font families and the macro parts. It is for the unit tests of the scanners, which run with no engine. No test reads it yet. `null` means that a scan does not apply or cannot run. An empty list means that the scan ran and found nothing.
- **`cases`** gives one request each: the `options` part, as the API takes it, and an `expect` block. Most cases set `output` to `bundle`, because only a bundle carries both the manifest and the PDF. `images` and `bundle` answers are a tar stream in one zstd frame (`.tar.zst`). The options go to the service as they are, so a case without `output` gets the API default, `pdf`.
- **`expect`** gives the status, and the error `code` when the status is not 200. It can also give the format, family, PDF origin, page count, warnings, blank pages, hidden pages and the allowed canary connections (always 0). `text_present` and `text_absent` list strings that the text of the PDF must or must not hold, with white space ignored, and `pdf_images` gives the number of image XObjects in the PDF. These three need `output` `pdf` or `bundle`. `hidden_pages` lists the pages whose manifest entry has `hidden: true`. `pptx-hidden`, `pptx-hidden3` and `odp-hidden` test it with `slides.include_hidden`, and `pptx-deck20` tests that a deck without hidden slides gets no mark.
- **`warnings`** match as a multiset on code and page. `detail` matches as a subset, and `"*"` matches any value. `unless_installed` drops a font warning when `/v1/capabilities` lists the family.
- **`page_count`** is a number, a `{min, max}` range or `"unrecorded"`. An `"unrecorded"` count depends on layout: a person copies it from a reviewed `check` run. The nine counts of this kind (`docx-long`, `docx-one-page`, `potx`, the sheets) came from the image's engines on 2026-11-26.

The requested fonts follow the real template parts. The python-docx template has Calibri as its major font and Cambria as its minor font. Its default text uses the minor font, so a generated DOCX requests Cambria unless its theme changes the minor font, as in `docx-aptos-theme`. The warning rules are in [src/README.md](../src/README.md#warnings).
<!-- /memoria:section -->

<!-- memoria:section id="hashes" files="generate.py expected.json requirements.txt" -->
## Deterministic bytes

Two generator runs on one machine give the same bytes for every file except `docx-encrypted`, `pptx-encrypted` and `xlsx-encrypted`. msoffcrypto draws a random salt and key on each run, so these entries have an `unhashed` reason instead of a hash.

The generator gets repeatable bytes in three ways. It rewrites every zip with a fixed time stamp, fixed attributes and deflate level 6, and it keeps the entry order and the part bytes. It replaces the save time that openpyxl writes into `docProps/core.xml`. It makes image noise from a seeded `random.Random`, not from Pillow's `effect_noise`, which uses C `rand()`.

Deflate output can depend on the zlib build. The `generator` block of `expected.json` records the environment of the last `--write-hashes`: Python 3.13.5, zlib 1.3.1, Linux-aarch64, in the arm64 `dev` image. macOS arm64 with Python 3.13.18 and zlib 1.2.12 gives the same bytes for every file. `--check` prints a difference in environment first, then compares. The `corpus` workflow runs `corpus/check.sh` on `ubuntu-24.04` (x86_64). It passed on `main` with the 37 hashes of that time, and it checks the two data-descriptor files on its next run.

`requirements.txt` pins every package, with a SHA-256 for every wheel. A change to a pin can change the bytes, so record the hashes again after it.

The generator keeps the `docProps/app.xml` that the python-docx template writes: one page and the application "Microsoft Macintosh Word". Real third-party files look like that, and `docx-breaks12` uses it as the negative case for the page-count warning. Each run reads this fact back from the saved file.
<!-- /memoria:section -->

<!-- memoria:section id="setup" files="generate.py requirements.txt check.sh" -->
## Generation

The generator needs Python 3.13. Run Python with `-I`, so that environment variables and user packages cannot change the output. Run these commands from the repository root:

1. Make a virtual environment outside the repository: `python3.13 -m venv "$HOME/.venvs/rs-convert-corpus"`.
2. Install the pins: `"$HOME/.venvs/rs-convert-corpus/bin/python" -I -m pip install --require-hashes -r corpus/requirements.txt`.
3. Write the corpus: `"$HOME/.venvs/rs-convert-corpus/bin/python" -I corpus/generate.py corpus/out --hostile`.
4. Run `cargo run --manifest-path tools/corpus/Cargo.toml -- validate`.

On an Apple-silicon Mac, step 3 took about 12 s and wrote 47 MB (measured once with the pins in `requirements.txt`). Without `--hostile`, the generator leaves out the hostile files and the canaries, and `validate` then reports them as missing.

Each run checks its own output. Every file must open in its Python library, with two exceptions. The generator never parses the hostile files in `NEVER_PARSED`. For `docm` and `potx`, which python-docx and python-pptx refuse, it reads the main content type instead. The facts that the expectations use must read back from the saved bytes, and the size bounds in `SIZE_BOUNDS` must hold. A failed check stops the run with the file id.

`corpus/check.sh` runs `--check` with nothing but a Python 3.13: it installs the pins into a temporary virtual environment, checks, and removes the environment. The `corpus` workflow (`.github/workflows/corpus.yml`) runs `corpus/check.sh python` after `actions/setup-python` with Python 3.13. It runs on a push to `main` or a pull request that changes `corpus/` or the workflow, and on a manual dispatch. Run it from any directory as `corpus/check.sh`, or give the interpreter as its argument. On an Apple-silicon Mac with a warm pip cache it took about 14 s.

`generate.py` has two other modes:

- **`--check`** generates everything into a temporary directory and compares the ids, paths and hashes with `expected.json`. It exits 1 on a difference. It first prints any difference between the recorded environment and the current one.
- **`--write-hashes`** does the same generation and writes the hashes, the `unhashed` reasons and the `generator` block into `expected.json`. It rewrites the whole file with two-space JSON indentation.
<!-- /memoria:section -->

<!-- memoria:section id="usage" files="generate.py expected.json expected.schema.json" -->
## Changes to the corpus

A new generated file needs a builder and an entry:

1. Add a builder function to `generate.py`. It returns the file bytes, and it can read the bytes of an earlier entry.
2. Add the id, the path under `out/`, the builder and the hostile flag to `ENTRIES`.
3. If the file has a fact that an expectation depends on, read it back in `check_facts`.
4. Add the entry and its cases to `expected.json`.
5. Run `generate.py --write-hashes`, then `generate.py --check`.

These two modes generate the files into a temporary directory. `validate` reads `corpus/out`, so write the corpus again with step 3 of [Generation](#generation) before you run `corpus-bench validate`.

If a file cannot have stable bytes, add its id and the reason to `UNHASHED`. If the generator must never parse the file, add its id to `NEVER_PARSED`. The self-check then gives it only to `check_hostile`, which checks its size and headers.

A committed fixture needs `sha256` and a `licence` record (`spdx`, `source`, and an optional `url` and `notice`) in its entry, and no `cases`: the generator reads it, and the service never gets it. Record its source in `fixtures/SOURCES.md`. `validate` fails on a fixture file that `expected.json` does not list, and when `fixtures/` holds more than 2 MiB.

Next: run `generate.py --check` before you commit a change to the corpus.
<!-- /memoria:section -->
