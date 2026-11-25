# Documentation

This folder holds the documents of rs-convert that are not READMEs: measured results, the design plan, the writing guidance and the working record.

Some of these documents are records of past decisions. When two documents disagree, this order wins: `ledger/LEDGER.md`, then `ledger/INTERFACES.md`, then the `plan.md` of a ledger item, then `m0-spikes.md`, then `PLAN.md`, then `api/openapi.yaml`.

## Documents

| Document | What it holds | Current or record |
| --- | --- | --- |
| [m0-spikes.md](m0-spikes.md) | The results of the M0 spikes: LibreOfficeKit and PDFium from Rust, the spec emitted from the server, and the Go client against the generated code. Each number names its machine and its corpus. | The measurements are current. Its "Still unknown" list is a record, and the ledger tracks what is still open. |
| [../PLAN.md](../PLAN.md) | The design plan: the API, the architecture, the engines, the image, security and deployment. | A record. The ledger and `INTERFACES.md` replace it where they differ. For example, the ledger drops the `soffice` fallback engine and `OFFICE_ENGINE`. |
| [documentation/](documentation/) | How to write and review documentation: voice, structure and names (`writing.md`), section markers (`sections.md`), what each README covers (`boundaries.md`) and the Memoria procedure (`memoria.md`). | Current. Every README follows it. |
| [ledger/](ledger/) | The working record of intent and progress. `LEDGER.md` holds the owner's intent, decisions and progress. `INTERFACES.md` is the contract between the lanes of work. `AGENT-RULES.md` gives the rules for agents. Each item folder holds the plan of that item. | Current. It changes with each merge. |
| [architecture.png](architecture.png) | The diagram of the process model from `PLAN.md`: the supervisor, the office and render pools, and the job directory. | A record of the design. It shows a render pool, but a PDFium process for each request is still an open option. |

## Outside this folder

`spikes/` holds the frozen spike code that `m0-spikes.md` reports on. `research/` holds the study behind the engine choices. Both are records, and Memoria does not read them.
