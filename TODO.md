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
- [x] Full 18-project oracle sweep passes with the stable project-mode timing
      gate (`--diagnostic-quiet 5.0`).
- [x] Workspace-wide indexing: `refresh_flags` walks project + dep include
      roots and indexes every Fortran file (parallel parse via
      `Workspace::upsert_parsed`), so a single opened file resolves sibling
      modules; `didClose` restores disk state instead of un-indexing;
      `workspace/didChangeWatchedFiles` refreshes unopened changed files.
- [x] Build defines reach the preprocessor: manifest `[compiler]` +
      default-feature defines seed `#ifdef` evaluation
      (`Workspace::set_predefined_macros` / `ParsedFile::parse_with_defines`).
- [x] Legacy constructs indexed: COMMON members (incl. blank COMMON), NAMELIST
      group names, ENTRY points — via a deferred pass so explicit declarations
      win (no duplicate false positives).
- [x] Fixed-form comment cards skipped by call diagnostics / inlay hints
      (netlib ODEPACK: 416 false errors → 0).
- [x] Linear-time parse on large legacy files (`line_interface_state`
      memoized; masking pass prefilters by name — 10k-line file 5.9s → 173ms).
- [x] Project-mode differentials cover diagnostics, document/workspace symbols,
      and bounded `textDocument/definition` probes on real declaration
      positions. ODEPACK is now a project-mode fixture with documented
      fortls-only legacy-demo noise filtered by the harness.

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

- Project-mode request coverage now includes bounded definition probes, hover
  probes on real declaration positions, same-file free-form reference probes,
  implementation probes for ancestor `module subroutine` / `module function`
  prototypes, concrete call-site signature-help probes, free-form call-statement
  completion probes, and same-file local declaration rename probes. Procedure
  dummy / callback call signatures and fixed-form/cross-file reference policy
  remain under later modelling/debugging work; add more request types only when
  they compare reliably.
- Next point-1 sub-points, in order:
  1. Add folding-range probes on modules, interfaces, submodules, and fixed-form
     continuation-heavy files.
  2. Add semantic-token probes with a small normalized token summary per file
     before considering full token-array comparison.
- Add more production projects only when they exercise a new code shape.
- Convert mismatches into narrow parser/workspace rules only after ruling out
  fortls open-order noise, generated-template artifacts, and harness limits.

### 2. Parser / Model Gaps

- [x] Fuller C-preprocessor expression support. Implemented today:
      conditionals, `defined(...)`, `!`, `&&`, `||`, `==`, `!=`, numeric
      ordering comparisons, integer arithmetic, bitwise operators, shifts,
      modulo, hex/octal/binary literals, C integer suffixes, character
      constants, ternary `?:`, object/function-like macro expansion including
      calls from `#if` / `#elif`, and externally predefined macros (the build's
      `-D` set).
- [x] Broader polymorphic dispatch modelling when multiple runtime target types
      are possible: ambiguous concrete overrides now resolve to the declared
      abstract interface for definition/signature/diagnostics instead of
      guessing a descendant; deferred generic overloads still suppress
      misleading positional hints unless the candidate is unique.
- [x] Richer diagnostics for procedure/type interface compatibility: explicit
      type-bound procedure interfaces now compare required procedure
      characteristics (`pure`, `elemental`) in addition to kind, arguments,
      dummy declarations, result declarations, and passed-object compatibility.
- [x] COMMON **block names** as symbols (`/setup/` is queryable) and
      **BLOCK DATA** units (Program-kind scopes; `end block data` handled).
      COMMON members / NAMELIST groups / ENTRY were already done.
- [x] EQUIVALENCE statements tolerate storage association and create pending
      implicit symbols for undeclared associated names.
- [x] Statement functions (`f(x) = ...` in the specification part) become
      local Function symbols by upgrading their type declarations when present.
- [x] `do concurrent` locality specs are covered by a live no-false-masking
      regression.
- [x] Coarray declarations and basic image-control statements are covered by a
      live no-false-diagnostics regression.
- [x] Parameterized derived type declarations/usages are covered by a live
      no-false-diagnostics regression.
- [x] Defined I/O generic bindings are covered by a live no-false-diagnostics
      regression.
- [x] Continued fixed-form calls are folded for argument diagnostics while
      keeping diagnostic ranges anchored to the physical call-start line.

### 3. LSP Surface Gaps

- [x] Code action: **add `use <module>, only: <name>`** for an unresolvable
      name that an indexed module exports (`Workspace::code_actions_at`;
      inserts after the scope's last `use`, fixed-form aware).
- [x] Formatting provider: `textDocument/formatting` shells out to
      `fprettify` (stdin→stdout) for free-form Fortran when it is on PATH;
      answers null otherwise; forwards non-Fortran to clangd; threads
      `[language.fortran]` `indent`/`indent_width` and `max_line_length`
      through as fprettify flags.
- [x] Single-open-file differential mode: `fortran_lsp_compare.py
      --project <dir> --open-only <substring>` opens only matching files while
      both servers index the whole tree — catches the workspace-indexing bug
      class that all-files project mode structurally hides.

### 4. Performance

- [x] Module symbol caching / incremental reparse.
      `Workspace::upsert_file` already skips unchanged source and avoids
      rebuilding indexes for no-op updates. Body/comment-only text changes still
      reparse that file, but `Workspace::upsert_parsed` now caches the per-file
      symbol index and skips rebuilding the global name index when the parsed
      symbols are unchanged. Measure again before adding partial reparse.
      (The former O(n²) hotspot — `line_interface_state` rescanning the source
      per query — is fixed by per-file memoization; the test suite dropped
      25.5s → 0.8s.)

### 5. Next Hardening Cycle

These are the next TODO points for taking native `fortran-lsp` from "fortls
replacement in normal freight projects" to "hard to distinguish from fortls on
large and unusual Fortran codebases". Work them one at a time; add a regression
and run the deterministic harness for every completed point.

- [ ] Project-mode request parity beyond symbols/diagnostics. Extend
      `scripts/fortran_lsp_compare.py --project` with sampled hover, signature
      help, references, completion, implementation, rename, folding, and
      semantic-token probes on real source positions. Keep each probe type
      gated only after it is stable against fortls open-order and timing noise.
      Sampled declaration-position hover probes, same-file free-form reference
      probes, concrete call-site signature probes, call-statement completion
      probes, ancestor module-procedure implementation probes, and same-file
      local declaration rename probes are now live; minpack, ODEPACK, and full
      neural-fortran pass. Full neural-fortran stabilization added coverage for
      re-exported procedure signature help, derived-type receiver call
      completion, implicit function-result reference filtering, and
      declaration-probe sampling that no longer treats `type(name) :: var` as a
      derived-type definition.
- [ ] Preprocessor parity phase 2. Cover the remaining C-preprocessor shapes
      seen in production Fortran: macro stringification (`#`), token pasting
      (`##`), recursive/nested macro expansion in directive expressions,
      `#line` / line-marker tolerance, and multiline macro bodies with
      continuations. Port only with focused fixtures or real-project evidence.
- [ ] Procedure pointer and callback modelling. Index and resolve
      `procedure(interface), pointer :: cb`, dummy procedure arguments,
      procedure-pointer assignments, calls through procedure variables, and
      procedure pointer components without confusing them with type-bound
      bindings. Add hover/definition/signature/diagnostic coverage.
- [ ] Generic overload selection by argument characteristics. Improve generic
      interface and type-bound generic resolution beyond argument count and
      keyword names by using declared actual/dummy types where available,
      optional arguments, elemental/pure compatibility, and ambiguity reporting.
- [ ] Semantic-token, folding, and document-highlight audit. Compare Freight's
      native editor-only surfaces against real projects and editor snapshots:
      preprocessor tokens, type-bound bindings, generic interfaces,
      submodules, labels, fixed-form continuations, and include-grafted symbols.
- [ ] Incremental dependency invalidation. Measure large-project edits in
      stdlib/fpm/ODEPACK, then cache include/module dependency edges so edits
      to included files or exported module APIs re-index only affected
      dependents while body-only edits keep the global index stable.

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
| 13 | `modern-fortran/neural-fortran` | `/tmp/freight-neural-fixture` | Full 101-file fixture passes; covered `select rank`, submodule masking, constructor/type collisions, typed module functions, labeled blocks, re-exported procedure signature help, implicit function-result reference filtering, and derived-type receiver call completion. |
| 14 | `jacobwilliams/pyplot-fortran` | `/tmp/freight-pyplot-fixture` | Passes without code changes; preprocessed plotting-module coverage. |
| 15 | `jacobwilliams/fortran-search-and-sort` | `/tmp/freight-search-sort-fixture` | Passes without code changes; include-heavy sorting-module coverage. |
| 16 | `jacobwilliams/quadpack` | `/tmp/freight-quadpack-fixture` | Passes; covered include-wrapper diagnostic boundaries and `MOD_INCLUDE` template normalization. |
| 17 | `jacobwilliams/nlesolver-fortran` | `/tmp/freight-nlesolver-fixture` | Passes without code changes; compact nonlinear-solver and sparse-test coverage. |
| 18 | `jacobwilliams/odepack` | `/tmp/freight-odepack-fixture` | Passes project-mode harness with documented fortls-only legacy-demo noise filtered. Covered implicit unnamed main programs, top-level include tails after modules, legacy `external f` dummy declarations, statement-function vs array-element assignment disambiguation, and COMMON block names without false parent-masking. `archive/src/opkdmain.f` remains clean in Freight (1115 symbols in direct smoke). |

**Environment note (2026-07-03):** the system `python3` lost `json5`/`packaging`,
so `python3 -m fortls` no longer runs. Use a venv (`pip install fortls`) or a
wrapper around the `/tmp/fortls-reference` snapshot. The `stdlib` and `fpm`
project-mode runs currently show small fortls-side masking-warning diffs that
are **pre-existing** (A/B against a pre-change freight build produced
byte-identical diffs) — likely fortls-version drift. Re-record those baselines
with a pinned fortls version.

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
