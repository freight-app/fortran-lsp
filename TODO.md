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
- [x] Full 18-project oracle sweep passed with the stable project-mode timing
      gate (`--diagnostic-quiet 5.0`) before the latest request-probe expansion.
- [x] Expanded project-mode request-probe sweep passes across all 18 oracle
      fixtures with the stable timing gate (`--diagnostic-quiet 5.0`).
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
  completion probes, same-file local declaration rename probes, and
  folding-range probes on scope-bearing files. Procedure dummy / callback call
  signatures and fixed-form/cross-file reference policy remain under later
  modelling/debugging work; add more request types only when they compare
  reliably.
- Next point-1 sub-points, in order:
  - [x] Add project-mode semantic-token probes with a small normalized token
        summary per file before considering full token-array comparison.
  - [x] Add project-mode document-highlight probes on sampled same-file
        declaration/reference pairs, normalized to line spans.
  - [x] Add project-mode selection-range probes for representative free-form
        declarations, call expressions, and fixed-form continuation blocks.
  - [x] Add project-mode code-action probes only where the fixture has a real
        missing-import opportunity; otherwise keep code actions deterministic.
  - [x] Stabilize the expanded request probes on the current full-sweep
        failures before continuing the sweep:
        all 18 fixtures now pass with the expanded probes. Continue separating
        harness normalization issues from real resolver/model gaps.
        - Empty fortls/Freight highlight results should not fail the project
          gate.
        - `null` fortls rename results with an empty Freight edit list should
          not fail the project gate.
        - Same-file reference probes should avoid broad derived-type
          declaration positions unless the model intentionally compares type
          reference breadth.
        - Signature-help gaps for concrete helper calls should become focused
          regressions if they are real resolver gaps; procedure dummy/callback
          cases stay in point 5.
        - Fortls rename edits are only an availability oracle in project mode;
          Freight's scoped rename may be narrower than fortls's same-name
          same-file edits.
        - Top-level docs/example package trees are not used for request probes
          because duplicate vendored source copies create non-deterministic
          definition targets.
        - Project diagnostics fail on Freight-only diagnostics; fortls-only
          project diagnostics are treated as oracle noise.
  - [x] Re-run the full 18-project oracle sweep after the new request probes
        are stable on minpack, fftpack, stdlib, fpm, json-fortran,
        neural-fortran, and ODEPACK.
  - [x] Record the expanded sweep result in the fixture table, including any
        new harness filters and model regressions added during the run.
  - [x] Close the duplicate "Project-mode request parity" item in the hardening
        cycle once these probes are live and documented here.
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

- [x] Project-mode request parity beyond symbols/diagnostics. Extend
      `scripts/fortran_lsp_compare.py --project` with sampled hover, signature
      help, references, completion, implementation, rename, folding, and
      semantic-token probes on real source positions. Keep each probe type
      gated only after it is stable against fortls open-order and timing noise.
      Sampled declaration-position hover probes, same-file free-form reference
      probes, concrete call-site signature probes, call-statement completion
      probes, ancestor module-procedure implementation probes, and same-file
      local declaration rename probes are now live. Folding-range probes are
      also live as a Freight-native project gate because fortls returns
      `method not found` for folding; minpack, ODEPACK, and full neural-fortran
      pass. Full neural-fortran stabilization added coverage for re-exported
      procedure signature help, derived-type receiver call completion, implicit
      function-result reference filtering, and declaration-probe sampling that
      no longer treats `type(name) :: var` as a derived-type definition.
      Semantic-token probes are now live with capped normalized summaries
      (token count, valid token count, covered lines, token-type histogram);
      minpack, ODEPACK, and full neural-fortran pass.
      Document-highlight probes are now live on unambiguous local declarations;
      Freight highlight lines must be non-empty and accepted by fortls, while
      fortls-only surrounding/context lines are not required.
      Selection-range probes are now live for declarations, calls, and
      fixed-form continuation positions; Freight must return non-empty
      normalized range chains.
      Code-action probes are now live via a project-local missing-import probe;
      Freight must offer the native add-use quick fix for the indexed export.
      The expanded 18-project sweep passes with these gates; project-mode
      diagnostics, definitions, hover, signatures, rename, and completion now
      treat fortls as an availability oracle where fortls has known missing or
      overbroad behavior.
- [x] Preprocessor parity phase 2. Cover the remaining C-preprocessor shapes
      seen in production Fortran: macro stringification (`#`), token pasting
      (`##`), recursive/nested macro expansion in directive expressions,
      `#line` / line-marker tolerance, and multiline macro bodies with
      continuations. Port only with focused fixtures or real-project evidence.
      Added focused regressions for all listed shapes; `cargo test -p
      fortran-lsp` passes. The repaired `/tmp/fortls-wrapper` oracle is now the
      local differential command.
- [x] Procedure pointer and callback modelling. Index and resolve
      `procedure(interface), pointer :: cb`, dummy procedure arguments,
      procedure-pointer assignments, calls through procedure variables, and
      procedure pointer components without confusing them with type-bound
      bindings. Added hover, definition, signature-help, and call-diagnostic
      coverage for local/imported abstract-interface procedure variables,
      procedure-pointer assignments, and procedure pointer components.
- [x] Generic overload selection by argument characteristics. Improve generic
      interface and type-bound generic resolution beyond argument count and
      keyword names by using declared actual/dummy types where available,
      optional arguments, elemental/pure compatibility, and ambiguity reporting.
      Line-call generic selection now uses a conservative unique-best score
      over actual argument types (declared variables and literals) against
      dummy declarations for ordinary and type-bound generics. Optional and
      keyword compatibility remains the gate, and ambiguous type scores fall
      back to the older non-type selector instead of guessing.
- [x] Semantic-token, folding, and document-highlight audit. Compare Freight's
      native editor-only surfaces against real projects and editor snapshots:
      preprocessor tokens, type-bound bindings, generic interfaces,
      submodules, labels, fixed-form continuations, and include-grafted symbols.
      Added focused semantic-token coverage for preprocessor macros,
      type-bound bindings/generics, generic `module procedure` links, and
      submodule procedure implementations. Freight adapter coverage now checks
      document highlights and folding ranges for type-bound/generic/interface
      shapes. Fixed semantic tokens for named generic bindings and highlights
      for type-bound method aliases that target an implementation.
- [x] Incremental dependency invalidation. Measure large-project edits in
      stdlib/fpm/ODEPACK, then cache include/module dependency edges so edits
      to included files or exported module APIs re-index only affected
      dependents while body-only edits keep the global index stable.
      `Workspace` now caches direct include and `use module` reverse edges,
      tracks a richer API fingerprint (signature, args, visibility, type
      spec, attributes, binding metadata), and reparses only direct dependents
      when an included file changes or a provider file's API fingerprint
      changes. Name-only body edits still skip global symbol-index rebuilding.
      Added regressions for late include insertion and module dependent
      tracking. Full Rust suite passes. After refreshing the local fortls
      oracle, the default fixture and bounded stdlib/fpm/ODEPACK samples pass
      again. Semantic tokens now use a single identifier scan with file-local
      symbol, scope, and token-type caches, and the project harness keeps the
      ODEPACK semantic sample to representative files instead of the 10k-line
      fixed-form archive file.

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

**Environment note (2026-07-14):** `/tmp/fortls-reference` was refreshed from
`fortran-lang/fortls` at `fc68d91` and installed editable into
`/tmp/fortls-venv`; use `/tmp/fortls-wrapper` as the `--fortls` command. The
system `python3 -m fortls` path still lacks package metadata/dependencies. The
`stdlib --max-files 5`, `fpm --max-files 5`, and `ODEPACK --max-files 5` gates
pass with this wrapper after the submodule implementation-definition fix,
semantic-token optimization, and bounded-probe harness updates.

## Validation Commands

Use the smallest relevant subset first, then broaden:

```sh
cargo fmt -p fortran-lsp
cargo test -p fortran-lsp
cargo build -p freight

python3 -m py_compile scripts/fortran_lsp_compare.py
python3 scripts/fortran_lsp_compare.py --fortls /tmp/fortls-wrapper \
  --freight target/debug/freight \
  --request-timeout 30 --diagnostic-timeout 5 --diagnostic-quiet 0.35

python3 scripts/fortran_lsp_compare.py --fortls /tmp/fortls-wrapper \
  --freight target/debug/freight \
  --project /tmp/freight-stdlib-fixture --max-files 0 \
  --request-timeout 90 --diagnostic-timeout 40 --diagnostic-quiet 5.0
```

For large symbol-heavy projects such as quadpack, raise `--request-timeout` to
90 seconds before assuming a hang.
