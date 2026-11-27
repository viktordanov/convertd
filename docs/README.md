# Documentation

This folder holds the documents of rs-convert that are not READMEs: measured results, the writing guidance and a diagram.

Some of these documents are records of past decisions. When one of them disagrees with a README, the README describes the current service. `api/openapi.json` is the contract of the API.

## Documents

| Document | What it holds | Current or record |
| --- | --- | --- |
| [m0-spikes.md](m0-spikes.md) | The results of the M0 spikes: LibreOfficeKit and PDFium from Rust, the spec emitted from the server, and the Go client against the generated code. Each number names its machine and its corpus. | The measurements are current. Its "Still unknown" list is a record. |
| [documentation/](documentation/) | How to write and review documentation: voice, structure and names (`writing.md`), section markers (`sections.md`), what each README covers (`boundaries.md`) and the Memoria procedure (`memoria.md`). | Current. Every README follows it. |
| [architecture.png](architecture.png) | The diagram of the process model: the supervisor, the office and render pools, and the job directory. | A record of the design. [src/README.md](../src/README.md) describes the current process model. |

## Outside this folder

The spike code that `m0-spikes.md` reports on is not in this repository.
