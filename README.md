# fortran-lsp

Native Rust Fortran language intelligence for Freight and standalone editors.

`fortran-lsp` is a Rust Fortran language server engine. It is not a Python
process and it does not need `fortls` at runtime. The crate exposes a parser,
workspace index, diagnostics, and LSP-shaped query primitives that `freight lsp`
embeds directly, and it also ships a small `fortran-lsp` executable for people
who want to run the Fortran server by itself.

## What It Does

`fortran-lsp` gives Freight native Fortran support for:

- free-form and fixed-form parsing, including continuations and legacy comment
  cards;
- preprocessing, `#include`, Fortran `include`, macro definitions, and active
  conditional regions;
- workspace indexing across files, modules, submodules, include files, and
  dependency include roots;
- hover, definition, references, rename, document/workspace symbols,
  completion, signature help, inlay hints, semantic tokens, folding ranges,
  selection ranges, document highlights, implementation lookup, diagnostics,
  and code actions;
- intrinsic procedure/module tables derived from fortls behavior;
- modern Fortran features such as type-bound procedures, generics, submodules,
  abstract interfaces, procedure pointers, `select type`, `select rank`,
  `associate`, and public/private export rules;
- legacy Fortran shapes seen in real numerical libraries, including `COMMON`,
  `NAMELIST`, `ENTRY`, fixed-form continuation blocks, statement functions, and
  old declaration syntax.

The end result is that a Freight project no longer needs a runtime `fortls`
subprocess for normal Fortran IDE features. `fortls` remains useful as a
reference oracle for differential testing.

## Which Binary Should I Use?

For Freight projects, use:

```sh
freight lsp
```

That gives you manifest-aware Fortran support alongside Freight manifest,
C/C++, and assembly language intelligence.

For a plain Fortran project or an editor setup that wants only this server, use:

```sh
fortran-lsp
```

The standalone executable speaks standard LSP over stdio. It indexes the
workspace root plus common `src`, `include`, and `inc` directories, and accepts
optional initialization settings:

```json
{
  "includeRoots": ["mod", "generated"],
  "maxLineLength": 132,
  "maxCommentLineLength": 100,
  "predefinedMacros": {
    "USE_MPI": "1"
  }
}
```

`predefinedMacros` may also be an array such as `["USE_MPI=1", "DEBUG"]`.

## How Freight Uses It

`freight lsp` registers a native `FortranIndexer` by default. The indexer wraps
`fortran_lsp::Workspace`, feeds it manifest source roots, include directories,
predefined preprocessor macros, and `[language.fortran]` settings, then maps the
crate's responses to JSON-RPC LSP responses.

Freight users should prefer `freight lsp` because it adds manifest context that
a standalone language server cannot infer by itself.

## Library Example

Embedding code can use the workspace API directly:

```rust
use std::path::{Path, PathBuf};

use fortran_lsp::{Position, Workspace};

let mut ws = Workspace::new();

ws.set_include_roots([PathBuf::from("include")]);
ws.upsert_file(
    "src/math.f90",
    "module math\ncontains\nsubroutine axpy(a, x)\nreal :: a, x\nend subroutine\nend module",
);
ws.upsert_file(
    "src/app.f90",
    "program app\nuse math\ncall axpy(1.0, 2.0)\nend program",
);

let hover = ws.hover(
    Path::new("src/app.f90"),
    Position::new(2, 7),
    "program app\nuse math\ncall axpy(1.0, 2.0)\nend program",
);

let diagnostics = ws.diagnostics(Path::new("src/app.f90"));
```

`Workspace::upsert_file` is convenient for editor updates. Bulk indexers can
parse files in parallel with `ParsedFile::parse_with_defines` and insert the
results sequentially with `Workspace::upsert_parsed`.

## Design

The crate is split into small internal layers:

- `model.rs` defines public data types: positions, ranges, symbols, parsed
  files, diagnostics, imports, includes, semantic tokens, and edits.
- `parser.rs` handles free/fixed-form source parsing, preprocessing, statement
  recognition, and source-position helpers.
- `intrinsics.rs` contains the intrinsic procedure/module tables used for
  hover, completion, signature help, and diagnostics.
- `workspace.rs` owns the cross-file index and all query operations.
- `tests.rs` contains regression coverage for real-world Fortran behavior.

The workspace index is incremental. It skips unchanged source, keeps a stable
global symbol index for body-only edits, tracks include/module dependency edges,
and reparses only affected direct dependents when included files or exported
module APIs change.

## Validation

The crate is tested at three levels:

```sh
cargo fmt -p fortran-lsp
cargo test -p fortran-lsp
cargo run -p fortran-lsp -- --version
cargo build -p freight
```

Differential testing uses `fortls` as an oracle through the workspace script:

```sh
python3 -m py_compile scripts/fortran_lsp_compare.py
python3 scripts/fortran_lsp_compare.py --fortls /tmp/fortls-wrapper
python3 scripts/fortran_lsp_compare.py --fortls /tmp/fortls-wrapper \
  --project /tmp/freight-stdlib-fixture --max-files 0 \
  --request-timeout 90 --diagnostic-timeout 40 --diagnostic-quiet 5.0
```

The local oracle used during development is an editable fortls checkout at
`/tmp/fortls-reference`, installed into `/tmp/fortls-venv`, with
`/tmp/fortls-wrapper` as the command passed to the compare script.

Current validation includes the default fixture, full `fortran-lang/stdlib`,
and bounded real-project gates for `fpm` and ODEPACK. See `TODO.md` for the
fixture table and historical hardening notes.

## Scope

This crate implements enough LSP transport to run as a standalone stdio server.
It deliberately does not implement Freight manifest discovery, dependency graph
resolution, or editor-specific settings. Those belong to `freight lsp`.

This crate does implement the Fortran language model that Freight needs:
parsing, indexing, diagnostics, and editor query primitives.

## Status

Native Fortran support is considered complete for Freight's v1.0 Fortran LSP
replacement milestone. Future work should be tracked as new compatibility or
performance issues found by real projects, not as a missing runtime fortls
dependency.
