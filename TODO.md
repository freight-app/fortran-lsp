# fortran-lsp TODO

## Goal

Fully replace the `fortls` subprocess inside `freight lsp`.

Fortran files should get native, manifest-aware hover, definition, completion,
signature help, references, document/workspace symbols, diagnostics, folding,
semantic tokens, inlay hints, code actions, and rename from an embedded
`fortran_lsp::Workspace`. No Python dependency. No runtime fortls passthrough.

`fortls` remains the reference oracle for differential tests only. When behavior
is unclear, port what fortls does and add a regression.

See `README.md` for coverage details and `AGENTS.md` section 9b for the
workspace-level plan.

## Current Status

- [x] `freight lsp` registers `FortranIndexer` by default.
- [x] `FortranIndexer` wraps `fortran_lsp::Workspace`.
- [x] Manifest include roots feed `Workspace` include resolution.
- [x] `fortls` is no longer launched by `freight lsp`.
- [x] Freight adapter tests cover LSP-shaped native responses.
- [x] Deterministic JSON-RPC harness covers shared fortls behavior plus
      Freight-only native surfaces.
- [x] Full 17-project oracle sweep passes with the stable project-mode timing
      gate (`--diagnostic-quiet 5.0`).
- [ ] Keep expanding real-project differentials and close concrete gaps found
      there.

## Open Work

### 1. Differential Coverage

Keep using `scripts/fortran_lsp_compare.py` as the gate:

- Deterministic mode compares hover, definition, implementation, references,
  signature help, completion, diagnostics, document symbols, and workspace
  symbols against fortls.
- Deterministic mode also sends Freight-only live requests for inlay hints,
  document highlights, folding ranges, selection ranges, semantic tokens,
  rename, and code actions.
- Project mode copies a real project to a temp root, opens all Fortran files in
  both servers, compares diagnostics, and checks Freight exposes every fortls
  public document/workspace symbol.

Next useful work:

- Add broader/manual project-mode LSP request coverage where it can be compared
  reliably.
- Add more production projects only when they exercise a new code shape.
- Convert mismatches into narrow parser/workspace rules only after ruling out
  fortls open-order noise, generated-template artifacts, and harness limits.

### 2. Parser / Model Gaps

- [ ] Fuller C-preprocessor expression support. Implemented today:
      conditionals, `defined(...)`, `!`, `&&`, `||`, `==`, `!=`, numeric
      ordering comparisons, integer arithmetic, bitwise operators, shifts,
      modulo, hex/octal/binary literals, C integer suffixes, character
      constants, and object/function-like macro expansion.
- [ ] Broader polymorphic dispatch modelling when multiple runtime target types
      are possible.
- [ ] Richer diagnostics for procedure/type interface compatibility.

### 3. Performance

- [ ] Module symbol caching / incremental reparse.
      `Workspace::upsert_file` already skips unchanged source and avoids
      rebuilding indexes for no-op updates. Every real text change still
      reparses that file. Measure before adding partial reparse.

## Real-Project Oracle Fixtures

All paths are local temp clones used by `scripts/fortran_lsp_compare.py`.

| # | Project | Local path | Status / signal |
|---|---|---|---|
| 1 | `fortran-lang/minpack` | `/tmp/freight-minpack-fixture` | Passes; covered declarations, re-exported imports, procedure dummies, `c_ptr`, labeled blocks, `select case`. |
| 2 | `fortran-lang/fftpack` | `/tmp/freight-fftpack-fixture` | Passes; covered default-private exports, unresolved-module cascades, legacy declarations, variadic/reduction intrinsics. |
| 3 | `fortran-lang/stdlib` | `/tmp/freight-stdlib-fixture` | Full 411-file fixture passes; covered submodules, partial indexes, invalid UTF-8, include roots, call diagnostics, generics, kind selectors. |
| 4 | `fortran-lang/fpm` | `/tmp/freight-fpm-fixture` | Full 221-file fixture passes; covered large-project open-order normalization, C interop calls, free/fixed form edges, re-exports, masking rules. |
| 5 | `jacobwilliams/json-fortran` | `/tmp/freight-json-fortran-fixture` | No Freight-only diagnostics remain; remaining diff is fortls-only masking/declaration noise. |
| 6 | `fortran-lang/test-drive` | `/tmp/freight-test-drive-fixture` | Passes; covered same-name derived types and constructor/generic interfaces. |
| 7 | `toml-f/toml-f` | `/tmp/freight-toml-f-fixture` | Passes; covered public generic re-export chains, use renames, overload selection, select-type guards, inherited deferred bindings. |
| 8 | `jacobwilliams/Fortran-Astrodynamics-Toolkit` | `/tmp/freight-fat-fixture` | Passes; covered nested interface imports and whole-module re-export masking diagnostics. |
| 9 | `jacobwilliams/bspline-fortran` | `/tmp/freight-bspline-fixture` | Passes; covered contained procedure dummies masking ancestor parameters and diagnostic quiet timing. |
| 10 | `jacobwilliams/Fortran-CSV-Module` | `/tmp/freight-csv-fixture` | Passes; covered statement-form `open`/`close` and imported parameter masking. |
| 11 | `urbanjost/M_CLI2` | `/tmp/freight-m-cli2-fixture` | Passes; covered semicolon statements, whitespace-tolerant module procedure syntax, compact `doubleprecision`, intrinsic wrappers. |
| 12 | `jacobwilliams/roots-fortran` | `/tmp/freight-roots-fixture` | Passes without code changes; small OO/numerical library shape. |
| 13 | `modern-fortran/neural-fortran` | `/tmp/freight-neural-fixture` | Full 101-file fixture passes; covered `select rank`, submodule masking, constructor/type collisions, typed module functions, labeled blocks. |
| 14 | `jacobwilliams/pyplot-fortran` | `/tmp/freight-pyplot-fixture` | Passes without code changes; preprocessed plotting-module coverage. |
| 15 | `jacobwilliams/fortran-search-and-sort` | `/tmp/freight-search-sort-fixture` | Passes without code changes; include-heavy sorting-module coverage. |
| 16 | `jacobwilliams/quadpack` | `/tmp/freight-quadpack-fixture` | Passes; covered include-wrapper diagnostic boundaries and `MOD_INCLUDE` template normalization. |
| 17 | `jacobwilliams/nlesolver-fortran` | `/tmp/freight-nlesolver-fixture` | Passes without code changes; compact nonlinear-solver and sparse-test coverage. |

## Validation Commands

Use the smallest relevant subset first, then broaden:

```sh
cargo fmt -p fortran-lsp
cargo test -p fortran-lsp
cargo build -p freight

python3 -m py_compile scripts/fortran_lsp_compare.py
python3 scripts/fortran_lsp_compare.py --freight target/debug/freight \
  --request-timeout 30 --diagnostic-timeout 5 --diagnostic-quiet 0.35

python3 scripts/fortran_lsp_compare.py --freight target/debug/freight \
  --project /tmp/freight-stdlib-fixture --max-files 0 \
  --request-timeout 90 --diagnostic-timeout 40 --diagnostic-quiet 5.0
```

For large symbol-heavy projects such as quadpack, raise `--request-timeout` to
90 seconds before assuming a hang.
