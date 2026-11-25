# Fixture sources

This folder holds the committed bytes that no tool in the repository can make. Each file has an entry in `corpus/expected.json` with its SHA-256 and a `licence` record, and `corpus-bench validate` fails on a file that has no entry. All files together must stay below 2 MiB.

## VBA projects

The three `vbaProject-*.bin` files are the VBA projects of the Apache POI macro test files. `corpus/generate.py` puts each one into a macro-enabled file of its host: `docm-vba`, `xlsm-vba` and `pptm-vba`. No tool here can write a VBA project, and the owner did not supply one. Thus these files are the fallback of plan 005, Decision 11.

| File | Upstream file | Commit | SHA-256 of the upstream file | SHA-256 of the part |
| --- | --- | --- | --- | --- |
| `vbaProject-word.bin` | `test-data/document/SimpleMacro.docm`, part `word/vbaProject.bin` | `e3ad497b79b0135659c6b6d1c011e3fbabb91a14` | `fd591958fcf5322f72c0a740e9606309c949254bda4c3d9bd966481ddf220563` | `921bbf317d6bf7c46d4c425977ed22a0c18a364427c387327bc1792f3f43b837` |
| `vbaProject-xl.bin` | `test-data/spreadsheet/SimpleMacro.xlsm`, part `xl/vbaProject.bin` | `f5091846ffac98632ea67aa9cab7d94c4c2bf2a6` | `f76c986f4ebc25c2cc57c088b2511a1269f4bd61d6223a2ab58db351da348ba6` | `f9becb822f01de291b0ec95ea4061235e1f98ea31f80d840009a6caab829ce90` |
| `vbaProject-ppt.bin` | `test-data/slideshow/SimpleMacro.pptm`, part `ppt/vbaProject.bin` | `e3ad497b79b0135659c6b6d1c011e3fbabb91a14` | `8a3573c82fd07a301d7f175b8bee646c0b58eb8a945bd0ff408225b6e2b89b15` | `dcd9ead35ba39db10b32d95550e1ae67c747e10e22fb8f53b48d6b222820584f` |

The upstream files are in the repository `https://github.com/apache/poi`. Each commit is the last one that changed its file. The file is unchanged on `trunk` at `f63b77bb7497d950073969f9d418f2270dd87edd` (2026-11-25). To get a part again, download the upstream file at its commit and extract the part with `unzip -p SimpleMacro.docm word/vbaProject.bin > vbaProject-word.bin`. Then compare both SHA-256 values with this table.

**What the macros do.** Each project has one module, `Module1`, with one procedure, `TestMacro`. No event starts it: there is no `AutoOpen`, `Document_Open`, `Workbook_Open` or button. `olevba --decode` from oletools gives this source:

- **Word:** `ActiveDocument.Paragraphs(1).Range.Text = "This is a macro word processing document"`. The `ThisDocument` module is empty.
- **Excel:** `ActiveCell.FormulaR1C1 = "This is a macro workbook"`, then `Range("A2").Select`. The procedure has a keyboard shortcut attribute, which a user must press. The `ThisWorkbook` and `Sheet1` to `Sheet3` modules are empty.
- **PowerPoint:** sets the title text of the first two shapes on slide 1 to "This is a macro slideshow" and "subtitle".

No macro opens a file, a network connection or a process.

**License.** Apache POI is an Apache Software Foundation project under the Apache License 2.0. The files are in the ASF source tree, and its `legal/LICENSE` lists no separate terms for `test-data/`. Thus the files are under Apache-2.0, the license of this repository too. The POI `legal/NOTICE` starts with this text, which goes with the files:

```text
Apache POI
Copyright 2003-2026 The Apache Software Foundation

This product includes software developed at
The Apache Software Foundation (https://www.apache.org/).
```

The Word file also has a `word/vbaData.xml` part that lists the macro for Word's user interface. The corpus leaves it out: the VBA part and its relationship are enough for the `MACROS_IGNORED` rule.
