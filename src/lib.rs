//! Native Rust Fortran language intelligence primitives.
//!
//! This crate is a Rust port of `fortls` concepts. The primary API is the
//! embeddable parser/indexer used by `freight lsp`; the package also ships a
//! small stdio LSP binary for users who want to run the Fortran engine directly.

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
