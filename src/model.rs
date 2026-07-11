use std::collections::HashMap;
use std::path::PathBuf;

use crate::parser::{is_scope_kind, scope_match_len, word_at_source, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: usize,
    pub character: usize,
}

impl Position {
    pub const fn new(line: usize, character: usize) -> Self {
        Self { line, character }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    pub fn contains(&self, pos: Position) -> bool {
        (self.start.line < pos.line
            || (self.start.line == pos.line && self.start.character <= pos.character))
            && (pos.line < self.end.line
                || (pos.line == self.end.line && pos.character <= self.end.character))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Module,
    Program,
    Submodule,
    Subroutine,
    Function,
    Type,
    Interface,
    Block,
    Associate,
    SelectType,
    Variable,
    Method,
    Use,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
    Default,
}

impl Visibility {
    pub fn label(self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Private => "private",
            Visibility::Default => "default",
        }
    }
}

impl SymbolKind {
    pub fn label(self) -> &'static str {
        match self {
            SymbolKind::Module => "module",
            SymbolKind::Program => "program",
            SymbolKind::Submodule => "submodule",
            SymbolKind::Subroutine => "subroutine",
            SymbolKind::Function => "function",
            SymbolKind::Type => "type",
            SymbolKind::Interface => "interface",
            SymbolKind::Block => "block",
            SymbolKind::Associate => "associate",
            SymbolKind::SelectType => "select type",
            SymbolKind::Variable => "variable",
            SymbolKind::Method => "method",
            SymbolKind::Use => "use",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub file: PathBuf,
    pub range: Range,
    pub selection_range: Range,
    pub scope: Vec<String>,
    pub signature: String,
    pub args: Vec<String>,
    pub documentation: Option<String>,
    pub visibility: Visibility,
    pub type_spec: Option<String>,
    pub attributes: Vec<String>,
    pub result: Option<String>,
    pub is_parameter: bool,
    pub is_external: bool,
    pub extends: Option<String>,
    pub is_abstract: bool,
    pub binding_target: Option<String>,
    pub pass_arg: Option<String>,
    pub is_deferred: bool,
    pub is_module_procedure: bool,
    pub ancestor: Option<String>,
}

impl Symbol {
    pub fn qualified_name(&self) -> String {
        if self.scope.is_empty() {
            self.name.clone()
        } else {
            format!("{}::{}", self.scope.join("::"), self.name)
        }
    }

    pub fn hover_markdown(&self) -> String {
        let lang = "fortran";
        let mut out = format!("```{lang}\n{}\n```", self.signature);
        if !self.scope.is_empty() {
            out.push_str("\n\n");
            out.push_str("scope: `");
            out.push_str(&self.scope.join("::"));
            out.push('`');
        }
        if let Some(docs) = &self.documentation {
            out.push_str("\n\n");
            out.push_str(docs);
        }
        if self.visibility != Visibility::Default {
            out.push_str("\n\n");
            out.push_str("visibility: `");
            out.push_str(self.visibility.label());
            out.push('`');
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseStmt {
    pub module: String,
    pub only: Vec<String>,
    pub renames: Vec<UseRename>,
    pub intrinsic: bool,
    pub file: PathBuf,
    pub range: Range,
    pub scope: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseRename {
    pub local: String,
    pub remote: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    All,
    None,
    Only,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportStmt {
    pub kind: ImportKind,
    pub names: Vec<String>,
    pub file: PathBuf,
    pub range: Range,
    pub scope: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenericBinding {
    pub name: String,
    pub kind: GenericBindingKind,
    pub procedures: Vec<String>,
    pub visibility: Visibility,
    pub file: PathBuf,
    pub range: Range,
    pub scope: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenericBindingKind {
    Named,
    Operator,
    Assignment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilityStmt {
    pub visibility: Visibility,
    pub names: Vec<String>,
    pub file: PathBuf,
    pub range: Range,
    pub scope: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeStmt {
    pub path: String,
    pub file: PathBuf,
    pub range: Range,
    pub scope: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInclude {
    pub include: IncludeStmt,
    pub resolved_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreprocessorKind {
    If,
    Ifdef,
    Ifndef,
    Elif,
    Else,
    Endif,
    Define,
    Undef,
    Include,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreprocessorDirective {
    pub kind: PreprocessorKind,
    pub name: Option<String>,
    pub argument: Option<String>,
    pub file: PathBuf,
    pub range: Range,
    pub scope: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreprocessorRegion {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub file: PathBuf,
    pub range: Range,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeAction {
    pub title: String,
    pub kind: String,
    pub edits: Vec<TextEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticToken {
    pub range: Range,
    pub token_type: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHint {
    pub position: Position,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionRange {
    pub range: Range,
    pub parent: Option<Box<SelectionRange>>,
}

pub mod semantic_token_type {
    pub const NAMESPACE: u32 = 0;
    pub const TYPE: u32 = 1;
    pub const FUNCTION: u32 = 2;
    pub const METHOD: u32 = 3;
    pub const PROPERTY: u32 = 4;
    pub const VARIABLE: u32 = 5;
    pub const PARAMETER: u32 = 6;
    pub const ENUM_MEMBER: u32 = 7;
    pub const MACRO: u32 = 8;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameError {
    UnresolvedSymbol,
    InvalidIdentifier,
    ConflictingSymbol { file: PathBuf, range: Range },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedFile {
    pub path: PathBuf,
    pub source: String,
    pub symbols: Vec<Symbol>,
    pub uses: Vec<UseStmt>,
    pub imports: Vec<ImportStmt>,
    pub generic_bindings: Vec<GenericBinding>,
    pub includes: Vec<IncludeStmt>,
    pub preprocessor: Vec<PreprocessorDirective>,
    pub preprocessor_regions: Vec<PreprocessorRegion>,
    pub preprocessor_definitions: HashMap<String, String>,
    pub visibility: Vec<VisibilityStmt>,
    pub diagnostics: Vec<Diagnostic>,
}

impl ParsedFile {
    pub fn parse(path: impl Into<PathBuf>, source: &str) -> Self {
        Self::parse_with_defines(path, source, &[])
    }

    /// Parse with externally predefined preprocessor macros (the build
    /// system's `-D NAME[=VALUE]` set), so `#ifdef` regions are evaluated the
    /// way the real compilation would see them. A bare name uses the C
    /// convention of defining it to `1`.
    pub fn parse_with_defines(
        path: impl Into<PathBuf>,
        source: &str,
        defines: &[(String, String)],
    ) -> Self {
        Parser::with_defines(path.into(), source, defines).parse()
    }

    pub fn symbol_at(&self, pos: Position) -> Option<&Symbol> {
        let word = word_at_source(&self.source, pos)?;
        let current_scope = self.scope_at(pos);
        if let Some(sym) = self
            .symbols
            .iter()
            .filter(|sym| sym.name.eq_ignore_ascii_case(&word))
            .filter(|sym| sym.selection_range.contains(pos))
            .max_by_key(|sym| sym.scope.len())
        {
            return Some(sym);
        }
        self.symbols
            .iter()
            .filter(|sym| sym.name.eq_ignore_ascii_case(&word))
            .filter_map(|sym| {
                visible_scope_match_len(&current_scope, &sym.scope).map(|len| (len, sym))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, sym)| sym)
    }

    pub fn document_symbols(&self) -> Vec<DocumentSymbol> {
        let mut roots = Vec::new();
        for (idx, sym) in self.symbols.iter().enumerate() {
            if sym.scope.is_empty() {
                roots.push(self.document_symbol_at(idx));
            }
        }
        roots
    }

    pub fn scope_at(&self, pos: Position) -> Vec<String> {
        self.symbols
            .iter()
            .filter(|sym| is_scope_kind(sym.kind) && sym.range.contains(pos))
            .max_by_key(|sym| sym.range.start.line)
            .map(|sym| {
                let mut scope = sym.scope.clone();
                scope.push(sym.name.clone());
                scope
            })
            .unwrap_or_default()
    }

    fn document_symbol_at(&self, idx: usize) -> DocumentSymbol {
        let sym = &self.symbols[idx];
        let qualified = sym.qualified_name();
        let mut children: Vec<_> = self
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, child)| child.scope.join("::").eq_ignore_ascii_case(&qualified))
            .map(|(child_idx, _)| self.document_symbol_at(child_idx))
            .collect();
        children.extend(
            self.generic_bindings
                .iter()
                .filter(|generic| generic.scope.join("::").eq_ignore_ascii_case(&qualified))
                .map(document_symbol_for_generic_binding),
        );
        children.sort_by_key(|child| {
            (
                child.range.start.line,
                child.range.start.character,
                child.name.clone(),
            )
        });
        let symbol = DocumentSymbol {
            name: sym.name.clone(),
            detail: (!sym.signature.is_empty()).then(|| sym.signature.clone()),
            kind: sym.kind,
            range: sym.range.clone(),
            selection_range: sym.selection_range.clone(),
            children,
        };
        if sym.kind == SymbolKind::Submodule {
            if let Some(parent) = nested_submodule_parent_name(&sym.signature) {
                return DocumentSymbol {
                    name: parent,
                    detail: Some(sym.signature.clone()),
                    kind: SymbolKind::Submodule,
                    range: sym.range.clone(),
                    selection_range: sym.selection_range.clone(),
                    children: vec![symbol],
                };
            }
        }
        symbol
    }
}

fn document_symbol_for_generic_binding(generic: &GenericBinding) -> DocumentSymbol {
    DocumentSymbol {
        name: generic.name.clone(),
        detail: Some(format!(
            "generic binding => {}",
            generic.procedures.join(", ")
        )),
        kind: SymbolKind::Method,
        range: generic.range.clone(),
        selection_range: generic.range.clone(),
        children: Vec::new(),
    }
}

fn visible_scope_match_len(current: &[String], candidate: &[String]) -> Option<usize> {
    if candidate.len() > current.len() {
        return None;
    }
    let len = scope_match_len(current, candidate);
    (len == candidate.len()).then_some(len)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSymbol {
    pub name: String,
    pub detail: Option<String>,
    pub kind: SymbolKind,
    pub range: Range,
    pub selection_range: Range,
    pub children: Vec<DocumentSymbol>,
}

fn nested_submodule_parent_name(signature: &str) -> Option<String> {
    let rest = signature.trim_start();
    let keyword = rest.get(.."submodule".len())?;
    if !keyword.eq_ignore_ascii_case("submodule") {
        return None;
    }
    let rest = rest.get("submodule".len()..)?.trim_start();
    let inner = rest.strip_prefix('(')?.split_once(')')?.0;
    let (_, parent) = inner.split_once(':')?;
    let parent = parent.trim();
    (!parent.is_empty()).then(|| parent.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub file: PathBuf,
    pub range: Range,
}
