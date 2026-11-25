# Corpus

The corpus is the set of test files for rs-convert, and `expected.json` records what the service must answer for each file.

Know these facts before you change anything here:

- The repository commits the generator and the expectations, not the files. `corpus/generate.py` writes the files into `corpus/out/`, which git ignores.
- `expected.json` pins a SHA-256 for 33 of the 34 files that `generate.py` writes. The hashes hold only in the environment that recorded them. Linux parity is unverified.
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

Each file in the corpus exercises one path through the service. The public corpus has one file for each engine path and container, except the legacy `doc`, `xls` and `ppt` formats that step 7 adds. It also has a positive and a negative case for each warning, an encrypted DOCX, five hostile files and three network canaries.

`corpus-bench check` will run every case against the service. A pass will prove these facts:

- The service routes each format to the correct engine.
- Each case gets the expected status, error code, page count and warnings.
- The service rejects each hostile file within its limits.
- LibreOffice makes no network connection.

`check` is step 5 of item 005, and it does not exist yet. So far, no engine read these files. The expectations come from the rules in the 005 plan, not from observed output.

A pass does not prove that LibreOffice output matches Microsoft Office. The files are synthetic: Python libraries and hand-written XML make them, not Office. The private corpus carries the evidence from real documents.

[`tools/corpus/README.md`](../tools/corpus/README.md) describes `corpus-bench`, the harness that reads `expected.json`.
<!-- /memoria:section -->

<!-- memoria:section id="kinds" files="generate.py expected.json .gitignore" -->
## Kinds of file

The `origin` field of each entry in `expected.json` names its kind:

| `origin` | Made by | In git | Hash |
| --- | --- | --- | --- |
| `generated` | `generate.py`, from python-docx, python-pptx, openpyxl, msoffcrypto and hand-written XML, RTF and PDF | No, written to `out/` | Pinned, except `docx-encrypted` |
| `hostile` | `generate.py --hostile`: the zip bomb, the XML entity bomb, the PNG pixel bomb, a truncated DOCX and HTML named `.doc` | No, written to `out/hostile/` | Pinned |
| `fixture` | Committed bytes that no tool here can make, under `fixtures/` | Yes | Required, with a `licence` record |
| `derived` | LibreOffice, from a generated file (`doc`, `xls`, `ppt`) | No | A hash or an `unhashed` reason, as for generated files |
| `private` | The owner, outside the repository | Never | Not required |

The three network canaries are `generated` files that `--hostile` also writes, into `out/canary/`. Each one asks LibreOffice to connect to the canary listener at `127.0.0.1:47123`: a Basic macro on load, an external image link and a `WEBSERVICE` formula. The service must not bind that address.

The hostile files stay small on disk. `zip-ratio.docx` is about 2 MB and declares a 2 GiB entry. `png-bomb.png` is about 1.2 MB and declares 100,000 x 100,000 pixels. The generator never decompresses or parses them.

The corpus has no fixture and no derived file at this time. Step 4 of item 005 adds `fixtures/vbaProject.bin` for `docm-vba`, and step 7 adds `doc`, `xls` and `ppt`. Their entries enter `expected.json` with their files.

The private corpus lives in `CORPUS_PRIVATE_DIR`, by default `corpus/private/`, with its own `expected.json` in the same schema. No private URL, file name, render or result goes into the repository. The tooling that fills it and checks it is step 12 of item 005, and it is not built yet.
<!-- /memoria:section -->

<!-- memoria:section id="contract" files="expected.json expected.schema.json" -->
## expected.json

`expected.json` lists every file with its path, origin, hash, tags, pre-flight facts and cases. `expected.schema.json` is its JSON Schema (draft 2020-12, schema version 1), and `corpus-bench validate` applies it.

- **`preflight`** gives what the scanners must read from the bytes alone: format, encryption, the declared slide count, the requested Latin font families and the macro parts. Lane 001 uses it in unit tests with no engine. `null` means that a scan does not apply or cannot run. An empty list means that the scan ran and found nothing.
- **`cases`** gives one request each: the `options` part, as the API takes it, and an `expect` block. A case uses `output=bundle` by default, because only the ZIP carries the manifest.
- **`expect`** gives the status, and the error `code` when the status is not 200. It can also give the format, family, PDF origin, page count, warnings, blank pages and the allowed canary connections (always 0).
- **`warnings`** match as a multiset on code and page. `detail` matches as a subset, and `"*"` matches any value. `unless_installed` drops a font warning when `/v1/capabilities` lists the family.
- **`page_count`** is a number, a `{min, max}` range or `"unrecorded"`. Seven cases are `"unrecorded"`: their count depends on layout, and a person must copy it from a reviewed `check` run.

The requested fonts follow the real template parts. The python-docx template has Calibri as its major font and Cambria as its minor font. Its default text uses the minor font, so every generated DOCX requests Cambria. The warning rules are in [the 005 plan, section 5](../docs/ledger/005-corpus-bench/plan.md).
<!-- /memoria:section -->

<!-- memoria:section id="hashes" files="generate.py expected.json requirements.txt" -->
## Deterministic bytes

Two generator runs on one machine give the same bytes for every file except `docx-encrypted`. msoffcrypto draws a random salt and key on each run, so that entry has an `unhashed` reason instead of a hash.

The generator gets repeatable bytes in three ways. It rewrites every zip with a fixed time stamp, fixed attributes and deflate level 6, and it keeps the entry order and the part bytes. It replaces the save time that openpyxl writes into `docProps/core.xml`. It makes image noise from a seeded `random.Random`, not from Pillow's `effect_noise`, which uses C `rand()`.

Deflate output depends on the zlib build. Thus the hashes hold only for the environment in the `generator` block of `expected.json`: Python 3.13.18, zlib 1.2.12, Darwin-arm64. No Linux run of the generator exists yet, so Linux parity is unverified. Before CI runs `generate.py --check`, record the hashes once with `--write-hashes` in the pinned Linux container.

`requirements.txt` pins every package, with a SHA-256 for every wheel. A change to a pin can change the bytes, so record the hashes again after it.

The generator keeps the `docProps/app.xml` that the python-docx template writes: one page and the application "Microsoft Macintosh Word". Real third-party files look like that, and `docx-breaks12` uses it as the negative case for the page-count warning. Each run reads this fact back from the saved file.
<!-- /memoria:section -->

<!-- memoria:section id="setup" files="generate.py requirements.txt" -->
## Generation

The generator needs Python 3.13. Run Python with `-I`, so that environment variables and user packages cannot change the output. Run these commands from the repository root:

1. Make a virtual environment outside the repository: `python3.13 -m venv "$HOME/.venvs/rs-convert-corpus"`.
2. Install the pins: `"$HOME/.venvs/rs-convert-corpus/bin/python" -I -m pip install --require-hashes -r corpus/requirements.txt`.
3. Write the corpus: `"$HOME/.venvs/rs-convert-corpus/bin/python" -I corpus/generate.py corpus/out --hostile`.
4. Run `cargo run --manifest-path tools/corpus/Cargo.toml -- validate`.

On an Apple-silicon Mac, step 3 took about 12 s and wrote 47 MB (measured once with the pins in `requirements.txt`). Without `--hostile`, the generator leaves out the hostile files and the canaries, and `validate` then reports them as missing.

Each run checks its own output. Every file must open in its Python library, the facts that the expectations use must read back from the saved bytes, and the size bounds in `SIZE_BOUNDS` must hold. A failed check stops the run with the file id.

`generate.py` has three other modes:

- **`--check`** generates everything into a temporary directory and compares the ids, paths and hashes with `expected.json`. It exits 1 on a difference. It first prints any difference between the recorded environment and the current one.
- **`--write-hashes`** does the same generation and writes the hashes, the `unhashed` reasons and the `generator` block into `expected.json`. It rewrites the whole file with two-space JSON indentation.
- **`--spike-layout DIR`** writes the seven M0 spike files under their old flat names, such as `small.docx` and `deck.pptx`. `spikes/corpus/make_corpus.py` calls this mode, because older plans cite that path.
<!-- /memoria:section -->

<!-- memoria:section id="usage" files="generate.py expected.json expected.schema.json" -->
## Changes to the corpus

A new generated file needs a builder and an entry:

1. Add a builder function to `generate.py`. It returns the file bytes, and it can read the bytes of an earlier entry.
2. Add the id, the path under `out/`, the builder and the hostile flag to `ENTRIES`.
3. If the file has a fact that an expectation depends on, read it back in `check_facts`.
4. Add the entry and its cases to `expected.json`.
5. Run `generate.py --write-hashes`, then `generate.py --check` and `corpus-bench validate`.

If a file cannot have stable bytes, add its id and the reason to `UNHASHED`. If the file is hostile, check only its size and headers in `check_hostile`, and never parse it.

A committed fixture needs `sha256` and a `licence` record (`spdx`, `source`, and an optional `url` and `notice`) in its entry. `validate` fails on a fixture file that `expected.json` does not list.

Next: run `generate.py --check` before you commit a change to the corpus.
<!-- /memoria:section -->
