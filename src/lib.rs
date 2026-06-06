//! Native Rust Fortran language intelligence primitives.
//!
//! This crate is the start of a Rust port of `fortls` concepts. It deliberately
//! exposes parser/indexer primitives rather than an LSP transport server so
//! callers such as `freight lsp` can embed it directly.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Module,
    Program,
    Submodule,
    Subroutine,
    Function,
    Type,
    Interface,
    Variable,
    Use,
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
            SymbolKind::Variable => "variable",
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
    pub scope: Vec<String>,
    pub signature: String,
    pub args: Vec<String>,
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
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseStmt {
    pub module: String,
    pub only: Vec<String>,
    pub file: PathBuf,
    pub range: Range,
    pub scope: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: DiagnosticSeverity,
    pub message: String,
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
    pub symbols: Vec<Symbol>,
    pub uses: Vec<UseStmt>,
    pub diagnostics: Vec<Diagnostic>,
}

impl ParsedFile {
    pub fn parse(path: impl Into<PathBuf>, source: &str) -> Self {
        Parser::new(path.into(), source).parse()
    }

    pub fn symbol_at(&self, pos: Position) -> Option<&Symbol> {
        let line = self.source_line(pos.line)?;
        let word = word_at_line(&line, pos.character)?;
        self.symbols.iter().find(|sym| {
            sym.name.eq_ignore_ascii_case(&word)
                || sym
                    .qualified_name()
                    .rsplit("::")
                    .next()
                    .is_some_and(|name| name.eq_ignore_ascii_case(&word))
        })
    }

    fn source_line(&self, line: usize) -> Option<String> {
        self.symbols
            .iter()
            .find(|sym| sym.range.start.line == line)
            .map(|sym| sym.signature.clone())
    }
}

#[derive(Debug, Clone, Default)]
pub struct Workspace {
    files: HashMap<PathBuf, ParsedFile>,
    by_name: HashMap<String, Vec<(PathBuf, usize)>>,
}

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert_file(&mut self, path: impl Into<PathBuf>, source: &str) {
        let parsed = ParsedFile::parse(path.into(), source);
        self.remove_file(&parsed.path);
        let path = parsed.path.clone();
        for (idx, sym) in parsed.symbols.iter().enumerate() {
            self.by_name
                .entry(sym.name.to_ascii_lowercase())
                .or_default()
                .push((path.clone(), idx));
        }
        self.files.insert(path, parsed);
    }

    pub fn remove_file(&mut self, path: &Path) {
        if let Some(old) = self.files.remove(path) {
            for sym in old.symbols {
                if let Some(entries) = self.by_name.get_mut(&sym.name.to_ascii_lowercase()) {
                    entries.retain(|(p, _)| p != path);
                }
            }
        }
    }

    pub fn file(&self, path: &Path) -> Option<&ParsedFile> {
        self.files.get(path)
    }

    pub fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        self.files
            .get(path)
            .map(|f| f.diagnostics.clone())
            .unwrap_or_default()
    }

    pub fn hover(&self, path: &Path, pos: Position, source: &str) -> Option<String> {
        let word = word_at_source(source, pos)?;
        self.find_visible_symbol(path, &word)
            .map(Symbol::hover_markdown)
    }

    pub fn definition(&self, path: &Path, pos: Position, source: &str) -> Option<&Symbol> {
        let word = word_at_source(source, pos)?;
        self.find_visible_symbol(path, &word)
    }

    pub fn completions(&self, path: &Path, prefix: &str) -> Vec<CompletionItem> {
        let prefix = prefix.to_ascii_lowercase();
        let mut items = BTreeMap::new();
        if let Some(file) = self.files.get(path) {
            for sym in &file.symbols {
                if sym.name.to_ascii_lowercase().starts_with(&prefix) {
                    items.insert(sym.name.clone(), CompletionItem::from_symbol(sym));
                }
            }
        }
        for entries in self.by_name.values() {
            for (file, idx) in entries {
                let Some(sym) = self.files.get(file).and_then(|f| f.symbols.get(*idx)) else {
                    continue;
                };
                if sym.name.to_ascii_lowercase().starts_with(&prefix) {
                    items.insert(sym.name.clone(), CompletionItem::from_symbol(sym));
                }
            }
        }
        items.into_values().collect()
    }

    fn find_visible_symbol(&self, path: &Path, name: &str) -> Option<&Symbol> {
        if let Some(file) = self.files.get(path) {
            if let Some(sym) = file
                .symbols
                .iter()
                .find(|sym| sym.name.eq_ignore_ascii_case(name))
            {
                return Some(sym);
            }
            for use_stmt in &file.uses {
                if !use_stmt.only.is_empty()
                    && !use_stmt
                        .only
                        .iter()
                        .any(|item| item.eq_ignore_ascii_case(name))
                {
                    continue;
                }
                if let Some(sym) = self
                    .by_name
                    .get(&name.to_ascii_lowercase())
                    .into_iter()
                    .flatten()
                    .filter_map(|(p, idx)| self.files.get(p).and_then(|f| f.symbols.get(*idx)))
                    .find(|sym| {
                        sym.scope
                            .first()
                            .is_some_and(|scope| scope.eq_ignore_ascii_case(&use_stmt.module))
                            || sym.name.eq_ignore_ascii_case(&use_stmt.module)
                    })
                {
                    return Some(sym);
                }
            }
        }
        self.by_name
            .get(&name.to_ascii_lowercase())
            .and_then(|entries| entries.first())
            .and_then(|(p, idx)| self.files.get(p).and_then(|f| f.symbols.get(*idx)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub detail: String,
    pub kind: SymbolKind,
}

impl CompletionItem {
    fn from_symbol(sym: &Symbol) -> Self {
        Self {
            label: sym.name.clone(),
            detail: sym.signature.clone(),
            kind: sym.kind,
        }
    }
}

struct Parser<'a> {
    path: PathBuf,
    source: &'a str,
    parsed: ParsedFile,
    scopes: Vec<ScopeFrame>,
}

#[derive(Debug, Clone)]
struct ScopeFrame {
    name: String,
    kind: SymbolKind,
    symbol_idx: usize,
}

impl<'a> Parser<'a> {
    fn new(path: PathBuf, source: &'a str) -> Self {
        Self {
            parsed: ParsedFile {
                path: path.clone(),
                ..ParsedFile::default()
            },
            path,
            source,
            scopes: Vec::new(),
        }
    }

    fn parse(mut self) -> ParsedFile {
        for (line_no, raw_line) in logical_lines(self.source) {
            let code = strip_inline_comment(&raw_line);
            let code = code.trim();
            if code.is_empty() {
                continue;
            }
            let lower = code.to_ascii_lowercase();
            if lower.starts_with("end ") || lower == "end" {
                self.close_scope(line_no, &lower);
                continue;
            }
            if lower == "contains" {
                continue;
            }
            if let Some(stmt) = parse_use(code, line_no, &self.path, &self.current_scope()) {
                self.parsed.uses.push(stmt);
                continue;
            }
            if let Some((kind, name, args, signature)) = parse_scope_start(code) {
                self.open_scope(line_no, kind, name, args, signature);
                continue;
            }
            if let Some(vars) = parse_variables(code, line_no, &self.path, &self.current_scope()) {
                self.parsed.symbols.extend(vars);
            }
        }
        while let Some(scope) = self.scopes.pop() {
            if let Some(sym) = self.parsed.symbols.get_mut(scope.symbol_idx) {
                sym.range.end = Position::new(self.source.lines().count().saturating_sub(1), 0);
            }
        }
        self.add_duplicate_diagnostics();
        self.parsed
    }

    fn open_scope(
        &mut self,
        line: usize,
        kind: SymbolKind,
        name: String,
        args: Vec<String>,
        signature: String,
    ) {
        let scope = self.current_scope();
        let col = self
            .source
            .lines()
            .nth(line)
            .and_then(|l| find_ci(l, &name))
            .unwrap_or(0);
        let idx = self.parsed.symbols.len();
        self.parsed.symbols.push(Symbol {
            name: name.clone(),
            kind,
            file: self.path.clone(),
            range: Range {
                start: Position::new(line, col),
                end: Position::new(line, col + name.len()),
            },
            scope,
            signature,
            args,
        });
        self.scopes.push(ScopeFrame {
            name,
            kind,
            symbol_idx: idx,
        });
    }

    fn close_scope(&mut self, line: usize, lower: &str) {
        let target_kind = lower.split_whitespace().nth(1).and_then(end_kind);
        if let Some(pos) = self
            .scopes
            .iter()
            .rposition(|scope| target_kind.is_none_or(|kind| scope.kind == kind))
        {
            let closing = self.scopes.split_off(pos);
            for scope in closing.into_iter().rev() {
                if let Some(sym) = self.parsed.symbols.get_mut(scope.symbol_idx) {
                    sym.range.end = Position::new(line, 0);
                }
            }
        }
    }

    fn current_scope(&self) -> Vec<String> {
        self.scopes.iter().map(|scope| scope.name.clone()).collect()
    }

    fn add_duplicate_diagnostics(&mut self) {
        let mut seen: HashMap<(Vec<String>, String), Position> = HashMap::new();
        for sym in &self.parsed.symbols {
            let key = (sym.scope.clone(), sym.name.to_ascii_lowercase());
            if let Some(first) = seen.get(&key) {
                self.parsed.diagnostics.push(Diagnostic {
                    range: sym.range.clone(),
                    severity: DiagnosticSeverity::Error,
                    message: format!(
                        "symbol `{}` is already defined in this scope at line {}",
                        sym.name,
                        first.line + 1
                    ),
                });
            } else {
                seen.insert(key, sym.range.start);
            }
        }
    }
}

fn logical_lines(source: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut start = 0;
    for (idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_end();
        let continued = trimmed.ends_with('&');
        let part = trimmed.trim_end_matches('&').trim_end();
        if current.is_empty() {
            start = idx;
        } else {
            current.push(' ');
        }
        current.push_str(part.trim_start_matches('&').trim_start());
        if !continued {
            out.push((start, current.trim().to_string()));
            current.clear();
        }
    }
    if !current.is_empty() {
        out.push((start, current.trim().to_string()));
    }
    out
}

fn parse_scope_start(code: &str) -> Option<(SymbolKind, String, Vec<String>, String)> {
    let lower = code.to_ascii_lowercase();
    if lower.starts_with("module procedure") {
        return None;
    }
    for (keyword, kind) in [
        ("module", SymbolKind::Module),
        ("program", SymbolKind::Program),
        ("submodule", SymbolKind::Submodule),
        ("subroutine", SymbolKind::Subroutine),
        ("function", SymbolKind::Function),
        ("interface", SymbolKind::Interface),
    ] {
        if let Some(rest) = after_keyword(code, keyword) {
            let name = first_ident(rest)?;
            let args = arg_list(rest).unwrap_or_default();
            return Some((kind, name.to_string(), args, code.trim().to_string()));
        }
    }
    if lower.starts_with("type") && lower.contains("::") {
        let name = code.split("::").nth(1).and_then(first_ident)?;
        return Some((
            SymbolKind::Type,
            name.to_string(),
            Vec::new(),
            code.trim().to_string(),
        ));
    }
    None
}

fn parse_use(code: &str, line: usize, file: &Path, scope: &[String]) -> Option<UseStmt> {
    let rest = after_keyword(code, "use")?;
    let mut rest = rest.trim_start();
    if rest.to_ascii_lowercase().starts_with(", intrinsic") {
        rest = rest.split_once("::").map(|(_, rhs)| rhs).unwrap_or(rest);
    }
    let module = first_ident(rest)?.to_string();
    let only = rest
        .to_ascii_lowercase()
        .find("only")
        .and_then(|idx| rest[idx..].split_once(':').map(|(_, rhs)| rhs))
        .map(split_names)
        .unwrap_or_default();
    let col = find_ci(code, &module).unwrap_or(0);
    Some(UseStmt {
        module,
        only,
        file: file.to_path_buf(),
        range: Range {
            start: Position::new(line, col),
            end: Position::new(line, col + rest.len()),
        },
        scope: scope.to_vec(),
    })
}

fn parse_variables(code: &str, line: usize, file: &Path, scope: &[String]) -> Option<Vec<Symbol>> {
    let (lhs, rhs) = code.split_once("::")?;
    let type_name = first_ident(lhs.trim())?;
    if !matches!(
        type_name.to_ascii_lowercase().as_str(),
        "integer"
            | "real"
            | "double"
            | "complex"
            | "character"
            | "logical"
            | "type"
            | "class"
            | "procedure"
            | "external"
    ) {
        return None;
    }
    let mut symbols = Vec::new();
    for name in split_names(rhs) {
        let col = find_ci(code, &name).unwrap_or(0);
        symbols.push(Symbol {
            name: name.clone(),
            kind: SymbolKind::Variable,
            file: file.to_path_buf(),
            range: Range {
                start: Position::new(line, col),
                end: Position::new(line, col + name.len()),
            },
            scope: scope.to_vec(),
            signature: format!("{} :: {}", lhs.trim(), name),
            args: Vec::new(),
        });
    }
    Some(symbols)
}

fn split_names(s: &str) -> Vec<String> {
    s.split(',')
        .filter_map(|item| first_ident(item.trim()))
        .map(ToString::to_string)
        .collect()
}

fn after_keyword<'a>(code: &'a str, keyword: &str) -> Option<&'a str> {
    let trimmed = code.trim_start();
    let prefix = trimmed.get(..keyword.len())?;
    if !prefix.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = &trimmed[keyword.len()..];
    if rest
        .chars()
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(rest)
}

fn first_ident(s: &str) -> Option<&str> {
    let start = s.find(|ch: char| ch == '_' || ch.is_ascii_alphabetic())?;
    let tail = &s[start..];
    let end = tail
        .find(|ch: char| !(ch == '_' || ch.is_ascii_alphanumeric()))
        .unwrap_or(tail.len());
    Some(&tail[..end])
}

fn arg_list(s: &str) -> Option<Vec<String>> {
    let start = s.find('(')?;
    let end = s[start + 1..].find(')')? + start + 1;
    Some(split_names(&s[start + 1..end]))
}

fn end_kind(word: &str) -> Option<SymbolKind> {
    match word {
        "module" => Some(SymbolKind::Module),
        "program" => Some(SymbolKind::Program),
        "submodule" => Some(SymbolKind::Submodule),
        "subroutine" => Some(SymbolKind::Subroutine),
        "function" => Some(SymbolKind::Function),
        "type" => Some(SymbolKind::Type),
        "interface" => Some(SymbolKind::Interface),
        _ => None,
    }
}

fn strip_inline_comment(line: &str) -> String {
    let mut single = false;
    let mut double = false;
    for (idx, ch) in line.char_indices() {
        match ch {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '!' if !single && !double => return line[..idx].to_string(),
            _ => {}
        }
    }
    line.to_string()
}

fn word_at_source(source: &str, pos: Position) -> Option<String> {
    let line = source.lines().nth(pos.line)?;
    word_at_line(line, pos.character)
}

fn word_at_line(line: &str, character: usize) -> Option<String> {
    let bytes = line.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut idx = character.min(bytes.len().saturating_sub(1));
    if !is_ident(bytes[idx] as char) && idx > 0 {
        idx -= 1;
    }
    if !is_ident(bytes[idx] as char) {
        return None;
    }
    let mut start = idx;
    while start > 0 && is_ident(bytes[start - 1] as char) {
        start -= 1;
    }
    let mut end = idx + 1;
    while end < bytes.len() && is_ident(bytes[end] as char) {
        end += 1;
    }
    Some(line[start..end].to_string())
}

fn is_ident(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .to_ascii_lowercase()
        .find(&needle.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modules_subroutines_functions_and_vars() {
        let src = r#"
module math
  implicit none
  integer, parameter :: rk = 8
contains
  subroutine axpy(a, x, y)
    real, intent(in) :: a
    real, intent(in) :: x
    real, intent(inout) :: y
  end subroutine axpy
end module math
"#;
        let parsed = ParsedFile::parse("math.f90", src);
        let names: Vec<_> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"math"));
        assert!(names.contains(&"axpy"));
        assert!(names.contains(&"rk"));
        assert!(names.contains(&"a"));
        let axpy = parsed.symbols.iter().find(|s| s.name == "axpy").unwrap();
        assert_eq!(axpy.kind, SymbolKind::Subroutine);
        assert_eq!(axpy.args, vec!["a", "x", "y"]);
        assert_eq!(axpy.scope, vec!["math"]);
    }

    #[test]
    fn workspace_resolves_use_only_definition() {
        let mut ws = Workspace::new();
        ws.upsert_file(
            PathBuf::from("math.f90"),
            "module math\ncontains\nsubroutine axpy()\nend subroutine\nend module",
        );
        let app = "program app\nuse math, only: axpy\ncall axpy()\nend program";
        ws.upsert_file(PathBuf::from("app.f90"), app);
        let sym = ws
            .definition(Path::new("app.f90"), Position::new(2, 6), app)
            .unwrap();
        assert_eq!(sym.name, "axpy");
        assert_eq!(sym.scope, vec!["math"]);
    }

    #[test]
    fn reports_duplicate_symbols_in_scope() {
        let parsed = ParsedFile::parse(
            "dup.f90",
            "subroutine s()\ninteger :: x\nreal :: x\nend subroutine",
        );
        assert_eq!(parsed.diagnostics.len(), 1);
        assert!(parsed.diagnostics[0].message.contains("already defined"));
    }

    #[test]
    fn handles_free_form_continuations() {
        let parsed = ParsedFile::parse(
            "cont.f90",
            "subroutine long(&\n  a, b)\ninteger :: a\nend subroutine",
        );
        let sub = parsed.symbols.iter().find(|s| s.name == "long").unwrap();
        assert_eq!(sub.args, vec!["a", "b"]);
    }
}
