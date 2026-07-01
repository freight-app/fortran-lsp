//! Native Rust Fortran language intelligence primitives.
//!
//! This crate is the start of a Rust port of `fortls` concepts. It deliberately
//! exposes parser/indexer primitives rather than an LSP transport server so
//! callers such as `freight lsp` can embed it directly.

mod intrinsics;
mod model;
mod parser;
mod workspace;

pub use intrinsics::{IntrinsicCompletion, IntrinsicKind, IntrinsicSymbol};
pub use model::{
    semantic_token_type, CodeAction, Diagnostic, DiagnosticSeverity, DocumentSymbol,
    GenericBinding, GenericBindingKind, ImportKind, ImportStmt, IncludeStmt, InlayHint, Location,
    ParsedFile, Position, PreprocessorDirective, PreprocessorKind, PreprocessorRegion, Range,
    RenameError, ResolvedInclude, SelectionRange, SemanticToken, Symbol, SymbolKind, TextEdit,
    UseRename, UseStmt, Visibility, VisibilityStmt,
};
pub use workspace::{CompletionItem, SignatureHelp, Workspace, WorkspaceConfig};

#[cfg(test)]
mod tests;
