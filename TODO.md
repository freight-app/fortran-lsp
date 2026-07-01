# fortran-lsp TODO

**End goal:** this crate fully replaces the `fortls` subprocess inside
`freight lsp`. Fortran files get native, manifest-aware hover, definition,
completion, signature help, references, document symbols, and diagnostics from
an embedded `fortran_lsp::Workspace` — no Python dependency, no passthrough.

`fortls` (cloned at `/tmp/fortls-reference` in past sessions) is the reference
implementation: when behaviour is unclear, port what fortls does and add a
regression test.

See `README.md` for current coverage. Workspace-level plan: `AGENTS.md` §9b.

---

## 1. Integration into `freight lsp` (the actual end goal — lives in `crates/freight`)

The crate is useful only once freight calls it. How to solve:

- [x] `FortranIndexer` in `freight/src/lsp/indexers/` wrapping `Workspace`;
      Fortran URIs now use native hover, definition, completion, signature
      help, diagnostics, document symbols, folding ranges, references,
      document highlights, implementation lookup, selection ranges, semantic
      tokens, code actions, and rename, and `fortls` is no longer launched by
      default.
      Freight adapter tests now cover the LSP-shaped responses for include-root
      and line-length manifest plumbing, diagnostics, workspace symbols,
      semantic tokens, selection ranges, implementation lookup, inlay hints,
      document highlights, folding ranges, code actions, and rename workspace
      edits.
      The deterministic JSON-RPC differential harness also sends Freight-only
      live `textDocument/inlayHint`, `documentHighlight`, `foldingRange`,
      `selectionRange`, `semanticTokens/full`, `rename`, and `codeAction`
      requests against the actual `freight lsp` process, while keeping the
      fortls comparison scoped to fortls-owned behavior.
- [x] Feed include roots from the manifest (`[compiler].includes`, dep include
      dirs) into `Workspace`'s include resolution.
- [ ] Differential-test against fortls over JSON-RPC on real projects (same
      oracle technique as clang-bridge vs clangd — see
      `clang-bridge/TODO.md`) and close any remaining behavior gaps. Freight now
      uses the embedded Fortran indexer by default instead of a fortls
      passthrough.
      - Harness started in `scripts/fortran_lsp_compare.py`: compares native
        Freight Fortran responses with fortls for hover, definition,
        implementation, references, signature help, diagnostics, document
        symbols, and workspace symbols on a deterministic fixture. The local
        harness uses lightweight shims for missing fortls Python dependencies
        when running against `/tmp/fortls-reference`.
      - Real-project mode added with `--project <dir>`; it copies the project to
        a temp root, opens every Fortran file in both servers, compares
        diagnostics exactly, and checks that Freight exposes every fortls public
        document/workspace symbol. It currently passes on the local Freight
        examples `examples/fortran/hello`, `examples/mixed/tri-lang`, and
        `examples/misc/doc/libs/linalg`.
      - External fixture coverage started with `fortran-lang/minpack` cloned at
        `/tmp/freight-minpack-fixture`; the full 13-file project passes after fixing
        declaration array constructors, user-module re-exported `import`
        names, procedure dummy declarations, intrinsic derived types such as
        `c_ptr`, labeled `block` constructs, and `select case`.
      - Second external fixture started with `fortran-lang/fftpack` cloned at
        `/tmp/freight-fftpack-fixture`. It drove fixes for public interface
        prototype exports in default-private modules, unresolved-module type
        diagnostic cascades, legacy declarations without `::`, and lenient
        diagnostics for variadic/reduction intrinsic calls such as `max(...)`
        and `all(...)`. The full 70-file project now passes the real-project
        differential harness.
      - The project harness now drains diagnostics while bulk-opening files and
        waits for a quiet final diagnostic state instead of comparing the first
        notification burst. This keeps larger projects from blocking on stdout
        pipe backpressure and makes full `minpack`/`fftpack` runs usable gates.
      - Third external fixture started with `fortran-lang/stdlib` cloned at
        `/tmp/freight-stdlib-fixture`. The first 20-file slice drove support
        for completion comparison in the deterministic harness, prefixed module
        procedure prototypes such as `pure module function` inside named
        interfaces, submodule host association through ancestor modules
        including private helper types, legacy derived-type definitions written
        as `type name`, constructor/named generic interface duplicate handling,
        type-bound targets declared in anonymous or named host interface
        prototypes, procedure pointer components in type data parts,
        partial-module export diagnostic suppression, type-bound generic
        document symbols, free-form continuations with interleaved comments,
        ancestor-module `use` association into submodules, and intrinsic calls
        such as `date_and_time(values=...)` and
        `flush(..., iostat=..., iomsg=...)`. Kind selector diagnostics now cover
        declarations once per affected declared object, with cascade suppression
        through unresolved direct uses, ancestor uses, and partially indexed
        re-export modules. Function/result masking covers logger/hashmap cases
        such as `time_stamp`, `slots_bits`, and explicit-result
        `total_depth`, while avoiding false-positive component result
        cascades. Abstract-interface dummy arguments distinguish same-name
        type-bound bindings such as `slots_bits` from aliased bindings such as
        `pid => process_get_id`, matching the next stdlib system/hashmap
        diagnostic boundary. Whole-module imports of partially indexed modules
        now report the fortls-style unresolved module primary diagnostic. The
        project harness now sends large `didOpen` messages with a nonblocking
        write loop that drains diagnostics while stdin back-pressures, fixing
        the bulk-open timeout at `src/system/stdlib_system.F90`; the 20-file
        stdlib slice now passes. The 25-file slice reaches
        `src/system/stdlib_system_subprocess.F90`; false native diagnostics for
        `import process_ID` in submodule C-binding interfaces are fixed by
        resolving imports through ancestor-module host association and ancestor
        uses. Named ancestor generic interfaces now participate in submodule
        parent-scope masking diagnostics, closing the fortls-only `elapsed`,
        `is_running`, and `wait` warnings without reintroducing ANSI/path
        overreports. Type-bound result names, selected repeated clock locals,
        and named-interface function dummy/result collisions now cover the
        remaining `count_max`, `current_time`, and `process` diagnostics in
        `stdlib_system_subprocess.F90`. The 25-file stdlib slice now passes the
        real-project differential harness. Expanding to the 100-file stdlib
        slice drove fixes for partial-module unresolved diagnostics on private
        `use, only:` dependencies, array-constructor call argument splitting in
        hashmap examples, and continued `use, only: operator(...)` parsing in
        ANSI examples. Expanding again to 220 files drove lossy source decoding
        in the harness for invalid UTF-8 fixtures, filesystem include-root
        resolution for unopened include files such as `include/macros.inc`,
        declared-type cascade suppression through partially indexed imports,
        and a narrower fortls-style partially indexed module rule: whole-module
        imports of local-API modules such as `stdlib_hashmap_wrappers` report
        unresolved, while pure re-export aggregators such as `stdlib_sparse`
        stay quiet. The 220-file stdlib slice now passes. Expanding to the full
        411-file local stdlib fixture drove call-diagnostic cascade fixes for
        array references that shadow intrinsic names (`loc`, `shape`, `scale`),
        unresolved modules that may provide intrinsic-name generics (`char`),
        `use,intrinsic` syntax without a space after the comma,
        `merge(mask=...)`, comparison operators inside call arguments such as
        `stride == 0`, and use-site-sensitive partial-module diagnostics
        (program imports stay quiet, module/submodule imports still report).
        The harness now filters fortls workspace-symbol false positives that do
        not appear in any fortls document-symbol tree. The full local stdlib
        fixture now passes.
      - Fourth external fixture started with `fortran-lang/fpm` cloned at
        `/tmp/freight-fpm-fixture`. The initial 80-file slice drove fixes for
        non-ASCII source-line slicing, include lookup through files already
        indexed elsewhere in the project, quiet external `mpif.h` includes,
        nested submodule document-symbol parents, preprocessor directives
        embedded inside continued free-form declarations, and unresolved
        `only:` re-export cascade behavior. The 80-file fpm slice still
        mismatches diagnostics; remaining differences are mostly partial-project
        module-resolution policy and fortls variable-masking diagnostics.
        A follow-up pass added module-callable/generic-interface parent masking
        for concrete procedure locals and dummies (`lower`, `upper`, `str`,
        `os_name`) while excluding interface prototypes and a function's own
        implicit result (`glob`, `has_manifest`), and marked `c_f_pointer`'s
        `shape` argument optional for scalar targets. The `f_string` duplicate
        parser issue from generic module-procedure links is fixed, and direct
        unqualified calls to type-bound procedure implementations now keep their
        explicit passed-object argument, removing the fpm
        `add_dependency_node`/`has_dependency` call-shape false positives.
        Member-call receiver extraction now handles array components such as
        `self%variants(i)%has_cpp()` and
        `self%dependency(jj)%load_from_toml(...)`, removing those direct-call
        cascades. The parser also indexes `enumerator` declarations as
        parameter-like public symbols and treats `& ! comment` as a continued
        free-form line, removing the `fpm_compiler` enum and
        `flag_gnu_openmp` export false positives. Derived-type lookup now
        follows public module re-export chains, removing fpm declared-type
        cascades for re-exported types such as `package_config_t`,
        `dependency_config_t`, `serializable_t`, and `fortran_config_t`.
        Type-member masking now verifies that a candidate actually belongs to
        the parent type source range and ignores aliased type-bound binding
        names when comparing against local dummies/results, removing fpm false
        positives around `new_build_progress`, platform `name`/`os_name`, and
        `write_response_file(name, argv)` while keeping fortls-compatible
        reports such as `compiler_name`. `command_argument_count()` is now
        treated as an all-optional intrinsic, and the previous Freight-side
        `c_opendir`/`c_closedir`/`get_dos_path` call-shape diagnostics are gone
        from the comparison. The project differential harness now normalizes
        unresolved diagnostics for modules present in the compared project
        slice, avoiding fortls open-order/stale local-module noise while still
        comparing real external missing modules. The fpm 80-file and 120-file
        slices now pass. Expanding to 160 files drove `.f` files under explicit
        `free-form` paths, include-provided shorthand kind selectors, implicit
        result substring references, string-literal call text, and
        `c_associated` optional-argument handling. Continue from the 160-file
        slice before expanding to the full 221-file fixture. Current
        high-signal remaining gap: shorthand kind-selector diagnostic policy
        around direct unresolved `only:` imports (`stdlib_kinds: dp`) versus
        resolved/re-exported kinds (`sp`, `compiler_enum`). Keep preferring
        narrow fortls parity rules; broad repeated-local, prototype-argument,
        generic result-name, broad partial-module `only:`, or broad
        kind-selector suppression/reporting rules overreport in other
        stdlib/fpm submodules and should not be used as-is. The 160-file
        boundary is now closed: project-symbol stale diagnostics are normalized
        in the differential harness, explicit parent-scope `use, only:` names
        participate in variable-masking diagnostics (`OS_NAME`/`os_name`), and
        `use, non_intrinsic :: ...` is parsed correctly. The fpm 200-file slice
        now passes. The full 221-file fpm fixture now passes as well
        (`--max-files 220` and full-project `--max-files 0`). Continue with a
        new external fixture or broader/manual LSP request coverage, and only
        convert new mismatches into narrow parser/workspace rules when they are
        not harness/open-order artifacts.
      - The project harness now includes project-local `.fypp` declaration
        names when normalizing open-order diagnostics, including explicit
        `use` aliases such as `i8 => int64`, and compares diagnostics as a
        message set because location information is intentionally stripped.
        This keeps generated-template projects such as stdlib from treating
        unresolved generated modules as real native-regression cascades while
        preserving external missing-module cases such as fpm's stdlib
        metapackage example.
      - Fifth external fixture started with `jacobwilliams/json-fortran`
        cloned at `/tmp/freight-json-fortran-fixture`. The full 61-file
        project drove preprocessor include splicing for continued procedure
        dummy argument lists and declaration blocks, fold-time filtering of
        inactive preprocessor branches inside continued generic/type-bound
        binding statements, and call-site scoped resolution so internal
        procedures shadow same-name type-bound methods. Full-project
        diagnostics now have no Freight-only messages in the differential;
        the remaining mismatch is fortls-only masking/declaration warnings
        such as repeated `json_string`/`root` masking and include-generated
        dummy declaration reports in `json_value_module`.
      - Sixth external fixture started with `fortran-lang/test-drive` cloned at
        `/tmp/freight-test-drive-fixture`. The full 6-file project drove a fix
        for same-name derived types and constructor/generic interfaces: type
        members inside `type color_output` were incorrectly treated as if they
        belonged to a later `interface color_output`, causing false
        "not imported in interface" diagnostics for `type(color_code)`
        components. Interface-scope detection now requires the candidate
        interface range to contain the symbol line. The full fixture now passes
        the real-project differential harness.
      - Seventh external fixture started with `toml-f/toml-f` cloned at
        `/tmp/freight-toml-f-fixture`. The full 89-file project drove fixes for
        public use-associated generic re-export chains (`tomlf` →
        `tomlf_build` → leaf `get_value`/`set_value` interfaces), partial
        public re-export chains where the leaf module is outside the indexed
        slice, declared derived types imported through `use` renames such as
        `toml_lexer => abstract_lexer`, generic overload selection requiring
        all non-optional dummy arguments before choosing a specific procedure,
        `class is(...)` / `type is(...)` select-type guards being parsed as a
        variable/type named `is`, and fortls-style missing direct overrides for
        deferred bindings inherited through a used-module parent. The 40-file,
        80-file, and full-project toml-f differential runs now pass.
      - Eighth external fixture started with
        `jacobwilliams/Fortran-Astrodynamics-Toolkit` cloned at
        `/tmp/freight-fat-fixture`. The full 58-file project drove fixes for
        host type imports inside interface prototype bodies (`import ::
        rk_class` nested under the prototype subroutine scope), and
        fortls-style masking diagnostics for local `parameter` declarations
        that shadow names exported through whole-module re-export aggregators
        (`use fortran_astrodynamics_toolkit` exposing `day2sec`/`sec2day` from
        `conversion_module`). The 30-file and full-project differential runs
        now pass.
      - Ninth external fixture started with `jacobwilliams/bspline-fortran`
        cloned at `/tmp/freight-bspline-fixture`. The full 19-file project
        drove a workspace-level backstop for contained procedure dummies that
        mask ancestor `parameter` declarations, matching fortls diagnostics for
        `test_regrid.f90` (`x`/`y` program parameters vs contained function
        dummies). It also exposed a harness timing issue: Freight can publish
        an early empty diagnostic set for a URI before the slower related
        native diagnostic recomputation arrives, so the project harness default
        `--diagnostic-quiet` is now 2.0 seconds. The full bspline differential
        now passes with the default quiet window.
      - Tenth external fixture started with
        `jacobwilliams/Fortran-CSV-Module` cloned at
        `/tmp/freight-csv-fixture`. The full 8-file project drove fixes for
        statement-form `open(...)` / `close(...)` being misclassified as
        intrinsic procedure calls, and for fortls-style local variables masking
        parameters imported through whole-module `use` statements, including
        public re-export chains. The full CSV differential now passes.
      - Eleventh external fixture started with `urbanjost/M_CLI2` cloned at
        `/tmp/freight-m-cli2-fixture`. The full 61-file project drove parser
        support for semicolon-separated statements on one physical line,
        whitespace-tolerant `module  procedure` / `module  function` /
        `module  subroutine`, compact `doubleprecision` declarations,
        fortls-compatible intrinsic `type(integer)` /
        `type(character(len=:))` wrappers, and order-independent same-module
        callable masking diagnostics while still excluding procedure
        dummies/results. The full M_CLI2 differential now passes.
      - Twelfth external fixture started with `jacobwilliams/roots-fortran`
        cloned at `/tmp/freight-roots-fixture`. The full 4-file project already
        passes the real-project differential, covering another OO/numerical
        library shape without requiring new parser or workspace rules.
      - Thirteenth external fixture started with `modern-fortran/neural-fortran`
        cloned at `/tmp/freight-neural-fixture`. The first 40-file slice drove
        support for `select rank` construct scopes, suppression of function
        result variables in submodule ancestor masking diagnostics, and a fix
        for same-name constructor interfaces colliding with derived-type member
        scopes (`interface conv1d_layer` vs `type conv1d_layer`). Constructor
        prototype dummies such as `filters`, `stride`, and `activation` now stay
        quiet when they share names with type components, while real lexical
        parent masking diagnostics remain intact. Expanding to the full 101-file
        fixture drove typed module-function prototype parsing such as
        `module integer function`, labeled block document symbols, function
        result masking against parent variables (`result(converged)`), and
        host-type import suppression for typed module-procedure prototypes. The
        full neural-fortran differential now passes.
      - Fourteenth external fixture started with
        `jacobwilliams/pyplot-fortran` cloned at
        `/tmp/freight-pyplot-fixture`. The full 5-file project already passes
        the real-project differential, adding preprocessed `.F90` plotting
        module coverage without requiring new parser or workspace rules.
      - Fifteenth external fixture started with
        `jacobwilliams/fortran-search-and-sort` cloned at
        `/tmp/freight-search-sort-fixture`. The full 4-file project already
        passes the real-project differential, adding include-heavy sorting
        module coverage without requiring new parser or workspace rules.
      - Sixteenth external fixture started with `jacobwilliams/quadpack` cloned
        at `/tmp/freight-quadpack-fixture`. The full 13-file project drove a
        diagnostic-boundary fix: workspace diagnostics produced while parsing
        preprocessor-included template text are no longer published against a
        wrapper file when their ranges are outside that wrapper's source. The
        oracle also now filters fortls-only scope/masking noise from
        `#ifndef MOD_INCLUDE` template files. The full quadpack differential now
        passes with a larger request timeout for its large document-symbol sets.
      - Seventeenth external fixture started with
        `jacobwilliams/nlesolver-fortran` cloned at
        `/tmp/freight-nlesolver-fixture`. The full 3-file project already passes
        the real-project differential, adding compact nonlinear-solver and
        sparse-test coverage without requiring new parser or workspace rules.
      - Still needs expansion to larger external projects so parser/indexer edge
        cases are driven by production Fortran code, not only repo examples.
        The harness has bounded request/diagnostic timeout knobs and verbose
        progress markers for locating slow or hanging request phases.

## 2. Language-feature gaps (crate-side)

Parity items fortls has that we don't yet; each is "parse → index → expose
through Workspace queries → regression test":

- [ ] Module symbol caching/incremental reparse — unchanged `Workspace::upsert_file`
      calls now return without reparsing or rebuilding symbol indexes; every real
      text change still reparses the file. Fine now, measure before optimizing
      partial reparse.
- [x] Full intrinsic procedure/module table from fortls' JSON data.
- [x] `associate`, `block`, and `select type` construct scopes.
- [x] Operator interfaces (`interface operator(+)`) and `assignment(=)`.
- [x] Type-bound generic resolution at call sites (overload pick for
      completion detail / signature help).
- [x] Document symbols: expose the full hierarchy already in the model through
      an LSP-shaped `document_symbols()` (verify parity with fortls output).
- [x] Rename (workspace-wide, using the existing references machinery +
      conflict check à la clang-bridge `cb_rename`).
- [x] Semantic tokens (classifier over the indexed symbols; reuse the
      clang-bridge 9-type legend so freight's LSP layer stays uniform).

## 3. Robustness

- [x] Broken/mid-edit source fixtures: unterminated constructs, half-typed
      `use` statements — parser must return a best-effort ParsedFile and
      diagnostics, never panic.
- [x] UTF-16 column encoding test (LSP columns are UTF-16 code units; verify
      multi-byte comment lines don't shift symbol columns).
- [x] Fuzz the free/fixed-form line handler (cargo-fuzz or a property test
      over random continuation/comment mixes).
