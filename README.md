# fortran-lsp

Native Rust Fortran language intelligence primitives for `freight lsp`.

This crate is a Rust porting target for the useful parts of `fortls`, but it is
not a standalone subprocess language server. It is meant to be embedded directly
by Freight.

## Source layout

- `model.rs` — public data types: positions, ranges, symbols, parsed files,
  diagnostics, imports, includes, and document symbols.
- `intrinsics.rs` — built-in intrinsic procedure/module table and helpers.
- `parser.rs` — free/fixed-form line handling and statement parsers.
- `workspace.rs` — cross-file index, hover, definition, completions, signature
  help, references, workspace symbols, selection ranges, and semantic
  diagnostics.
- `tests.rs` — parser/workspace regression tests.

## Current coverage

- Parse free-form Fortran logical lines, including `&` continuations.
- Parse fixed-form Fortran files (`.f`, `.for`, `.ftn`, `.f77`) with column-six
  continuations.
- Cover free/fixed-form continuation and comment mixes with deterministic
  property-style parser tests.
- Index modules, programs, submodules, subroutines, functions, interfaces,
  derived types, `use` statements, and basic variable declarations.
- Index legacy constructs: `COMMON` block members and blank COMMON,
  `NAMELIST` group names, and `ENTRY` points (as siblings of the enclosing
  procedure) — without duplicate-symbol false positives when members are also
  declared with explicit types.
- Accept externally predefined preprocessor macros (a build system's `-D`
  set) via `Workspace::set_predefined_macros` / `ParsedFile::parse_with_defines`,
  so `#ifdef` regions match the real compilation; changing the set reparses
  the workspace.
- Skip fixed-form comment cards in call diagnostics and inlay hints, so
  netlib-style prologues (`C  CALL DINTDY(,,,,,)`) produce no false errors.
- Parse large legacy files in linear time (per-line interface state is
  computed once; the parent-masking pass prefilters by name), and support
  parallel whole-workspace parsing via `Workspace::upsert_parsed`.
- Track submodule ancestor modules and link `module procedure` implementations
  to their ancestor module interface prototypes for hover and definition.
- Treat unresolved submodule ancestors as partial-index state to avoid cascaded
  diagnostics; once the ancestor module is indexed, report `module procedure`
  implementations without a matching ancestor interface prototype.
- Resolve declared types in submodule implementations through ancestor module
  types, including private helper types, and through unresolved ancestor `use`
  imports without cascading extra type errors.
- Diagnose unresolved kind selector names in declarations such as
  `integer(kind=int_index)`, once per affected declared object, while
  suppressing cascades through unresolved direct uses, ancestor-module uses, and
  partially indexed re-export modules.
- Track `public` / `private` visibility statements, default-private modules,
  declaration attributes, parameters, externals, and function `result(...)`
  names.
- Parse type-prefixed functions such as `logical function solve2(...)` and
  `real(dp) function norm(...)`.
- Track interface `import` statements, generic type-bound bindings, derived-type
  `extends(...)`, abstract types, deferred type-bound procedures, `pass(...)`,
  and binding targets (`procedure :: name => impl`), while distinguishing
  procedure dummy declarations from type-bound procedure bindings.
- Track standalone `interface operator(...)` / `interface assignment(=)` scopes
  and type-bound `generic :: operator(...) => proc` / `assignment(=) => proc`
  bindings.
- Accept multi-word procedure keywords with extra whitespace such as
  `module  procedure`.
- Treat public interface prototypes in default-private modules as module
  exports for `use module, only: ...` diagnostics, completion, and lookup.
- Accept host-associated types in module procedure prototypes inside named
  interfaces such as `interface operator(+)`, including prefixed and typed
  forms like `pure module function` and `module integer function`.
- Propagate types imported by an ancestor module's `use` statements into
  submodule implementations, matching host association for submodules.
- Accept interface `import` names inside submodules when provided by the
  ancestor module or by modules used from that ancestor.
- Diagnose submodule locals that mask named generic interfaces from the
  ancestor module, matching fortls for cases such as `elapsed`, `is_running`,
  and `wait`.
- Match fortls' submodule diagnostics for type-bound result names, selected
  repeated clock locals, and named-interface function dummy collisions, while
  suppressing explicit function result variables in ancestor masking checks.
- Warn when contained function result names mask parent variables, matching
  fortls for `result(name)` without a separate result declaration.
- Preserve generic specifiers such as `operator(+)` and `operator(//)` in
  `public` and `use, only:` lists, including continued lines.
- Parse call arguments with nested array constructors so `[a, b]` stays one
  positional argument for diagnostics, signature help, and inlay hints, and so
  comparison operators such as `stride == 0` are not mistaken for keyword
  arguments.
- Index legacy derived-type definitions written as `type name`, not only
  `type :: name`, and keep their component declarations scoped to the type.
- Treat standalone `interface` symbols as generic/interface markers for
  duplicate detection, so constructor generics like `interface type_name` and
  named interfaces wrapping same-named module procedures do not collide with
  the type or procedure symbol.
- Resolve type-bound procedure targets declared as module procedure prototypes
  in anonymous or named host interface blocks, including typed prototypes such
  as `logical module function`, and distinguish procedure pointer components in
  a type's data part from type-bound procedure bindings after `contains`.
- Suppress cascaded interface `import` diagnostics when the imported host name
  may come from an unresolved `use` module; the unresolved module diagnostic is
  reported once, matching fortls behavior on partial project indexes.
- Suppress precise `use module, only: name` export diagnostics when the target
  module is only partially indexed because its own dependencies are unresolved.
- Report whole-module `use module` imports of partially indexed modules as
  unresolved when the imported module has its own local API and the use site is
  module/submodule-like, matching fortls' primary diagnostic for incomplete
  dependency graphs such as `stdlib_hashmap_wrappers`, while allowing program
  imports and pure re-export aggregators such as `stdlib_sparse` and preserving
  `only:` cascade suppression.
- Track `block`, `associate`, `select type`, `select rank`, and `select case`
  construct scopes, including labeled constructs, local
  `associate(name => expr)` aliases, and construct-local shadowing.
- Preserve named construct labels such as `training1: block` in document and
  workspace symbols, while keeping synthetic names for unlabeled constructs.
- Preserve free-form continued declarations when comment-only lines appear
  between continuation lines, while keeping full-line comments from starting
  fake continuations.
- Split semicolon-separated statements on one physical line while preserving
  source positions, including one-line module functions.
- Provide built-in intrinsic procedure/module symbols for hover, completion,
  signature help, and `use, intrinsic` diagnostics, including `use,intrinsic`
  syntax without whitespace after the comma.
- Treat all-optional standard intrinsic subroutines such as `date_and_time`,
  `random_seed`, and `system_clock` as optional for call diagnostics, even when
  the vendored fortls table lists positional argument names.
- Accept standard optional `flush` keyword arguments `iostat` and `iomsg` even
  though the vendored fortls intrinsic table only lists `unit`.
- Accept standard optional `merge` keyword argument `mask` and positional mask
  expressions.
- Treat statement-form `open(...)` and `close(...)` as I/O statements, not
  procedure calls for argument diagnostics.
- Record Fortran `include` and preprocessor `#include` statements for callers to
  resolve.
- Resolve include statements against the including file's directory and
  caller-configured include roots, including include files that exist on disk
  but have not been opened or indexed yet; expose resolved include metadata,
  hover text, and unresolved-include diagnostics.
- Make top-level symbols from resolved include files visible to the including
  file for hover, definition, completion, and references. Nested includes are
  traversed recursively with cycle protection.
- Graft included symbols into the scope where the include statement appears, so
  internal subroutine/block includes do not leak declarations to unrelated
  scopes.
- Record preprocessor directives (`#if`, `#ifdef`, `#ifndef`, `#elif`, `#else`,
  `#endif`, `#define`, `#undef`, `#include`), macro definitions, and conditional
  regions. Unbalanced conditionals are reported as diagnostics.
- Provide fortls-style hover summaries, definition locations, completions,
  references, rename edits, and semantic tokens for active preprocessor
  `#define` symbols, including object-like and function-like macro replacements.
- Evaluate simple preprocessor conditionals (`defined`, identifiers, integer
  constants, `!`, `&&`, `||`, `==`, `!=`) so inactive branches are skipped during
  parsing and inactive `#include` directives are ignored.
- Return best-effort symbols and diagnostics for mid-edit sources, including
  unterminated scopes and half-typed `use` statements.
- Report `use` statements that appear after an `implicit` statement in the same
  scope.
- Report invalid `contains` placement, duplicate `contains` statements, and
  subroutine/function definitions that appear before `contains`.
- Report `implicit` statements without an enclosing scope and `import`
  statements outside interface blocks.
- Report fortls-style procedure argument declaration issues: `intent(...)`
  declarations for names outside the argument list, and undeclared dummy
  arguments when `implicit none` is active in the procedure or an enclosing
  scope.
- Warn when variables in subroutine, function, or block scopes mask variables
  from parent scopes, while skipping derived-type members. Also warn for
  fortls-style procedure/function-result names that mask derived-type members
  or type-bound methods in the same module, while suppressing direct
  type-bound procedure targets and abstract-interface result prototypes.
- Avoid treating same-named generic constructor interfaces as lexical type
  scopes, so constructor prototype dummies do not falsely mask derived-type
  components with the same name.
- Warn when local variables mask parameters imported through whole-module `use`
  statements, including public re-export chains.
- Diagnose non-abstract derived types that inherit deferred type-bound
  procedures without providing a concrete override.
- Provide quick-fix text edits for missing inherited deferred type-bound
  procedures, ready for the embedding LSP layer to expose as code actions.
- Report non-deferred type-bound procedure declarations whose concrete target
  subroutine/function cannot be resolved.
- Report concrete type-bound procedure targets that do not match an explicit
  `procedure(interface_name)` prototype's procedure kind, argument list,
  passed-object type relationship, non-passed dummy argument type, or key dummy
  attributes.
- Report generic type-bound bindings that reference unknown type-bound
  procedures.
- Report standalone generic interfaces whose `module procedure` links do not
  resolve to concrete procedures in the enclosing scope.
- Link type-bound procedure declarations to their concrete subroutine/function
  implementation for hover, definition, and signature help, including hiding the
  implicit passed-object argument unless `nopass` is present.
- Resolve fortls-style implementation locations from type-bound procedure
  declarations/member calls to their concrete targets, and from module
  procedure interface prototypes to submodule implementations.
- Track keyword arguments in signature help so named calls such as
  `foo(y=value)` activate the matching dummy argument.
- Provide parameter-name inlay hints for positional arguments in resolvable
  procedure, intrinsic, and type-bound method calls.
- Report bad arguments in resolvable procedure, intrinsic, and type-bound
  method calls, including too many positional arguments, unknown keywords, and
  repeated keywords, while respecting optional dummy arguments.
- Suppress call-argument diagnostics when a visible non-callable symbol is
  being indexed as an array reference or when an unresolved import may provide
  the callable.
- Report missing required arguments in resolvable procedure, intrinsic, and
  type-bound method calls.
- Resolve type-bound generic member calls such as `obj%render(...)` to the
  matching bound procedure for signature help, using the call argument count
  when multiple procedures share a generic.
- Resolve standalone generic interface calls such as `set(...)` through linked
  `module procedure` implementations for signature help, parameter inlay hints,
  and call-argument diagnostics.
- Resolve object member calls such as `obj%method(...)` through the receiver's
  declared `type(...)` / `class(...)`, including inherited type-bound methods.
- Resolve `class(parent)` member calls to a unique concrete descendant override
  when the declared parent binding is deferred and the workspace has exactly one
  possible implementation.
- Provide workspace-level primitives for hover markdown, definition lookup,
  implementation lookup, completion items, document symbols, workspace symbol
  search, signature help, references, selection ranges, and diagnostics.
- Include type-bound generic bindings such as `generic :: get => get_item` in
  hierarchical document symbols under their containing derived type.
- Provide fortls-style hover type summaries for integer, real, logical, and
  character literal constants.
- Provide workspace-wide rename text edits with invalid-identifier and
  same-scope conflict checks.
- Provide semantic tokens and LSP delta-encoded semantic-token data using
  Freight's shared clang-bridge legend.
- Accept and return UTF-16 column offsets for LSP-facing cursor queries and
  locations.
- Attach `!!` / `!>` doc comments to the next indexed scope.
- Track `use` renames (`local => remote`) for user and intrinsic modules in
  definition lookup, hover, completion, and diagnostics.
- Provide cursor-position-aware completions that respect local scope, scoped
  includes, module export visibility, and `use only` lists.
- Complete module names in `use` statements and public module members after
  `use module, only:`.
- Complete host variables and derived types inside interface `import`
  statements.
- Complete local variables, derived types, subroutines, and functions inside
  visibility statements such as `public :: ...` and `private :: ...`.
- Complete scope-sensitive declaration attributes such as `parameter`,
  `optional`, `intent(...)`, `deferred`, `allocatable`, `pointer`, and
  visibility keywords before `::` in declarations.
- Complete only variables inside declaration variable lists after `::`,
  excluding procedures and types.
- Complete Fortran statement keywords at the first word of a line while keeping
  normal visible-symbol completions available.
- Suppress completions on scope declarations and `end ...` statements, matching
  fortls' skip contexts.
- Complete local concrete procedures after `module procedure ...` links inside
  generic interfaces.
- Complete visible derived types inside `type(...)`, `class(...)`, and
  `extends(...)` contexts.
- Complete abstract-interface procedure prototypes inside `procedure(...)`
  declarations, including prototypes imported with `use, only:`.
- Complete callable procedures, generic interfaces, and intrinsic subroutines
  after bare `call` statements while excluding variables, types, and functions.
- Complete type-bound methods and generic bindings after member access such as
  `obj%ren`, including inherited public bindings and excluding private ones.
- Report unresolved non-intrinsic `use` modules and missing `only:` imports once
  the workspace has been indexed.
- Optionally report fortls-style line-length warnings through
  `Workspace::set_line_length_limits`, with separate limits for code lines and
  comment lines. Freight wires these from `[language.fortran]` string options
  `max_line_length` and `max_comment_line_length`.
- Report `import, only:` names inside interfaces that do not exist in the host
  scope.
- Accept interface `import, only:` names that are host-associated through a
  containing scope's `use` statement, including intrinsic modules such as
  `iso_fortran_env` and user modules that re-export imported names.
- Report unresolved declared derived types in `type(name)` / `class(name)`
  variables, including support for host-associated and imported module types
  while accepting unlimited polymorphic `class(*)` and derived types from
  intrinsic modules such as `iso_c_binding`.
- Suppress cascaded unresolved-type diagnostics for names that are imported
  from an unresolved `use, only:` module, matching fortls' primary-error style.
- Report host derived types used inside non-abstract interface blocks without a
  matching `import`, while accepting `import, only:` and interface-local types.
- Keep declaration splitting at top-level commas only, so array constructors in
  parameter initializers do not create duplicate symbols.
- Index legacy declarations without `::`, such as `complex(rk) f_hat(0:n)`.
- Accept compact `doubleprecision` declarations.
- Accept fortls-compatible intrinsic wrappers such as `type(integer)` and
  `type(character(len=:))` as intrinsic declarations, not unresolved derived
  types.
- Avoid false argument diagnostics for variadic/reduction intrinsic forms such
  as `max(a, b, c)`, `all(mask)`, and stdlib array/generic contexts that shadow
  intrinsic names.

## Porting references

The initial parser shape mirrors fortls concepts:

- scope stack: modules/programs/subroutines/functions/types
- module/use visibility: `use mod, only: name`
- diagnostics as parser/indexer output, not server output

Known next hardening work lives in `TODO.md` and is tracked one point at a
time. The current focus areas are project-mode request parity, remaining
C-preprocessor edge cases, procedure pointer/callback modelling, richer generic
overload selection, editor-surface audits, and measured incremental dependency
invalidation.

## Assembly LSP note

`asm-lsp` 0.10.1 is a usable Rust library crate. It exports parser and LSP helper
functions such as hover, completion, document symbols, signature help, goto
definition, and references. It also includes full server/config code and several
transitive dependencies. Freight should wrap its parser/helper API behind a thin
native assembly indexer rather than launching the `asm-lsp` binary.
