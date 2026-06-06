# fortran-lsp

Native Rust Fortran language intelligence primitives for `freight lsp`.

This crate is a Rust porting target for the useful parts of `fortls`, but it is
not a standalone subprocess language server. It is meant to be embedded directly
by Freight.

## Current coverage

- Parse free-form Fortran logical lines, including `&` continuations.
- Index modules, programs, submodules, subroutines, functions, interfaces,
  derived types, `use` statements, and basic variable declarations.
- Provide workspace-level primitives for hover markdown, definition lookup,
  completion items, and duplicate-symbol diagnostics.

## Porting references

The initial parser shape mirrors fortls concepts:

- scope stack: modules/programs/subroutines/functions/types
- module/use visibility: `use mod, only: name`
- diagnostics as parser/indexer output, not server output

Known next ports from fortls:

- full fixed-form source handling
- include and preprocessor handling
- intrinsic module and intrinsic symbol tables
- type-bound procedure resolution
- references/rename
- signature help
- richer diagnostics for unresolved use/import/type references

## Assembly LSP note

`asm-lsp` 0.10.1 is a usable Rust library crate. It exports parser and LSP helper
functions such as hover, completion, document symbols, signature help, goto
definition, and references. It also includes full server/config code and several
transitive dependencies. Freight should wrap its parser/helper API behind a thin
native assembly indexer rather than launching the `asm-lsp` binary.
