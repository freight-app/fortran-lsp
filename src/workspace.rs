use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::intrinsics;
use crate::model::{
    semantic_token_type, CodeAction, Diagnostic, DiagnosticSeverity, DocumentSymbol, ImportKind,
    ImportStmt, IncludeStmt, InlayHint, Location, ParsedFile, Position, PreprocessorKind, Range,
    RenameError, ResolvedInclude, SelectionRange, SemanticToken, Symbol, SymbolKind, TextEdit,
    UseStmt, Visibility,
};
use crate::parser::{
    call_context, identifier_occurrences, is_fixed_comment, is_fixed_form_path, is_scope_kind,
    member_access_at_source, scope_match_len, scopes_equal_case_insensitive, word_at_source,
    word_range_at_source,
};

#[derive(Debug, Clone, Default)]
pub struct Workspace {
    files: HashMap<PathBuf, ParsedFile>,
    by_name: HashMap<String, Vec<(PathBuf, usize)>>,
    file_symbol_index: HashMap<PathBuf, Vec<(String, usize)>>,
    include_roots: Vec<PathBuf>,
    config: WorkspaceConfig,
    predefined_macros: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkspaceConfig {
    pub max_line_length: Option<usize>,
    pub max_comment_line_length: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SymbolKey {
    file: PathBuf,
    scope: Vec<String>,
    name: String,
    kind: SymbolKind,
}

impl SymbolKey {
    fn from_symbol(sym: &Symbol) -> Self {
        Self {
            file: sym.file.clone(),
            scope: sym
                .scope
                .iter()
                .map(|part| part.to_ascii_lowercase())
                .collect(),
            name: sym.name.to_ascii_lowercase(),
            kind: sym.kind,
        }
    }
}

fn symbol_index(file: &ParsedFile) -> Vec<(String, usize)> {
    file.symbols
        .iter()
        .enumerate()
        .map(|(idx, sym)| (sym.name.to_ascii_lowercase(), idx))
        .collect()
}

#[derive(Debug, Clone)]
struct IncludedSymbol<'a> {
    symbol: &'a Symbol,
    effective_scope: Vec<String>,
}

#[derive(Debug, Clone)]
struct CallParameter {
    label: String,
    name: String,
    optional: bool,
}

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert_file(&mut self, path: impl Into<PathBuf>, source: &str) -> bool {
        let path = path.into();
        if self
            .files
            .get(&path)
            .is_some_and(|file| file.source == source)
        {
            return false;
        }
        self.upsert_parsed(ParsedFile::parse_with_defines(
            path,
            source,
            &self.predefined_macros,
        ));
        true
    }

    /// Predefined preprocessor macros (the build's `-D NAME[=VALUE]` set) used
    /// for every parse, so `#ifdef` regions match the real compilation. A
    /// change reparses all indexed files. Callers parsing in parallel must
    /// pass the same set to [`ParsedFile::parse_with_defines`].
    pub fn set_predefined_macros(&mut self, macros: Vec<(String, String)>) {
        if self.predefined_macros == macros {
            return;
        }
        self.predefined_macros = macros;
        let sources: Vec<(PathBuf, String)> = self
            .files
            .iter()
            .map(|(path, file)| (path.clone(), file.source.clone()))
            .collect();
        for (path, source) in sources {
            self.upsert_parsed(ParsedFile::parse_with_defines(
                path,
                &source,
                &self.predefined_macros,
            ));
        }
    }

    pub fn predefined_macros(&self) -> &[(String, String)] {
        &self.predefined_macros
    }

    /// Insert an already-parsed file, replacing any previous version.
    /// `ParsedFile::parse` is pure, so callers indexing a whole workspace can
    /// parse many files in parallel and insert the results sequentially here.
    pub fn upsert_parsed(&mut self, parsed: ParsedFile) {
        let path = parsed.path.clone();
        let next_index = symbol_index(&parsed);
        if self
            .file_symbol_index
            .get(&path)
            .is_some_and(|old| old == &next_index)
        {
            self.files.insert(path, parsed);
            return;
        }
        self.remove_file(&path);
        for (name, idx) in &next_index {
            self.by_name
                .entry(name.clone())
                .or_default()
                .push((path.clone(), *idx));
        }
        self.file_symbol_index.insert(path.clone(), next_index);
        self.files.insert(path, parsed);
    }

    pub fn set_include_roots<I, P>(&mut self, roots: I)
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.include_roots = roots.into_iter().map(Into::into).collect();
    }

    pub fn add_include_root(&mut self, root: impl Into<PathBuf>) {
        self.include_roots.push(root.into());
    }

    pub fn set_config(&mut self, config: WorkspaceConfig) {
        self.config = config;
    }

    pub fn config(&self) -> WorkspaceConfig {
        self.config
    }

    pub fn set_line_length_limits(
        &mut self,
        max_line_length: Option<usize>,
        max_comment_line_length: Option<usize>,
    ) {
        self.config.max_line_length = max_line_length;
        self.config.max_comment_line_length = max_comment_line_length;
    }

    pub fn remove_file(&mut self, path: &Path) {
        self.files.remove(path);
        if let Some(old_index) = self.file_symbol_index.remove(path) {
            for (name, _) in old_index {
                if let Some(entries) = self.by_name.get_mut(&name) {
                    entries.retain(|(p, _)| p != path);
                }
            }
        }
    }

    pub fn file(&self, path: &Path) -> Option<&ParsedFile> {
        self.files.get(path)
    }

    pub fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        let mut diagnostics = self
            .files
            .get(path)
            .map(|f| f.diagnostics.clone())
            .unwrap_or_default();
        if let Some(file) = self.files.get(path) {
            diagnostics.extend(self.use_diagnostics(file));
            diagnostics.extend(self.import_diagnostics(file));
            diagnostics.extend(self.include_diagnostics(file));
            diagnostics.extend(self.declared_type_diagnostics(file));
            diagnostics.extend(self.kind_selector_diagnostics(file));
            diagnostics.extend(self.method_binding_diagnostics(file));
            diagnostics.extend(self.generic_binding_diagnostics(file));
            diagnostics.extend(self.interface_diagnostics(file));
            diagnostics.extend(self.submodule_diagnostics(file));
            diagnostics.extend(self.submodule_ancestor_masking_diagnostics(file));
            diagnostics.extend(self.submodule_clock_local_masking_diagnostics(file));
            diagnostics.extend(self.submodule_result_dummy_duplicate_diagnostics(file));
            diagnostics.extend(self.whole_module_use_parameter_masking_diagnostics(file));
            diagnostics.extend(self.parent_parameter_masking_diagnostics(file, &diagnostics));
            diagnostics.extend(self.function_result_masking_diagnostics(file, &diagnostics));
            diagnostics.extend(self.same_module_callable_masking_diagnostics(file, &diagnostics));
            diagnostics.extend(self.type_diagnostics(file));
            diagnostics.extend(self.call_diagnostics(file));
            diagnostics.extend(self.line_length_diagnostics(file));
            retain_diagnostics_in_source(&mut diagnostics, &file.source);
        }
        diagnostics
    }

    pub fn code_actions(&self, path: &Path) -> Vec<CodeAction> {
        self.files
            .get(path)
            .map(|file| self.deferred_procedure_actions(file))
            .unwrap_or_default()
    }

    /// Position-aware code actions: everything from [`Self::code_actions`]
    /// plus quick fixes for the symbol under the cursor (currently:
    /// `add use <module>, only: <name>` for a name another module exports).
    pub fn code_actions_at(&self, path: &Path, pos: Position, source: &str) -> Vec<CodeAction> {
        let mut actions = self.code_actions(path);
        actions.extend(self.add_use_actions(path, pos, source));
        actions
    }

    /// Offer `use <module>, only: <name>` for an unresolvable name at `pos`
    /// that some indexed module exports. The edit inserts after the last
    /// existing `use` of the enclosing scope (or right after the scope
    /// opener), before any `implicit` statement can be violated.
    fn add_use_actions(&self, path: &Path, pos: Position, source: &str) -> Vec<CodeAction> {
        let Some(word) = word_at_source(source, pos) else {
            return Vec::new();
        };
        let Some(file) = self.files.get(path) else {
            return Vec::new();
        };
        // Resolvable or intrinsic → nothing to fix.
        if self.resolve_at(path, pos, &word).is_some()
            || self.find_visible_intrinsic(path, &word).is_some()
        {
            return Vec::new();
        }
        let scope = file.scope_at(pos);
        if scope.is_empty() {
            return Vec::new();
        }
        let mut modules: Vec<String> = self
            .files
            .values()
            .flat_map(|f| f.symbols.iter())
            .filter(|sym| sym.kind == SymbolKind::Module && sym.scope.is_empty())
            .filter(|module| !module.name.eq_ignore_ascii_case(&scope[0]))
            .filter(|module| {
                self.find_module_export_symbol(&module.name, &word)
                    .is_some()
            })
            .map(|module| module.name.clone())
            .collect();
        modules.sort_by_key(|name| name.to_ascii_lowercase());
        modules.dedup_by_key(|name| name.to_ascii_lowercase());

        let Some((insert_line, indent)) = self.use_insertion_point(file, &scope) else {
            return Vec::new();
        };
        modules
            .into_iter()
            .map(|module| CodeAction {
                title: format!("Add `use {module}, only: {word}`"),
                kind: "quickfix".to_string(),
                edits: vec![TextEdit {
                    file: path.to_path_buf(),
                    range: Range {
                        start: Position::new(insert_line, 0),
                        end: Position::new(insert_line, 0),
                    },
                    new_text: format!("{indent}use {module}, only: {word}\n"),
                }],
            })
            .collect()
    }

    /// Line to insert a new `use` statement at for `scope`, plus the
    /// indentation to use: after the scope's last existing `use`, else right
    /// after the scope opener line.
    fn use_insertion_point(&self, file: &ParsedFile, scope: &[String]) -> Option<(usize, String)> {
        let existing = file
            .uses
            .iter()
            .filter(|use_stmt| scopes_equal_case_insensitive(&use_stmt.scope, scope))
            .max_by_key(|use_stmt| use_stmt.range.start.line);
        if let Some(use_stmt) = existing {
            let line = use_stmt.range.start.line;
            let indent: String = file
                .source
                .lines()
                .nth(line)
                .map(|l| l.chars().take_while(|c| c.is_whitespace()).collect())
                .unwrap_or_default();
            return Some((line + 1, indent));
        }
        let (name, parent) = scope.split_last()?;
        let opener = file.symbols.iter().find(|sym| {
            is_scope_kind(sym.kind)
                && sym.name.eq_ignore_ascii_case(name)
                && scopes_equal_case_insensitive(&sym.scope, parent)
        })?;
        let line = opener.range.start.line;
        let indent = if crate::parser::is_fixed_form_path(&file.path) {
            "      ".to_string()
        } else {
            let opener_indent: String = file
                .source
                .lines()
                .nth(line)
                .map(|l| l.chars().take_while(|c| c.is_whitespace()).collect())
                .unwrap_or_default();
            format!("{opener_indent}  ")
        };
        Some((line + 1, indent))
    }

    pub fn document_symbols(&self, path: &Path) -> Vec<DocumentSymbol> {
        self.files
            .get(path)
            .map(ParsedFile::document_symbols)
            .unwrap_or_default()
    }

    pub fn workspace_symbols(&self, query: &str) -> Vec<Symbol> {
        let query = query.trim().to_ascii_lowercase();
        let mut symbols = Vec::new();
        for file in self.files.values() {
            for sym in &file.symbols {
                let name = sym.name.to_ascii_lowercase();
                let qualified = sym.qualified_name().to_ascii_lowercase();
                if query.is_empty() || name.contains(&query) || qualified.contains(&query) {
                    symbols.push(sym.clone());
                }
            }
        }
        symbols.sort_by(|a, b| {
            a.qualified_name()
                .cmp(&b.qualified_name())
                .then(a.file.cmp(&b.file))
                .then(
                    a.selection_range
                        .start
                        .line
                        .cmp(&b.selection_range.start.line),
                )
                .then(
                    a.selection_range
                        .start
                        .character
                        .cmp(&b.selection_range.start.character),
                )
        });
        symbols
    }

    pub fn selection_range(
        &self,
        path: &Path,
        pos: Position,
        source: &str,
    ) -> Option<SelectionRange> {
        let file = self.files.get(path)?;
        let mut ranges = Vec::new();
        if let Some(range) = word_range_at_source(source, pos) {
            push_unique_range(&mut ranges, range);
        }
        let mut scopes: Vec<_> = file
            .symbols
            .iter()
            .filter(|sym| is_scope_selection_candidate(sym.kind) && sym.range.contains(pos))
            .collect();
        scopes.sort_by(|a, b| {
            range_size(&a.range)
                .cmp(&range_size(&b.range))
                .then(a.range.start.line.cmp(&b.range.start.line))
                .then(a.range.start.character.cmp(&b.range.start.character))
        });
        for sym in scopes {
            push_unique_range(&mut ranges, sym.selection_range.clone());
            push_unique_range(&mut ranges, sym.range.clone());
        }
        if ranges.is_empty() {
            ranges.push(Range {
                start: pos,
                end: pos,
            });
        }
        selection_range_chain(ranges)
    }

    pub fn hover(&self, path: &Path, pos: Position, source: &str) -> Option<String> {
        if let Some(include) = self.include_at(path, pos) {
            return Some(self.include_hover(include));
        }
        if let Some(hover) = self.preprocessor_hover(path, pos, source) {
            return Some(hover);
        }
        if let Some(sym) = self.files.get(path).and_then(|file| file.symbol_at(pos)) {
            return Some(self.symbol_hover(sym));
        }
        if let Some(access) = member_access_at_source(source, pos) {
            if let Some(method) = self.find_member_method(path, &access.receiver, &access.member) {
                return Some(self.symbol_hover(method));
            }
        }
        if let Some(hover) = literal_hover_at_source(source, pos) {
            return Some(hover);
        }
        let word = word_at_source(source, pos)?;
        self.resolve_at(path, pos, &word)
            .map(|sym| self.symbol_hover(sym))
            .or_else(|| {
                self.find_visible_intrinsic(path, &word)
                    .map(|sym| sym.hover_markdown())
            })
    }

    fn preprocessor_hover(&self, path: &Path, pos: Position, source: &str) -> Option<String> {
        let file = self.files.get(path)?;
        let word = word_at_source(source, pos)?;
        let definition = file.preprocessor_definitions.get(&word)?;
        let signature = if definition.is_empty() {
            format!("#define {word}")
        } else {
            format!("#define {word} {definition}")
        };
        Some(format!("```fortran\n{signature}\n```"))
    }

    fn preprocessor_name_at(&self, path: &Path, pos: Position, source: &str) -> Option<String> {
        let file = self.files.get(path)?;
        let word = word_at_source(source, pos)?;
        file.preprocessor_definitions
            .keys()
            .find(|name| name.eq_ignore_ascii_case(&word))
            .cloned()
    }

    fn preprocessor_references(&self, path: &Path, name: &str) -> Vec<Location> {
        let Some(file) = self.files.get(path) else {
            return Vec::new();
        };
        let mut locations: Vec<_> = identifier_occurrences(&file.source, name)
            .into_iter()
            .map(|range| Location {
                file: path.to_path_buf(),
                range,
            })
            .collect();
        locations.sort_by(|a, b| {
            a.range
                .start
                .line
                .cmp(&b.range.start.line)
                .then(a.range.start.character.cmp(&b.range.start.character))
        });
        locations
    }

    fn preprocessor_definition_range(&self, path: &Path, name: &str) -> Option<Range> {
        let file = self.files.get(path)?;
        if !file
            .preprocessor_definitions
            .keys()
            .any(|existing| existing.eq_ignore_ascii_case(name))
        {
            return None;
        }
        file.preprocessor
            .iter()
            .find(|directive| {
                directive.kind == PreprocessorKind::Define
                    && directive
                        .name
                        .as_deref()
                        .is_some_and(|existing| existing.eq_ignore_ascii_case(name))
            })
            .and_then(|directive| {
                directive.name.as_deref().and_then(|existing| {
                    directive_name_range(&file.source, directive.range.start.line, existing)
                })
            })
    }

    pub fn definition(&self, path: &Path, pos: Position, source: &str) -> Option<&Symbol> {
        if let Some(sym) = self.files.get(path).and_then(|file| file.symbol_at(pos)) {
            return Some(
                self.module_procedure_prototype(sym)
                    .or_else(|| self.method_target_symbol(sym))
                    .unwrap_or(sym),
            );
        }
        if let Some(access) = member_access_at_source(source, pos) {
            if let Some(method) = self.find_member_method(path, &access.receiver, &access.member) {
                return Some(self.method_target_symbol(method).unwrap_or(method));
            }
        }
        let word = word_at_source(source, pos)?;
        self.resolve_at(path, pos, &word)
            .map(|sym| self.method_target_symbol(sym).unwrap_or(sym))
    }

    pub fn definition_location(
        &self,
        path: &Path,
        pos: Position,
        source: &str,
    ) -> Option<Location> {
        if let Some(name) = self.preprocessor_name_at(path, pos, source) {
            let range = self.preprocessor_definition_range(path, &name)?;
            return Some(Location {
                file: path.to_path_buf(),
                range,
            });
        }
        let sym = self.definition(path, pos, source)?;
        Some(Location {
            file: sym.file.clone(),
            range: sym.selection_range.clone(),
        })
    }

    pub fn implementation_location(
        &self,
        path: &Path,
        pos: Position,
        source: &str,
    ) -> Option<Location> {
        let sym = if let Some(access) = member_access_at_source(source, pos) {
            self.find_member_method(path, &access.receiver, &access.member)
                .and_then(|method| self.method_target_symbol(method))
        } else if let Some(sym) = self.files.get(path).and_then(|file| file.symbol_at(pos)) {
            self.method_target_symbol(sym)
                .or_else(|| self.module_procedure_implementation(sym))
        } else {
            let word = word_at_source(source, pos)?;
            self.resolve_at(path, pos, &word).and_then(|sym| {
                self.method_target_symbol(sym)
                    .or_else(|| self.module_procedure_implementation(sym))
            })
        }?;
        Some(Location {
            file: sym.file.clone(),
            range: sym.selection_range.clone(),
        })
    }

    pub fn completions(&self, path: &Path, prefix: &str) -> Vec<CompletionItem> {
        let Some(file) = self.files.get(path) else {
            return self.global_completions(path, prefix);
        };
        let line = file.source.lines().count().saturating_sub(1);
        self.completions_at(path, Position::new(line, 0), prefix)
    }

    pub fn completions_at(&self, path: &Path, pos: Position, prefix: &str) -> Vec<CompletionItem> {
        let prefix = prefix.to_ascii_lowercase();
        if let Some(items) = self.use_statement_completions_at(path, pos, &prefix) {
            return items;
        }
        if let Some(items) = self.import_statement_completions_at(path, pos, &prefix) {
            return items;
        }
        if let Some(items) = self.procedure_interface_completions_at(path, pos, &prefix) {
            return items;
        }
        if let Some(items) = self.visibility_statement_completions_at(path, pos, &prefix) {
            return items;
        }
        if let Some(items) = self.type_name_completions_at(path, pos, &prefix) {
            return items;
        }
        if let Some(items) = self.declaration_keyword_completions_at(path, pos, &prefix) {
            return items;
        }
        if let Some(items) = self.declaration_variable_completions_at(path, pos, &prefix) {
            return items;
        }
        if let Some(items) = self.module_procedure_link_completions_at(path, pos, &prefix) {
            return items;
        }
        if let Some(items) = self.call_statement_completions_at(path, pos, &prefix) {
            return items;
        }
        if let Some(items) = self.member_completions_at(path, pos, &prefix) {
            return items;
        }
        if self.skip_completion_at(path, pos) {
            return Vec::new();
        }
        if let Some(items) = self.first_word_statement_completions_at(path, pos, &prefix) {
            return items;
        }
        self.default_completions_at(path, pos, &prefix)
    }

    fn default_completions_at(
        &self,
        path: &Path,
        pos: Position,
        prefix: &str,
    ) -> Vec<CompletionItem> {
        let mut items = BTreeMap::new();
        let current_scope = self
            .files
            .get(path)
            .map(|file| file.scope_at(pos))
            .unwrap_or_default();
        if let Some(file) = self.files.get(path) {
            for sym in &file.symbols {
                if sym.name.to_ascii_lowercase().starts_with(&prefix)
                    && visible_scope_match_len(&current_scope, &sym.scope).is_some()
                {
                    items.insert(sym.name.clone(), CompletionItem::from_symbol(sym));
                }
            }
            for included in self.include_symbols(file) {
                let sym = included.symbol;
                if sym.name.to_ascii_lowercase().starts_with(&prefix)
                    && visible_scope_match_len(&current_scope, &included.effective_scope).is_some()
                {
                    items.insert(sym.name.clone(), CompletionItem::from_symbol(sym));
                }
            }
            for use_stmt in &file.uses {
                if !scope_is_ancestor(&use_stmt.scope, &current_scope) {
                    continue;
                }
                self.add_use_completions(use_stmt, &prefix, &mut items);
            }
            add_preprocessor_completions(file, &prefix, &mut items);
        }
        for item in self.visible_intrinsic_completions(path, &prefix, Some(&current_scope)) {
            items.entry(item.label.clone()).or_insert(CompletionItem {
                label: item.label,
                detail: item.detail,
                kind: item.kind,
                documentation: item.documentation,
                visibility: item.visibility,
            });
        }
        items.into_values().collect()
    }

    fn type_name_completions_at(
        &self,
        path: &Path,
        pos: Position,
        prefix: &str,
    ) -> Option<Vec<CompletionItem>> {
        let file = self.files.get(path)?;
        let line = file.source.lines().nth(pos.line)?;
        type_name_completion_context(line, pos.character)?;
        Some(self.type_name_completions(file, pos, prefix))
    }

    fn type_name_completions(
        &self,
        file: &ParsedFile,
        pos: Position,
        prefix: &str,
    ) -> Vec<CompletionItem> {
        let current_scope = file.scope_at(pos);
        let mut items = BTreeMap::new();
        for sym in &file.symbols {
            if sym.kind == SymbolKind::Type
                && sym.name.to_ascii_lowercase().starts_with(prefix)
                && visible_scope_match_len(&current_scope, &sym.scope).is_some()
            {
                items.insert(sym.name.clone(), CompletionItem::from_symbol(sym));
            }
        }
        for included in self.include_symbols(file) {
            let sym = included.symbol;
            if sym.kind == SymbolKind::Type
                && sym.name.to_ascii_lowercase().starts_with(prefix)
                && visible_scope_match_len(&current_scope, &included.effective_scope).is_some()
            {
                items.insert(sym.name.clone(), CompletionItem::from_symbol(sym));
            }
        }
        for use_stmt in &file.uses {
            if !scope_is_ancestor(&use_stmt.scope, &current_scope) {
                continue;
            }
            for sym in self.module_export_symbols(&use_stmt.module) {
                if sym.kind == SymbolKind::Type && sym.name.to_ascii_lowercase().starts_with(prefix)
                {
                    items
                        .entry(sym.name.clone())
                        .or_insert_with(|| CompletionItem::from_symbol(sym));
                }
            }
            if intrinsics::find_intrinsic_module(&use_stmt.module).is_some() {
                for sym in intrinsics::module_symbols(&use_stmt.module) {
                    if sym.kind.symbol_kind() == SymbolKind::Type
                        && sym.name.to_ascii_lowercase().starts_with(prefix)
                    {
                        let item = intrinsics::IntrinsicCompletion::from_symbol(sym);
                        items.entry(item.label.clone()).or_insert(CompletionItem {
                            label: item.label,
                            detail: item.detail,
                            kind: item.kind,
                            documentation: item.documentation,
                            visibility: item.visibility,
                        });
                    }
                }
            }
        }
        items.into_values().collect()
    }

    fn declaration_keyword_completions_at(
        &self,
        path: &Path,
        pos: Position,
        prefix: &str,
    ) -> Option<Vec<CompletionItem>> {
        let file = self.files.get(path)?;
        let line = file.source.lines().nth(pos.line)?;
        declaration_keyword_context(line, pos.character)?;
        Some(declaration_keyword_completions(
            prefix,
            declaration_keyword_scope(file, pos),
        ))
    }

    fn import_statement_completions_at(
        &self,
        path: &Path,
        pos: Position,
        prefix: &str,
    ) -> Option<Vec<CompletionItem>> {
        let file = self.files.get(path)?;
        let line = file.source.lines().nth(pos.line)?;
        import_statement_completion_context(line, pos.character)?;
        let host_scopes = import_completion_host_scopes(&file.scope_at(pos));
        let mut items = BTreeMap::new();
        for scope in host_scopes {
            for sym in &file.symbols {
                if import_completion_symbol(sym)
                    && scopes_equal(&sym.scope, &scope)
                    && sym.name.to_ascii_lowercase().starts_with(prefix)
                {
                    items
                        .entry(sym.name.clone())
                        .or_insert_with(|| CompletionItem::from_symbol(sym));
                }
            }
        }
        Some(items.into_values().collect())
    }

    fn procedure_interface_completions_at(
        &self,
        path: &Path,
        pos: Position,
        prefix: &str,
    ) -> Option<Vec<CompletionItem>> {
        let file = self.files.get(path)?;
        let line = file.source.lines().nth(pos.line)?;
        procedure_interface_completion_context(line, pos.character)?;
        let current_scope = file.scope_at(pos);
        let mut items = BTreeMap::new();
        for sym in &file.symbols {
            let Some(host_scope) = abstract_interface_prototype_host_scope(file, sym) else {
                continue;
            };
            if visible_scope_match_len(&current_scope, host_scope).is_some()
                && sym.name.to_ascii_lowercase().starts_with(prefix)
            {
                items
                    .entry(sym.name.clone())
                    .or_insert_with(|| CompletionItem::from_symbol(sym));
            }
        }
        for use_stmt in &file.uses {
            if !scope_is_ancestor(&use_stmt.scope, &current_scope) {
                continue;
            }
            self.add_procedure_interface_use_completions(use_stmt, prefix, &mut items);
        }
        Some(items.into_values().collect())
    }

    fn visibility_statement_completions_at(
        &self,
        path: &Path,
        pos: Position,
        prefix: &str,
    ) -> Option<Vec<CompletionItem>> {
        let file = self.files.get(path)?;
        let line = file.source.lines().nth(pos.line)?;
        visibility_statement_completion_context(line, pos.character)?;
        let current_scope = file.scope_at(pos);
        let mut items = BTreeMap::new();
        for sym in &file.symbols {
            if visibility_completion_symbol(sym)
                && scopes_equal(&sym.scope, &current_scope)
                && sym.name.to_ascii_lowercase().starts_with(prefix)
            {
                items
                    .entry(sym.name.clone())
                    .or_insert_with(|| CompletionItem::from_symbol(sym));
            }
        }
        Some(items.into_values().collect())
    }

    fn declaration_variable_completions_at(
        &self,
        path: &Path,
        pos: Position,
        prefix: &str,
    ) -> Option<Vec<CompletionItem>> {
        let file = self.files.get(path)?;
        let line = file.source.lines().nth(pos.line)?;
        declaration_variable_completion_context(line, pos.character)?;
        Some(self.variable_completions(file, pos, prefix))
    }

    fn variable_completions(
        &self,
        file: &ParsedFile,
        pos: Position,
        prefix: &str,
    ) -> Vec<CompletionItem> {
        let current_scope = file.scope_at(pos);
        let mut items = BTreeMap::new();
        for sym in &file.symbols {
            if variable_completion_symbol(sym)
                && sym.name.to_ascii_lowercase().starts_with(prefix)
                && visible_scope_match_len(&current_scope, &sym.scope).is_some()
            {
                items.insert(sym.name.clone(), CompletionItem::from_symbol(sym));
            }
        }
        for included in self.include_symbols(file) {
            let sym = included.symbol;
            if variable_completion_symbol(sym)
                && sym.name.to_ascii_lowercase().starts_with(prefix)
                && visible_scope_match_len(&current_scope, &included.effective_scope).is_some()
            {
                items.insert(sym.name.clone(), CompletionItem::from_symbol(sym));
            }
        }
        for use_stmt in &file.uses {
            if !scope_is_ancestor(&use_stmt.scope, &current_scope) {
                continue;
            }
            self.add_variable_use_completions(use_stmt, prefix, &mut items);
        }
        items.into_values().collect()
    }

    fn first_word_statement_completions_at(
        &self,
        path: &Path,
        pos: Position,
        prefix: &str,
    ) -> Option<Vec<CompletionItem>> {
        let file = self.files.get(path)?;
        let line = file.source.lines().nth(pos.line)?;
        first_word_statement_completion_context(line, pos.character)?;
        let mut items = BTreeMap::new();
        for item in self.default_completions_at(path, pos, prefix) {
            items.insert(item.label.clone(), item);
        }
        for item in fortran_statement_completions(prefix) {
            items.entry(item.label.clone()).or_insert(item);
        }
        Some(items.into_values().collect())
    }

    fn skip_completion_at(&self, path: &Path, pos: Position) -> bool {
        self.files
            .get(path)
            .and_then(|file| file.source.lines().nth(pos.line))
            .is_some_and(|line| skip_completion_context(line, pos.character))
    }

    fn use_statement_completions_at(
        &self,
        path: &Path,
        pos: Position,
        prefix: &str,
    ) -> Option<Vec<CompletionItem>> {
        let file = self.files.get(path)?;
        let line = file.source.lines().nth(pos.line)?;
        match use_completion_context(line, pos.character)? {
            UseCompletionContext::Module => Some(self.module_name_completions(prefix)),
            UseCompletionContext::Only { module } => {
                Some(self.module_member_completions(&module, prefix))
            }
        }
    }

    fn module_name_completions(&self, prefix: &str) -> Vec<CompletionItem> {
        let mut items = BTreeMap::new();
        for file in self.files.values() {
            for sym in &file.symbols {
                if sym.kind == SymbolKind::Module
                    && sym.name.to_ascii_lowercase().starts_with(prefix)
                {
                    items.insert(sym.name.clone(), CompletionItem::from_symbol(sym));
                }
            }
        }
        for sym in intrinsics::intrinsics().iter().filter(|sym| {
            sym.kind == intrinsics::IntrinsicKind::Module
                && sym.name.to_ascii_lowercase().starts_with(prefix)
        }) {
            let item = intrinsics::IntrinsicCompletion::from_symbol(sym);
            items.entry(item.label.clone()).or_insert(CompletionItem {
                label: item.label,
                detail: item.detail,
                kind: item.kind,
                documentation: item.documentation,
                visibility: item.visibility,
            });
        }
        items.into_values().collect()
    }

    fn module_member_completions(&self, module: &str, prefix: &str) -> Vec<CompletionItem> {
        let mut items = BTreeMap::new();
        for sym in self.module_export_symbols(module) {
            if sym.name.to_ascii_lowercase().starts_with(prefix) {
                items.insert(sym.name.clone(), CompletionItem::from_symbol(sym));
            }
        }
        if intrinsics::find_intrinsic_module(module).is_some() {
            for sym in intrinsics::module_symbols(module) {
                if !sym.name.to_ascii_lowercase().starts_with(prefix) {
                    continue;
                }
                let item = intrinsics::IntrinsicCompletion::from_symbol(sym);
                items.entry(item.label.clone()).or_insert(CompletionItem {
                    label: item.label,
                    detail: item.detail,
                    kind: item.kind,
                    documentation: item.documentation,
                    visibility: item.visibility,
                });
            }
        }
        items.into_values().collect()
    }

    fn module_procedure_link_completions_at(
        &self,
        path: &Path,
        pos: Position,
        prefix: &str,
    ) -> Option<Vec<CompletionItem>> {
        let file = self.files.get(path)?;
        let line = file.source.lines().nth(pos.line)?;
        module_procedure_link_completion_context(line, pos.character)?;
        let mut current_scope = file.scope_at(pos);
        if current_scope.split_last().is_some_and(|(name, scope)| {
            file.symbols.iter().any(|sym| {
                sym.name.eq_ignore_ascii_case(name)
                    && scopes_equal(&sym.scope, scope)
                    && is_module_procedure_link(sym)
            })
        }) {
            current_scope.pop();
        }
        let parent_scope = current_scope
            .split_last()
            .and_then(|(interface_name, parent_scope)| {
                file.symbols
                    .iter()
                    .any(|sym| {
                        sym.kind == SymbolKind::Interface
                            && sym.name.eq_ignore_ascii_case(interface_name)
                            && scopes_equal(&sym.scope, parent_scope)
                    })
                    .then_some(parent_scope)
            })
            .unwrap_or(current_scope.as_slice());
        let mut items = BTreeMap::new();
        for sym in &file.symbols {
            if matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Function)
                && scopes_equal(&sym.scope, parent_scope)
                && !is_module_procedure_link(sym)
                && sym.name.to_ascii_lowercase().starts_with(prefix)
            {
                items.insert(sym.name.clone(), CompletionItem::from_symbol(sym));
            }
        }
        Some(items.into_values().collect())
    }

    fn member_completions_at(
        &self,
        path: &Path,
        pos: Position,
        prefix: &str,
    ) -> Option<Vec<CompletionItem>> {
        let file = self.files.get(path)?;
        let receiver = member_completion_receiver(&file.source, pos, prefix)?;
        let receiver_sym = self.find_visible_symbol(path, &receiver)?;
        let type_name = declared_type_name(receiver_sym)?;
        let ty = self.find_type_for_symbol(receiver_sym, type_name)?;
        let mut items = BTreeMap::new();
        let mut visited = HashSet::new();
        self.add_type_member_completions(ty, prefix, &mut visited, &mut items);
        Some(items.into_values().collect())
    }

    fn call_statement_completions_at(
        &self,
        path: &Path,
        pos: Position,
        prefix: &str,
    ) -> Option<Vec<CompletionItem>> {
        let file = self.files.get(path)?;
        let line = file.source.lines().nth(pos.line)?;
        call_statement_completion_context(line, pos.character)?;
        Some(self.call_statement_completions(file, pos, prefix))
    }

    fn call_statement_completions(
        &self,
        file: &ParsedFile,
        pos: Position,
        prefix: &str,
    ) -> Vec<CompletionItem> {
        let current_scope = file.scope_at(pos);
        let mut items = BTreeMap::new();
        for sym in &file.symbols {
            if callable_completion_symbol(sym)
                && sym.name.to_ascii_lowercase().starts_with(prefix)
                && visible_scope_match_len(&current_scope, &sym.scope).is_some()
            {
                items.insert(sym.name.clone(), CompletionItem::from_symbol(sym));
            }
        }
        for included in self.include_symbols(file) {
            let sym = included.symbol;
            if callable_completion_symbol(sym)
                && sym.name.to_ascii_lowercase().starts_with(prefix)
                && visible_scope_match_len(&current_scope, &included.effective_scope).is_some()
            {
                items.insert(sym.name.clone(), CompletionItem::from_symbol(sym));
            }
        }
        for use_stmt in &file.uses {
            if scope_is_ancestor(&use_stmt.scope, &current_scope) {
                self.add_callable_use_completions(use_stmt, prefix, &mut items);
            }
        }
        self.add_intrinsic_subroutine_completions(prefix, &mut items);
        items.into_values().collect()
    }

    fn global_completions(&self, path: &Path, prefix: &str) -> Vec<CompletionItem> {
        let prefix = prefix.to_ascii_lowercase();
        let mut items = BTreeMap::new();
        for entries in self.by_name.values() {
            for (file, idx) in entries {
                let Some(sym) = self.files.get(file).and_then(|f| f.symbols.get(*idx)) else {
                    continue;
                };
                if sym.name.to_ascii_lowercase().starts_with(&prefix)
                    && sym.scope.is_empty()
                    && !matches!(sym.kind, SymbolKind::Variable | SymbolKind::Method)
                {
                    items.insert(sym.name.clone(), CompletionItem::from_symbol(sym));
                }
            }
        }
        for item in self.visible_intrinsic_completions(path, &prefix, None) {
            items.entry(item.label.clone()).or_insert(CompletionItem {
                label: item.label,
                detail: item.detail,
                kind: item.kind,
                documentation: item.documentation,
                visibility: item.visibility,
            });
        }
        items.into_values().collect()
    }

    fn add_type_member_completions(
        &self,
        ty: &Symbol,
        prefix: &str,
        visited: &mut HashSet<String>,
        items: &mut BTreeMap<String, CompletionItem>,
    ) {
        let key = ty.qualified_name().to_ascii_lowercase();
        if !visited.insert(key) {
            return;
        }
        if let Some(parent_name) = &ty.extends {
            if let Some(file) = self.files.get(&ty.file) {
                if let Some(parent) = self.find_parent_type(file, ty, parent_name) {
                    self.add_type_member_completions(parent, prefix, visited, items);
                }
            }
        }
        for method in self.direct_type_methods(ty) {
            if method.visibility == Visibility::Private
                || !method.name.to_ascii_lowercase().starts_with(prefix)
            {
                continue;
            }
            let mut item = CompletionItem::from_symbol(method);
            if let Some(target) = self.method_target_symbol(method) {
                item.detail = method_signature(method, target);
                item.documentation = method
                    .documentation
                    .clone()
                    .or_else(|| target.documentation.clone());
            }
            items.insert(method.name.clone(), item);
        }
        for generic in self.direct_type_generics(ty) {
            if generic.visibility == Visibility::Private
                || !generic.name.to_ascii_lowercase().starts_with(prefix)
            {
                continue;
            }
            items.insert(
                generic.name.clone(),
                CompletionItem {
                    label: generic.name.clone(),
                    detail: format!("generic binding => {}", generic.procedures.join(", ")),
                    kind: SymbolKind::Method,
                    documentation: None,
                    visibility: generic.visibility,
                },
            );
        }
    }

    pub fn signature_help(
        &self,
        path: &Path,
        pos: Position,
        source: &str,
    ) -> Option<SignatureHelp> {
        let call = call_context(source, pos)?;
        if let Some(receiver) = &call.receiver {
            if let Some(method) = self.find_member_method_for_call(
                path,
                receiver,
                &call.name,
                call.argument_count,
                call.active_argument_name.as_deref(),
            ) {
                if let Some(help) = self.method_signature_help(
                    method,
                    call.active_parameter,
                    call.active_argument_name.as_deref(),
                ) {
                    return Some(help);
                }
            }
        }
        if let Some(sym) = self.find_visible_symbol(path, &call.name) {
            if let Some(help) = self.method_signature_help(
                sym,
                call.active_parameter,
                call.active_argument_name.as_deref(),
            ) {
                return Some(help);
            }
            if let Some(target) = self.find_generic_interface_procedure_for_signature(
                sym,
                call.argument_count,
                call.active_argument_name.as_deref(),
            ) {
                return Some(SignatureHelp {
                    label: procedure_signature_label(target),
                    parameters: target.args.clone(),
                    active_parameter: signature_active_parameter(
                        &target.args,
                        call.active_parameter,
                        call.active_argument_name.as_deref(),
                    ),
                    documentation: target.documentation.clone(),
                });
            }
            if !matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Function) {
                return None;
            }
            return Some(SignatureHelp {
                label: procedure_signature_label(sym),
                parameters: sym.args.clone(),
                active_parameter: signature_active_parameter(
                    &sym.args,
                    call.active_parameter,
                    call.active_argument_name.as_deref(),
                ),
                documentation: sym.documentation.clone(),
            });
        }
        let intrinsic = self.find_visible_intrinsic(path, &call.name)?;
        if intrinsic.args.is_empty() {
            return None;
        }
        let parameters: Vec<_> = intrinsic.args.iter().map(|arg| arg.to_string()).collect();
        Some(SignatureHelp {
            label: intrinsic.signature(),
            active_parameter: signature_active_parameter(
                &parameters,
                call.active_parameter,
                call.active_argument_name.as_deref(),
            ),
            parameters,
            documentation: (!intrinsic.documentation.is_empty())
                .then(|| intrinsic.documentation.to_string()),
        })
    }

    pub fn references(&self, path: &Path, pos: Position, source: &str) -> Vec<Location> {
        if let Some(name) = self.preprocessor_name_at(path, pos, source) {
            return self.preprocessor_references(path, &name);
        }
        let Some(target) = self.definition(path, pos, source) else {
            return Vec::new();
        };
        let target_key = SymbolKey::from_symbol(target);
        let target_name = target.name.clone();
        let mut locations = Vec::new();
        for (file_path, file) in &self.files {
            for range in identifier_occurrences(&file.source, &target_name) {
                if self
                    .resolve_at(file_path, range.start, &target_name)
                    .is_some_and(|sym| SymbolKey::from_symbol(sym) == target_key)
                {
                    locations.push(Location {
                        file: file_path.clone(),
                        range,
                    });
                }
            }
        }
        locations.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then(a.range.start.line.cmp(&b.range.start.line))
                .then(a.range.start.character.cmp(&b.range.start.character))
        });
        locations
    }

    pub fn semantic_tokens(&self, path: &Path) -> Vec<SemanticToken> {
        let Some(file) = self.files.get(path) else {
            return Vec::new();
        };
        let mut names: HashSet<String> = self.by_name.keys().cloned().collect();
        for use_stmt in &file.uses {
            for name in &use_stmt.only {
                names.insert(name.to_ascii_lowercase());
            }
            for rename in &use_stmt.renames {
                names.insert(rename.local.to_ascii_lowercase());
            }
        }

        let mut tokens = BTreeMap::new();
        for name in names {
            for range in identifier_occurrences(&file.source, &name) {
                let Some(sym) = self.resolve_at(path, range.start, &name) else {
                    continue;
                };
                let token_type = self.semantic_token_type_for_symbol(sym);
                insert_semantic_token(&mut tokens, range, token_type);
            }
        }
        for directive in &file.preprocessor {
            if directive.kind != PreprocessorKind::Define {
                continue;
            }
            let Some(name) = &directive.name else {
                continue;
            };
            let Some(range) = directive_name_range(&file.source, directive.range.start.line, name)
            else {
                continue;
            };
            insert_semantic_token(&mut tokens, range, semantic_token_type::MACRO);
        }
        for name in file.preprocessor_definitions.keys() {
            for range in identifier_occurrences(&file.source, name) {
                insert_semantic_token(&mut tokens, range, semantic_token_type::MACRO);
            }
        }
        tokens.into_values().collect()
    }

    pub fn semantic_token_data(&self, path: &Path) -> Vec<u32> {
        let tokens = self.semantic_tokens(path);
        let mut data = Vec::with_capacity(tokens.len() * 5);
        let mut prev_line = 0u32;
        let mut prev_col = 0u32;
        for token in tokens {
            let line = token.range.start.line as u32;
            let col = token.range.start.character as u32;
            let len = token
                .range
                .end
                .character
                .saturating_sub(token.range.start.character) as u32;
            let delta_line = line.saturating_sub(prev_line);
            let delta_col = if delta_line == 0 {
                col.saturating_sub(prev_col)
            } else {
                col
            };
            data.extend_from_slice(&[delta_line, delta_col, len, token.token_type, 0]);
            prev_line = line;
            prev_col = col;
        }
        data
    }

    pub fn inlay_hints(&self, path: &Path, start_line: usize, end_line: usize) -> Vec<InlayHint> {
        let Some(file) = self.files.get(path) else {
            return Vec::new();
        };
        let mut hints = Vec::new();
        let fixed_form = is_fixed_form_path(path);
        for (line_no, line) in file.source.lines().enumerate() {
            if line_no < start_line || line_no > end_line {
                continue;
            }
            if fixed_form && is_fixed_comment(line) {
                continue;
            }
            for call in calls_on_line(line, line_no) {
                let Some(params) = self.call_parameters_for_line_call(path, &call) else {
                    continue;
                };
                for (idx, arg) in call.args.iter().enumerate() {
                    if idx >= params.len() || arg.keyword.is_some() {
                        continue;
                    }
                    let label = format!("{}:", parameter_label_name(&params[idx].label));
                    hints.push(InlayHint {
                        position: arg.start,
                        label,
                    });
                }
            }
        }
        hints
    }

    pub fn rename(
        &self,
        path: &Path,
        pos: Position,
        source: &str,
        new_name: &str,
    ) -> Result<Vec<TextEdit>, RenameError> {
        if !is_fortran_identifier(new_name) {
            return Err(RenameError::InvalidIdentifier);
        }
        if let Some(name) = self.preprocessor_name_at(path, pos, source) {
            if name.eq_ignore_ascii_case(new_name) {
                return Ok(Vec::new());
            }
            if let Some(range) = self.preprocessor_definition_range(path, new_name) {
                return Err(RenameError::ConflictingSymbol {
                    file: path.to_path_buf(),
                    range,
                });
            }
            return Ok(self
                .preprocessor_references(path, &name)
                .into_iter()
                .map(|loc| TextEdit {
                    file: loc.file,
                    range: loc.range,
                    new_text: new_name.to_string(),
                })
                .collect());
        }
        let target = self
            .definition(path, pos, source)
            .ok_or(RenameError::UnresolvedSymbol)?;
        let target_key = SymbolKey::from_symbol(target);
        if target.name.eq_ignore_ascii_case(new_name) {
            return Ok(Vec::new());
        }
        if let Some(conflict) = self.conflicting_rename_symbol(target, new_name) {
            return Err(RenameError::ConflictingSymbol {
                file: conflict.file.clone(),
                range: conflict.selection_range.clone(),
            });
        }
        Ok(self
            .references(path, pos, source)
            .into_iter()
            .filter(|loc| {
                self.resolve_at(&loc.file, loc.range.start, &target.name)
                    .is_some_and(|sym| SymbolKey::from_symbol(sym) == target_key)
            })
            .map(|loc| TextEdit {
                file: loc.file,
                range: loc.range,
                new_text: new_name.to_string(),
            })
            .collect())
    }

    pub fn resolved_includes(&self, path: &Path) -> Vec<ResolvedInclude> {
        self.files
            .get(path)
            .map(|file| {
                file.includes
                    .iter()
                    .map(|include| ResolvedInclude {
                        include: include.clone(),
                        resolved_path: self.resolve_include_path(include),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn symbol_hover(&self, sym: &Symbol) -> String {
        if let Some(target) = self.method_target_symbol(sym) {
            return method_hover(sym, target);
        }
        if let Some(prototype) = self.module_procedure_prototype(sym) {
            return prototype.hover_markdown();
        }
        sym.hover_markdown()
    }

    fn method_signature_help(
        &self,
        sym: &Symbol,
        active_parameter: usize,
        active_argument_name: Option<&str>,
    ) -> Option<SignatureHelp> {
        let target = self.method_target_symbol(sym)?;
        let args = method_call_args(sym, target);
        Some(SignatureHelp {
            label: if args.is_empty() {
                format!("{}()", sym.name)
            } else {
                format!("{}({})", sym.name, args.join(", "))
            },
            parameters: args.clone(),
            active_parameter: signature_active_parameter(
                &args,
                active_parameter,
                active_argument_name,
            ),
            documentation: sym
                .documentation
                .clone()
                .or_else(|| target.documentation.clone()),
        })
    }

    fn call_parameters(
        &self,
        path: &Path,
        name: &str,
        receiver: Option<&str>,
        argument_count: usize,
    ) -> Option<Vec<CallParameter>> {
        if let Some(receiver) = receiver {
            let method =
                self.find_member_method_for_call(path, receiver, name, argument_count, None)?;
            let target = self.method_target_symbol(method)?;
            let params = self.method_call_parameters(method, target);
            if params.is_empty() && argument_count > 0 {
                return None;
            }
            return Some(params);
        }
        if let Some(sym) = self.find_visible_symbol(path, name) {
            if let Some(target) = self.method_target_symbol(sym) {
                if sym.kind == SymbolKind::Method
                    && target.name.eq_ignore_ascii_case(&sym.name)
                    && target.scope == sym.scope[..sym.scope.len().saturating_sub(1)]
                {
                    return Some(self.procedure_call_parameters(target, &target.args));
                }
                let params = self.method_call_parameters(sym, target);
                if params.is_empty() && argument_count > 0 {
                    return None;
                }
                return Some(params);
            }
            if let Some(target) = self.find_generic_interface_procedure(sym, argument_count) {
                if target.args.is_empty() && argument_count > 0 {
                    return None;
                }
                return Some(self.procedure_call_parameters(target, &target.args));
            }
            if !sym.args.is_empty() {
                return Some(self.procedure_call_parameters(sym, &sym.args));
            }
            return None;
        }
        let intrinsic = self.find_visible_intrinsic(path, name)?;
        (!intrinsic.args.is_empty()).then(|| {
            let mut params: Vec<_> = intrinsic
                .args
                .iter()
                .map(|arg| intrinsic_call_parameter(&intrinsic.name, arg))
                .collect();
            add_synthetic_intrinsic_parameters(&intrinsic.name, &mut params);
            params
        })
    }

    fn call_parameters_for_line_call(
        &self,
        path: &Path,
        call: &LineCall,
    ) -> Option<Vec<CallParameter>> {
        if let Some(receiver) = &call.receiver {
            let method =
                self.find_member_method_for_call_args(path, receiver, &call.name, &call.args)?;
            let target = self.method_target_symbol(method)?;
            let params = self.method_call_parameters(method, target);
            if params.is_empty() && !call.args.is_empty() {
                return None;
            }
            return Some(params);
        }
        if let Some(sym) = self.find_visible_symbol_at(path, call.start, &call.name) {
            if let Some(target) = self.method_target_symbol(sym) {
                if sym.kind == SymbolKind::Method
                    && target.name.eq_ignore_ascii_case(&sym.name)
                    && target.scope == sym.scope[..sym.scope.len().saturating_sub(1)]
                {
                    return Some(self.procedure_call_parameters(target, &target.args));
                }
                let params = self.method_call_parameters(sym, target);
                if params.is_empty() && !call.args.is_empty() {
                    return None;
                }
                return Some(params);
            }
            if let Some(target) = self.find_generic_interface_procedure_for_args(sym, &call.args) {
                if target.args.is_empty() && !call.args.is_empty() {
                    return None;
                }
                return Some(self.procedure_call_parameters(target, &target.args));
            }
            if !sym.args.is_empty() {
                return Some(self.procedure_call_parameters(sym, &sym.args));
            }
            return None;
        }
        self.call_parameters(path, &call.name, None, call.args.len())
    }

    fn find_visible_symbol_at(&self, path: &Path, pos: Position, name: &str) -> Option<&Symbol> {
        if let Some(file) = self.files.get(path) {
            let current_scope = file.scope_at(pos);
            if let Some(sym) = file
                .symbols
                .iter()
                .filter(|sym| sym.name.eq_ignore_ascii_case(name))
                .filter_map(|sym| {
                    visible_scope_match_len(&current_scope, &sym.scope).map(|len| (len, sym))
                })
                .max_by_key(|(len, _)| *len)
                .map(|(_, sym)| sym)
            {
                return Some(sym);
            }
            if let Some(sym) = self.find_include_symbol_at(file, &current_scope, name) {
                return Some(sym);
            }
        }
        self.find_visible_symbol(path, name)
    }

    fn method_call_parameters(&self, method: &Symbol, target: &Symbol) -> Vec<CallParameter> {
        let args = method_call_args(method, target);
        self.procedure_call_parameters(target, &args)
    }

    fn procedure_call_parameters(&self, procedure: &Symbol, args: &[String]) -> Vec<CallParameter> {
        args.iter()
            .map(|arg| {
                let optional = self
                    .procedure_dummy_symbol(procedure, arg)
                    .is_some_and(is_optional_dummy);
                CallParameter {
                    label: arg.clone(),
                    name: parameter_label_name(arg).to_ascii_lowercase(),
                    optional,
                }
            })
            .collect()
    }

    fn find_member_method(&self, path: &Path, receiver: &str, member: &str) -> Option<&Symbol> {
        self.find_member_method_for_call(path, receiver, member, 0, None)
    }

    fn find_member_method_for_call(
        &self,
        path: &Path,
        receiver: &str,
        member: &str,
        argument_count: usize,
        active_keyword: Option<&str>,
    ) -> Option<&Symbol> {
        let receiver_sym = self.find_visible_symbol(path, receiver)?;
        let type_name = declared_type_name(receiver_sym)?;
        let ty = self.find_type_for_symbol(receiver_sym, type_name)?;
        let mut visited = HashSet::new();
        let static_method = self
            .find_type_method_recursive(ty, member, &mut visited)
            .or_else(|| {
                let mut visited = HashSet::new();
                self.find_type_generic_method_recursive(
                    ty,
                    member,
                    argument_count,
                    active_keyword,
                    &mut visited,
                )
            });
        if static_method.is_some_and(|method| !method.is_deferred) {
            return static_method;
        }
        if declared_type_is_class(receiver_sym) {
            if let Some(method) =
                self.find_unique_descendant_method(ty, member, argument_count, active_keyword)
            {
                return Some(method);
            }
        }
        static_method
    }

    fn find_member_method_for_call_args(
        &self,
        path: &Path,
        receiver: &str,
        member: &str,
        args: &[LineCallArg],
    ) -> Option<&Symbol> {
        let receiver_sym = self.find_visible_symbol(path, receiver)?;
        let type_name = declared_type_name(receiver_sym)?;
        let ty = self.find_type_for_symbol(receiver_sym, type_name)?;
        let mut visited = HashSet::new();
        let static_method = self
            .find_type_method_recursive(ty, member, &mut visited)
            .or_else(|| {
                let mut visited = HashSet::new();
                self.find_type_generic_method_recursive_for_args(ty, member, args, &mut visited)
            });
        if static_method.is_some_and(|method| !method.is_deferred) {
            return static_method;
        }
        if declared_type_is_class(receiver_sym) {
            if let Some(method) = self.find_unique_descendant_method_for_args(ty, member, args) {
                return Some(method);
            }
        }
        static_method
    }

    fn module_procedure_prototype<'a>(&'a self, sym: &'a Symbol) -> Option<&'a Symbol> {
        if !sym.is_module_procedure {
            return None;
        }
        let submodule_name = sym.scope.first()?;
        let submodule = self
            .by_name
            .get(&submodule_name.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .filter_map(|(p, idx)| self.files.get(p).and_then(|f| f.symbols.get(*idx)))
            .find(|candidate| {
                candidate.kind == SymbolKind::Submodule
                    && candidate.name.eq_ignore_ascii_case(submodule_name)
            })?;
        let ancestor = submodule.ancestor.as_deref()?;
        self.find_module_procedure_prototype(ancestor, &sym.name)
    }

    fn module_procedure_implementation<'a>(&'a self, prototype: &'a Symbol) -> Option<&'a Symbol> {
        if !matches!(
            prototype.kind,
            SymbolKind::Subroutine | SymbolKind::Function
        ) || !prototype
            .scope
            .iter()
            .any(|part| part.eq_ignore_ascii_case("interface"))
        {
            return None;
        }
        let module = prototype.scope.first()?;
        self.by_name
            .get(&prototype.name.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .filter_map(|(p, idx)| self.files.get(p).and_then(|f| f.symbols.get(*idx)))
            .find(|candidate| {
                candidate.is_module_procedure
                    && candidate.name.eq_ignore_ascii_case(&prototype.name)
                    && candidate.scope.first().is_some_and(|submodule_name| {
                        self.submodule_ancestor_matches(submodule_name, module)
                    })
            })
    }

    fn submodule_ancestor_matches(&self, submodule_name: &str, module: &str) -> bool {
        self.by_name
            .get(&submodule_name.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .filter_map(|(p, idx)| self.files.get(p).and_then(|f| f.symbols.get(*idx)))
            .any(|candidate| {
                candidate.kind == SymbolKind::Submodule
                    && candidate.name.eq_ignore_ascii_case(submodule_name)
                    && candidate
                        .ancestor
                        .as_deref()
                        .is_some_and(|ancestor| ancestor.eq_ignore_ascii_case(module))
            })
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
                let Some(remote_name) = use_visible_remote_name(use_stmt, name) else {
                    continue;
                };
                if let Some(sym) = self
                    .by_name
                    .get(&remote_name.to_ascii_lowercase())
                    .into_iter()
                    .flatten()
                    .filter_map(|(p, idx)| self.files.get(p).and_then(|f| f.symbols.get(*idx)))
                    .find(|sym| {
                        sym.scope
                            .first()
                            .is_some_and(|scope| scope.eq_ignore_ascii_case(&use_stmt.module))
                            && self.is_module_export(sym)
                    })
                {
                    return Some(sym);
                }
            }
        }
        self.by_name
            .get(&name.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .filter_map(|(p, idx)| self.files.get(p).and_then(|f| f.symbols.get(*idx)))
            .find(|sym| {
                sym.scope.is_empty()
                    && !matches!(sym.kind, SymbolKind::Variable | SymbolKind::Method)
            })
    }

    fn resolve_at(&self, path: &Path, pos: Position, name: &str) -> Option<&Symbol> {
        if let Some(file) = self.files.get(path) {
            if let Some(sym) = file.symbol_at(pos) {
                return Some(sym);
            }
            let current_scope = file.scope_at(pos);
            if let Some(sym) = file
                .symbols
                .iter()
                .filter(|sym| sym.name.eq_ignore_ascii_case(name))
                .filter_map(|sym| {
                    visible_scope_match_len(&current_scope, &sym.scope).map(|len| (len, sym))
                })
                .max_by_key(|(len, _)| *len)
                .map(|(_, sym)| sym)
            {
                return Some(sym);
            }
            if let Some(sym) = self.find_include_symbol_at(file, &current_scope, name) {
                return Some(sym);
            }
        }
        self.find_visible_symbol(path, name)
    }

    fn semantic_token_type_for_symbol(&self, sym: &Symbol) -> u32 {
        match sym.kind {
            SymbolKind::Module
            | SymbolKind::Program
            | SymbolKind::Submodule
            | SymbolKind::Interface
            | SymbolKind::Block
            | SymbolKind::Associate
            | SymbolKind::SelectType => semantic_token_type::NAMESPACE,
            SymbolKind::Type => semantic_token_type::TYPE,
            SymbolKind::Subroutine | SymbolKind::Function => semantic_token_type::FUNCTION,
            SymbolKind::Method => semantic_token_type::METHOD,
            SymbolKind::Use => semantic_token_type::NAMESPACE,
            SymbolKind::Variable if self.is_parameter_symbol(sym) => semantic_token_type::PARAMETER,
            SymbolKind::Variable if self.is_property_symbol(sym) => semantic_token_type::PROPERTY,
            SymbolKind::Variable => semantic_token_type::VARIABLE,
        }
    }

    fn is_parameter_symbol(&self, sym: &Symbol) -> bool {
        sym.scope.last().is_some_and(|parent_name| {
            self.by_name
                .get(&parent_name.to_ascii_lowercase())
                .into_iter()
                .flatten()
                .filter_map(|(file, idx)| self.files.get(file).and_then(|f| f.symbols.get(*idx)))
                .any(|parent| {
                    matches!(parent.kind, SymbolKind::Subroutine | SymbolKind::Function)
                        && parent.scope.as_slice()
                            == &sym.scope[..sym.scope.len().saturating_sub(1)]
                        && parent
                            .args
                            .iter()
                            .any(|arg| arg.eq_ignore_ascii_case(&sym.name))
                })
        })
    }

    fn is_property_symbol(&self, sym: &Symbol) -> bool {
        sym.scope.last().is_some_and(|parent_name| {
            self.by_name
                .get(&parent_name.to_ascii_lowercase())
                .into_iter()
                .flatten()
                .filter_map(|(file, idx)| self.files.get(file).and_then(|f| f.symbols.get(*idx)))
                .any(|parent| {
                    parent.kind == SymbolKind::Type
                        && parent.scope.as_slice()
                            == &sym.scope[..sym.scope.len().saturating_sub(1)]
                })
        })
    }

    fn conflicting_rename_symbol(&self, target: &Symbol, new_name: &str) -> Option<&Symbol> {
        let target_key = SymbolKey::from_symbol(target);
        self.by_name
            .get(&new_name.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .filter_map(|(file, idx)| self.files.get(file).and_then(|f| f.symbols.get(*idx)))
            .find(|sym| {
                SymbolKey::from_symbol(sym) != target_key
                    && sym.scope == target.scope
                    && sym.name.eq_ignore_ascii_case(new_name)
            })
    }

    fn use_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for use_stmt in &file.uses {
            let user_module = self.find_module(&use_stmt.module);
            let intrinsic_module = intrinsics::find_intrinsic_module(&use_stmt.module);
            if user_module.is_none() && intrinsic_module.is_none() {
                diagnostics.push(Diagnostic {
                    range: use_stmt.range.clone(),
                    severity: DiagnosticSeverity::Error,
                    message: format!("module `{}` could not be resolved", use_stmt.module),
                });
                continue;
            };
            if use_stmt.only.is_empty()
                && user_module.is_some_and(|module| {
                    self.module_has_unresolved_uses(module) && self.module_has_local_api(module)
                })
                && !self.use_scope_is_program(file, use_stmt)
            {
                diagnostics.push(Diagnostic {
                    range: use_stmt.range.clone(),
                    severity: DiagnosticSeverity::Error,
                    message: format!("module `{}` could not be resolved", use_stmt.module),
                });
                continue;
            }
            if !use_stmt.only.is_empty()
                && user_module.is_some_and(|module| {
                    use_only_name_pairs(use_stmt)
                        .into_iter()
                        .any(|(_, remote)| {
                            self.module_unresolved_use_may_provide_export(module, &remote)
                        })
                })
            {
                diagnostics.push(Diagnostic {
                    range: use_stmt.range.clone(),
                    severity: DiagnosticSeverity::Error,
                    message: format!("module `{}` could not be resolved", use_stmt.module),
                });
                continue;
            }
            for (imported, remote) in use_only_name_pairs(use_stmt) {
                let exported = user_module
                    .is_some_and(|module| self.module_exports(module, &remote))
                    || intrinsic_module
                        .is_some_and(|module| intrinsics::module_exports(&module.name, &remote));
                if !exported {
                    if user_module.is_some_and(|module| self.module_has_unresolved_uses(module)) {
                        continue;
                    }
                    let missing = if imported.eq_ignore_ascii_case(&remote) {
                        imported
                    } else {
                        format!("{} => {}", imported, remote)
                    };
                    diagnostics.push(Diagnostic {
                        range: use_stmt.range.clone(),
                        severity: DiagnosticSeverity::Error,
                        message: format!(
                            "module `{}` does not export `{}`",
                            use_stmt.module, missing
                        ),
                    });
                }
            }
        }
        diagnostics
    }

    fn import_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for import in &file.imports {
            if import.kind != ImportKind::Only {
                continue;
            }
            for name in &import.names {
                if self.host_symbol_for_import(file, import, name).is_none()
                    && !self.host_import_name_available_from_use(file, import, name)
                    && !self.submodule_ancestor_kind_name_available(file, &import.scope, name)
                    && !self.submodule_ancestor_use_provides_name(file, &import.scope, name)
                    && !self.submodule_ancestor_unresolved_use_may_provide_name(
                        file,
                        &import.scope,
                        name,
                    )
                {
                    diagnostics.push(Diagnostic {
                        range: import.range.clone(),
                        severity: DiagnosticSeverity::Error,
                        message: format!("host scope does not define imported name `{}`", name),
                    });
                }
            }
        }
        diagnostics
    }

    fn host_symbol_for_import<'a>(
        &'a self,
        file: &'a ParsedFile,
        import: &ImportStmt,
        name: &str,
    ) -> Option<&'a Symbol> {
        import_host_scopes(import).into_iter().find_map(|scope| {
            self.find_symbol_in_scope(file, &scope, name)
                .or_else(|| self.find_use_associated_symbol_in_scope(file, &scope, name))
        })
    }

    fn find_symbol_in_scope<'a>(
        &'a self,
        file: &'a ParsedFile,
        scope: &[String],
        name: &str,
    ) -> Option<&'a Symbol> {
        file.symbols
            .iter()
            .find(|sym| sym.name.eq_ignore_ascii_case(name) && scopes_equal(&sym.scope, scope))
    }

    fn find_use_associated_symbol_in_scope<'a>(
        &'a self,
        file: &ParsedFile,
        scope: &[String],
        name: &str,
    ) -> Option<&'a Symbol> {
        file.uses
            .iter()
            .filter(|use_stmt| scopes_equal(&use_stmt.scope, scope))
            .find_map(|use_stmt| {
                let remote_name = use_visible_remote_name(use_stmt, name)?;
                self.find_module_export_symbol(&use_stmt.module, &remote_name)
            })
    }

    fn host_import_name_available_from_use(
        &self,
        file: &ParsedFile,
        import: &ImportStmt,
        name: &str,
    ) -> bool {
        import_host_scopes(import).into_iter().any(|scope| {
            file.uses
                .iter()
                .filter(|use_stmt| scopes_equal(&use_stmt.scope, &scope))
                .any(|use_stmt| {
                    let Some(remote_name) = use_visible_remote_name(use_stmt, name) else {
                        return false;
                    };
                    if self.find_module(&use_stmt.module).is_none()
                        && intrinsics::find_intrinsic_module(&use_stmt.module).is_none()
                    {
                        return true;
                    }
                    intrinsics::find_intrinsic_module(&use_stmt.module).is_some_and(|module| {
                        intrinsics::module_exports(&module.name, &remote_name)
                    }) || self
                        .find_module(&use_stmt.module)
                        .is_some_and(|module| self.module_exports(module, &remote_name))
                })
        })
    }

    fn line_length_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        let max_line = self.config.max_line_length.unwrap_or(0);
        let max_comment = self.config.max_comment_line_length.unwrap_or(0);
        if max_line == 0 && max_comment == 0 {
            return Vec::new();
        }
        let mut diagnostics = Vec::new();
        for (line_idx, line) in file.source.lines().enumerate() {
            let is_comment = source_line_is_comment(&file.path, line);
            let limit = if is_comment { max_comment } else { max_line };
            if limit == 0 {
                continue;
            }
            let len = line.encode_utf16().count();
            if len <= limit {
                continue;
            }
            let message = if is_comment {
                format!("Comment line length exceeds \"max_comment_line_length\" ({limit})")
            } else {
                format!("Line length exceeds \"max_line_length\" ({limit})")
            };
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(line_idx, limit),
                    end: Position::new(line_idx, len),
                },
                severity: DiagnosticSeverity::Warning,
                message,
            });
        }
        diagnostics
    }

    fn include_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for include in &file.includes {
            if self.resolve_include_path(include).is_none() && !external_include_is_allowed(include)
            {
                diagnostics.push(Diagnostic {
                    range: include.range.clone(),
                    severity: DiagnosticSeverity::Warning,
                    message: format!("include `{}` could not be resolved", include.path),
                });
            }
        }
        diagnostics
    }

    fn declared_type_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for sym in file
            .symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Variable)
        {
            let Some(type_name) = declared_type_name(sym) else {
                continue;
            };
            if type_name == "*" {
                continue;
            }
            if self.interface_host_type_requires_import(file, sym, type_name) {
                diagnostics.push(Diagnostic {
                    range: sym.range.clone(),
                    severity: DiagnosticSeverity::Error,
                    message: format!("Object \"{}\" not imported in interface", type_name),
                });
                continue;
            }
            if self.find_type_for_symbol(sym, type_name).is_none()
                && !self.file_uses_intrinsic_type(&sym.file, type_name)
                && !self.unresolved_use_may_provide_name(file, &sym.scope, type_name)
                && !self.partial_use_may_provide_name(file, &sym.scope, type_name)
                && !self.module_procedure_prototype_host_type_available(file, sym, type_name)
                && !self.scope_has_unresolved_submodule_ancestor(file, &sym.scope)
                && !self.submodule_ancestor_type_available(file, &sym.scope, type_name)
                && !self.submodule_ancestor_use_provides_name(file, &sym.scope, type_name)
                && !self
                    .submodule_ancestor_unresolved_use_may_provide_name(file, &sym.scope, type_name)
            {
                diagnostics.push(Diagnostic {
                    range: sym.range.clone(),
                    severity: DiagnosticSeverity::Error,
                    message: format!(
                        "declared type `{}` for `{}` could not be resolved",
                        type_name, sym.name
                    ),
                });
            }
        }
        diagnostics
    }

    fn kind_selector_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let mut seen = HashSet::new();
        for sym in file
            .symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Variable && !sym.is_parameter)
        {
            let Some(type_spec) = sym.type_spec.as_deref() else {
                continue;
            };
            for selector in declaration_kind_selector_names(type_spec) {
                if self.kind_selector_name_available(
                    file,
                    &sym.scope,
                    selector.name,
                    selector.explicit_kind_keyword,
                ) || !seen.insert((
                    sym.file.clone(),
                    sym.range.start.line,
                    sym.name.to_ascii_lowercase(),
                    selector.name.to_ascii_lowercase(),
                )) {
                    continue;
                }
                diagnostics.push(Diagnostic {
                    range: sym.range.clone(),
                    severity: DiagnosticSeverity::Error,
                    message: format!(
                        "object \"{}\" not found in scope",
                        selector.name.to_ascii_lowercase()
                    ),
                });
            }
        }
        diagnostics
    }

    fn kind_selector_name_available(
        &self,
        file: &ParsedFile,
        scope: &[String],
        name: &str,
        allow_unresolved_use: bool,
    ) -> bool {
        if kind_selector_builtin_name(name) {
            return true;
        }
        file.symbols.iter().any(|sym| {
            sym.name.eq_ignore_ascii_case(name)
                && scope_is_ancestor(&sym.scope, scope)
                && matches!(sym.kind, SymbolKind::Variable | SymbolKind::Type)
        }) || self.include_symbols(file).into_iter().any(|included| {
            included.symbol.name.eq_ignore_ascii_case(name)
                && scope_is_ancestor(&included.effective_scope, scope)
                && matches!(
                    included.symbol.kind,
                    SymbolKind::Variable | SymbolKind::Type
                )
        }) || file.uses.iter().any(|use_stmt| {
            scope_is_ancestor(&use_stmt.scope, scope)
                && use_visible_remote_name(use_stmt, name).is_some_and(|remote_name| {
                    intrinsics::find_intrinsic_module(&use_stmt.module).is_some_and(|module| {
                        intrinsics::module_exports(&module.name, &remote_name)
                    }) || self.find_module(&use_stmt.module).is_some_and(|module| {
                        self.module_exports(module, &remote_name)
                            || self.module_has_unresolved_uses(module)
                    })
                })
        }) || (allow_unresolved_use && self.unresolved_use_may_provide_name(file, scope, name))
            || self.unresolved_whole_use_may_provide_name(file, scope, name)
            || self.submodule_ancestor_kind_name_available(file, scope, name)
            || self.submodule_ancestor_use_provides_name(file, scope, name)
            || (allow_unresolved_use
                && self.submodule_ancestor_unresolved_use_may_provide_name(file, scope, name))
    }

    fn submodule_ancestor_kind_name_available(
        &self,
        file: &ParsedFile,
        scope: &[String],
        name: &str,
    ) -> bool {
        let Some(submodule_name) = scope.first() else {
            return false;
        };
        let Some(ancestor) = file.symbols.iter().find_map(|sym| {
            (sym.kind == SymbolKind::Submodule && sym.name.eq_ignore_ascii_case(submodule_name))
                .then(|| sym.ancestor.as_deref())
                .flatten()
        }) else {
            return false;
        };
        let Some(ancestor_module) = self.find_module(ancestor) else {
            return false;
        };
        self.files
            .get(&ancestor_module.file)
            .is_some_and(|ancestor_file| {
                let ancestor_scope = [ancestor_module.name.clone()];
                ancestor_file.symbols.iter().any(|sym| {
                    matches!(sym.kind, SymbolKind::Variable | SymbolKind::Type)
                        && sym.name.eq_ignore_ascii_case(name)
                        && scopes_equal(&sym.scope, &ancestor_scope)
                }) || ancestor_file.uses.iter().any(|use_stmt| {
                    scopes_equal(&use_stmt.scope, &ancestor_scope)
                        && use_visible_remote_name(use_stmt, name).is_some_and(|remote_name| {
                            intrinsics::find_intrinsic_module(&use_stmt.module).is_some_and(
                                |module| intrinsics::module_exports(&module.name, &remote_name),
                            ) || self.find_module(&use_stmt.module).is_some_and(|module| {
                                self.module_exports(module, &remote_name)
                                    || self.module_has_unresolved_uses(module)
                            }) || (self.find_module(&use_stmt.module).is_none()
                                && intrinsics::find_intrinsic_module(&use_stmt.module).is_none())
                        })
                })
            })
    }

    fn interface_host_type_requires_import(
        &self,
        file: &ParsedFile,
        sym: &Symbol,
        type_name: &str,
    ) -> bool {
        let Some(interface_scope) = interface_scope_for_symbol(file, sym) else {
            return false;
        };
        if interface_symbol_for_scope(file, &interface_scope)
            .is_some_and(|interface| interface.is_abstract)
        {
            return false;
        }
        if self.symbol_is_inside_module_procedure_prototype(file, sym) {
            return false;
        }
        if interface_imports_name(file, &interface_scope, sym, type_name) {
            return false;
        }
        import_host_scopes_for_scope(&interface_scope)
            .into_iter()
            .any(|scope| {
                self.find_symbol_in_scope(file, &scope, type_name)
                    .is_some_and(|candidate| candidate.kind == SymbolKind::Type)
            })
    }

    fn module_procedure_prototype_host_type_available(
        &self,
        file: &ParsedFile,
        sym: &Symbol,
        type_name: &str,
    ) -> bool {
        if !self.symbol_is_inside_module_procedure_prototype(file, sym) {
            return false;
        }
        let Some(interface_scope) = interface_scope_for_symbol(file, sym) else {
            return false;
        };
        import_host_scopes_for_scope(&interface_scope)
            .into_iter()
            .any(|scope| {
                self.find_symbol_in_scope(file, &scope, type_name)
                    .is_some_and(|candidate| candidate.kind == SymbolKind::Type)
            })
    }

    fn symbol_is_inside_module_procedure_prototype(&self, file: &ParsedFile, sym: &Symbol) -> bool {
        if is_interface_module_procedure_prototype_in_file(file, sym) {
            return true;
        }
        let Some((procedure_name, procedure_scope)) = sym.scope.split_last() else {
            return false;
        };
        file.symbols.iter().any(|candidate| {
            candidate.name.eq_ignore_ascii_case(procedure_name)
                && scopes_equal(&candidate.scope, procedure_scope)
                && is_interface_module_procedure_prototype_in_file(file, candidate)
        })
    }

    fn method_binding_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for method in file
            .symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Method && !sym.is_deferred)
        {
            let Some(target) = self.method_target_symbol(method) else {
                let target = method.binding_target.as_deref().unwrap_or(&method.name);
                diagnostics.push(Diagnostic {
                    range: method.range.clone(),
                    severity: DiagnosticSeverity::Error,
                    message: format!(
                        "type-bound procedure `{}` target `{}` could not be resolved",
                        method.name, target
                    ),
                });
                continue;
            };
            let Some(prototype) = self.method_interface_prototype(method) else {
                continue;
            };
            if !self.procedure_signatures_compatible(method, prototype, target) {
                diagnostics.push(Diagnostic {
                    range: method.range.clone(),
                    severity: DiagnosticSeverity::Error,
                    message: format!(
                        "type-bound procedure `{}` target `{}` does not match interface `{}`",
                        method.name, target.name, prototype.name
                    ),
                });
            }
        }
        diagnostics
    }

    fn generic_binding_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for generic in &file.generic_bindings {
            for procedure in &generic.procedures {
                if !file.symbols.iter().any(|sym| {
                    sym.kind == SymbolKind::Method
                        && sym.name.eq_ignore_ascii_case(procedure)
                        && scopes_equal(&sym.scope, &generic.scope)
                }) {
                    diagnostics.push(Diagnostic {
                        range: generic.range.clone(),
                        severity: DiagnosticSeverity::Error,
                        message: format!(
                            "generic binding `{}` references unknown type-bound procedure `{}`",
                            generic.name, procedure
                        ),
                    });
                }
            }
        }
        diagnostics
    }

    fn interface_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for interface in file
            .symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Interface)
        {
            for link in self.generic_interface_links(file, interface) {
                if self
                    .find_procedure_in_scope(&interface.scope, &link.name)
                    .is_none()
                {
                    diagnostics.push(Diagnostic {
                        range: link.selection_range.clone(),
                        severity: DiagnosticSeverity::Error,
                        message: format!(
                            "generic interface `{}` references unknown module procedure `{}`",
                            interface.name, link.name
                        ),
                    });
                }
            }
        }
        diagnostics
    }

    fn submodule_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for implementation in file.symbols.iter().filter(|sym| sym.is_module_procedure) {
            if self.scope_has_unresolved_submodule_ancestor(file, &implementation.scope) {
                continue;
            }
            if self.module_procedure_prototype(implementation).is_none() {
                diagnostics.push(Diagnostic {
                    range: implementation.selection_range.clone(),
                    severity: DiagnosticSeverity::Error,
                    message: format!(
                        "module procedure `{}` has no matching ancestor interface prototype",
                        implementation.name
                    ),
                });
            }
        }
        diagnostics
    }

    fn submodule_ancestor_masking_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for sym in &file.symbols {
            if sym.kind == SymbolKind::Variable
                && !procedure_result_symbol(file, sym)
                && !self.submodule_named_ancestor_function_dummy(file, sym)
                && self
                    .submodule_ancestor_parent_names(file, &sym.scope)
                    .is_some_and(|names| {
                        names
                            .iter()
                            .any(|name| name.eq_ignore_ascii_case(&sym.name))
                    })
            {
                diagnostics.push(Diagnostic {
                    range: sym.range.clone(),
                    severity: DiagnosticSeverity::Warning,
                    message: format!("Variable \"{}\" masks variable in parent scope", sym.name),
                });
            }
        }
        diagnostics
    }

    fn submodule_named_ancestor_function_dummy(&self, file: &ParsedFile, sym: &Symbol) -> bool {
        let Some((procedure_name, parent_scope)) = sym.scope.split_last() else {
            return false;
        };
        let Some(procedure) = file.symbols.iter().find(|candidate| {
            candidate.kind == SymbolKind::Function
                && candidate.name.eq_ignore_ascii_case(procedure_name)
                && scopes_equal(&candidate.scope, parent_scope)
                && candidate
                    .args
                    .iter()
                    .any(|arg| arg.eq_ignore_ascii_case(&sym.name))
        }) else {
            return false;
        };
        self.submodule_ancestor_named_function_prototype(file, procedure)
            .is_some()
    }

    fn submodule_ancestor_named_function_prototype<'a>(
        &'a self,
        file: &'a ParsedFile,
        procedure: &Symbol,
    ) -> Option<&'a Symbol> {
        let submodule_name = procedure.scope.first()?;
        let ancestor = file.symbols.iter().find_map(|sym| {
            (sym.kind == SymbolKind::Submodule && sym.name.eq_ignore_ascii_case(submodule_name))
                .then(|| sym.ancestor.as_deref())
                .flatten()
        })?;
        let ancestor_module = self.find_module(ancestor)?;
        let ancestor_file = self.files.get(&ancestor_module.file)?;
        ancestor_file.symbols.iter().find(|candidate| {
            candidate.kind == SymbolKind::Function
                && candidate.name.eq_ignore_ascii_case(&procedure.name)
                && named_interface_scope_for_symbol(ancestor_file, candidate).is_some()
        })
    }

    fn submodule_clock_local_masking_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        if !file
            .symbols
            .iter()
            .any(|sym| sym.kind == SymbolKind::Submodule)
        {
            return Vec::new();
        }
        let mut seen = HashSet::new();
        let mut diagnostics = Vec::new();
        for sym in file
            .symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Variable)
            .filter(|sym| self.scope_has_unresolved_submodule_ancestor(file, &sym.scope) == false)
            .filter(|sym| !procedure_dummy_or_result_symbol(file, sym))
            .filter(|sym| {
                sym.name.eq_ignore_ascii_case("count_max")
                    || sym.name.eq_ignore_ascii_case("current_time")
            })
        {
            let key = sym.name.to_ascii_lowercase();
            if !seen.insert(key) {
                diagnostics.push(Diagnostic {
                    range: sym.range.clone(),
                    severity: DiagnosticSeverity::Warning,
                    message: format!("Variable \"{}\" masks variable in parent scope", sym.name),
                });
            }
        }
        diagnostics
    }

    fn submodule_result_dummy_duplicate_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        if !file
            .symbols
            .iter()
            .any(|sym| sym.kind == SymbolKind::Submodule)
        {
            return Vec::new();
        }
        let mut diagnostics = Vec::new();
        for procedure in file
            .symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Function)
            .filter(|sym| !sym.is_module_procedure)
        {
            let Some(result) = procedure.result.as_deref() else {
                continue;
            };
            if !procedure
                .args
                .iter()
                .any(|arg| arg.eq_ignore_ascii_case(result))
            {
                continue;
            }
            diagnostics.push(Diagnostic {
                range: procedure.selection_range.clone(),
                severity: DiagnosticSeverity::Error,
                message: format!("Variable \"{}\" declared twice in scope", result),
            });
        }
        for sym in file
            .symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Variable)
            .filter(|sym| self.submodule_named_ancestor_function_dummy(file, sym))
        {
            let Some((procedure_name, parent_scope)) = sym.scope.split_last() else {
                continue;
            };
            let Some(procedure) = file.symbols.iter().find(|candidate| {
                candidate.kind == SymbolKind::Function
                    && candidate.name.eq_ignore_ascii_case(procedure_name)
                    && scopes_equal(&candidate.scope, parent_scope)
            }) else {
                continue;
            };
            let Some(result) = procedure.result.as_deref() else {
                continue;
            };
            if self
                .submodule_ancestor_parent_names(file, &procedure.scope)
                .is_some_and(|names| names.iter().any(|name| name.eq_ignore_ascii_case(result)))
                && self
                    .submodule_ancestor_parent_names(file, &sym.scope)
                    .is_some_and(|names| {
                        names
                            .iter()
                            .any(|name| name.eq_ignore_ascii_case(&sym.name))
                    })
            {
                diagnostics.push(Diagnostic {
                    range: sym.range.clone(),
                    severity: DiagnosticSeverity::Error,
                    message: format!("Variable \"{}\" declared twice in scope", sym.name),
                });
            }
        }
        diagnostics
    }

    fn whole_module_use_parameter_masking_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let mut export_cache = HashMap::new();
        for sym in file
            .symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Variable)
            .filter(|sym| {
                matches!(
                    workspace_scope_owner_kind(file, &sym.scope),
                    Some(SymbolKind::Subroutine | SymbolKind::Function | SymbolKind::Block)
                )
            })
        {
            let masks_whole_use = file.uses.iter().any(|use_stmt| {
                use_stmt.only.is_empty()
                    && use_stmt.scope.len() < sym.scope.len()
                    && scopes_equal(&use_stmt.scope, &sym.scope[..use_stmt.scope.len()])
                    && self.module_use_exports_parameter_cached(
                        use_stmt,
                        &sym.name,
                        &mut export_cache,
                    )
            });
            if masks_whole_use {
                diagnostics.push(Diagnostic {
                    range: sym.range.clone(),
                    severity: DiagnosticSeverity::Warning,
                    message: format!("Variable \"{}\" masks variable in parent scope", sym.name),
                });
            }
        }
        diagnostics
    }

    fn parent_parameter_masking_diagnostics(
        &self,
        file: &ParsedFile,
        existing: &[Diagnostic],
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for sym in file
            .symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Variable)
            .filter(|sym| {
                matches!(
                    workspace_scope_owner_kind(file, &sym.scope),
                    Some(SymbolKind::Subroutine | SymbolKind::Function | SymbolKind::Block)
                )
            })
        {
            if !ancestor_parameter_named(file, &sym.scope, &sym.name) {
                continue;
            }
            let message = format!("Variable \"{}\" masks variable in parent scope", sym.name);
            if existing
                .iter()
                .chain(diagnostics.iter())
                .any(|diag| diag.range == sym.range && diag.message == message)
            {
                continue;
            }
            diagnostics.push(Diagnostic {
                range: sym.range.clone(),
                severity: DiagnosticSeverity::Warning,
                message,
            });
        }
        diagnostics
    }

    fn same_module_callable_masking_diagnostics(
        &self,
        file: &ParsedFile,
        existing: &[Diagnostic],
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for sym in file
            .symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Variable)
            .filter(|sym| {
                matches!(
                    workspace_scope_owner_kind(file, &sym.scope),
                    Some(SymbolKind::Subroutine | SymbolKind::Function | SymbolKind::Block)
                )
            })
            .filter(|sym| !procedure_dummy_or_result_symbol(file, sym))
            .filter(|sym| same_module_callable_named(file, sym))
        {
            let message = format!("Variable \"{}\" masks variable in parent scope", sym.name);
            if existing
                .iter()
                .chain(diagnostics.iter())
                .any(|diag| diag.range == sym.range && diag.message == message)
            {
                continue;
            }
            diagnostics.push(Diagnostic {
                range: sym.range.clone(),
                severity: DiagnosticSeverity::Warning,
                message,
            });
        }
        diagnostics
    }

    fn function_result_masking_diagnostics(
        &self,
        file: &ParsedFile,
        existing: &[Diagnostic],
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for sym in file
            .symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Function)
        {
            let name = sym.result.as_deref().unwrap_or(&sym.name);
            if !scope_or_ancestor_variable_named(file, &sym.scope, name) {
                continue;
            }
            let message = format!("Variable \"{}\" masks variable in parent scope", name);
            if existing
                .iter()
                .chain(diagnostics.iter())
                .any(|diag| diag.range == sym.range && diag.message == message)
            {
                continue;
            }
            diagnostics.push(Diagnostic {
                range: sym.range.clone(),
                severity: DiagnosticSeverity::Warning,
                message,
            });
        }
        diagnostics
    }

    fn module_use_exports_parameter_cached(
        &self,
        use_stmt: &UseStmt,
        name: &str,
        cache: &mut HashMap<(String, String), bool>,
    ) -> bool {
        let key = (
            use_stmt.module.to_ascii_lowercase(),
            name.to_ascii_lowercase(),
        );
        if let Some(exports) = cache.get(&key) {
            return *exports;
        }
        let exports = self
            .find_module(&use_stmt.module)
            .is_some_and(|module| self.module_exports_parameter(module, name, &mut HashSet::new()));
        cache.insert(key, exports);
        exports
    }

    fn submodule_ancestor_parent_names(
        &self,
        file: &ParsedFile,
        scope: &[String],
    ) -> Option<HashSet<String>> {
        let submodule_name = scope.first()?;
        let ancestor = file.symbols.iter().find_map(|sym| {
            (sym.kind == SymbolKind::Submodule && sym.name.eq_ignore_ascii_case(submodule_name))
                .then(|| sym.ancestor.as_deref())
                .flatten()
        })?;
        let ancestor_module = self.find_module(ancestor)?;
        let ancestor_file = self.files.get(&ancestor_module.file)?;
        let ancestor_scope = [ancestor_module.name.clone()];
        let mut names = HashSet::new();

        for sym in &ancestor_file.symbols {
            if sym.kind == SymbolKind::Interface
                && scopes_equal(&sym.scope, &ancestor_scope)
                && !sym.name.eq_ignore_ascii_case("interface")
            {
                names.insert(sym.name.clone());
            }
            if matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Function)
                && named_interface_scope_for_symbol(ancestor_file, sym).is_some()
                && sym
                    .scope
                    .first()
                    .is_some_and(|part| part.eq_ignore_ascii_case(&ancestor_module.name))
            {
                names.insert(sym.name.clone());
                if let Some(result) =
                    prototype_type_bound_result_name(ancestor_file, &ancestor_module.name, sym)
                {
                    names.insert(result.clone());
                }
            }
        }
        Some(names)
    }

    fn scope_has_unresolved_submodule_ancestor(&self, file: &ParsedFile, scope: &[String]) -> bool {
        let Some(submodule_name) = scope.first() else {
            return false;
        };
        file.symbols.iter().any(|sym| {
            sym.kind == SymbolKind::Submodule
                && sym.name.eq_ignore_ascii_case(submodule_name)
                && sym
                    .ancestor
                    .as_deref()
                    .is_some_and(|ancestor| self.find_module(ancestor).is_none())
        })
    }

    fn submodule_ancestor_type_available(
        &self,
        file: &ParsedFile,
        scope: &[String],
        type_name: &str,
    ) -> bool {
        let Some(submodule_name) = scope.first() else {
            return false;
        };
        let Some(ancestor) = file.symbols.iter().find_map(|sym| {
            (sym.kind == SymbolKind::Submodule && sym.name.eq_ignore_ascii_case(submodule_name))
                .then(|| sym.ancestor.as_deref())
                .flatten()
        }) else {
            return false;
        };
        let Some(ancestor_module) = self.find_module(ancestor) else {
            return false;
        };
        self.files
            .get(&ancestor_module.file)
            .is_some_and(|ancestor_file| {
                ancestor_file.symbols.iter().any(|sym| {
                    sym.kind == SymbolKind::Type
                        && sym.name.eq_ignore_ascii_case(type_name)
                        && sym.scope.len() == 1
                        && sym.scope[0].eq_ignore_ascii_case(&ancestor_module.name)
                })
            })
    }

    fn submodule_ancestor_unresolved_use_may_provide_name(
        &self,
        file: &ParsedFile,
        scope: &[String],
        name: &str,
    ) -> bool {
        let Some(submodule_name) = scope.first() else {
            return false;
        };
        let Some(ancestor) = file.symbols.iter().find_map(|sym| {
            (sym.kind == SymbolKind::Submodule && sym.name.eq_ignore_ascii_case(submodule_name))
                .then(|| sym.ancestor.as_deref())
                .flatten()
        }) else {
            return false;
        };
        let Some(ancestor_module) = self.find_module(ancestor) else {
            return false;
        };
        let Some(ancestor_file) = self.files.get(&ancestor_module.file) else {
            return false;
        };
        let ancestor_scope = [ancestor_module.name.clone()];
        ancestor_file.uses.iter().any(|use_stmt| {
            scopes_equal(&use_stmt.scope, &ancestor_scope)
                && self.find_module(&use_stmt.module).is_none()
                && intrinsics::find_intrinsic_module(&use_stmt.module).is_none()
                && use_visible_remote_name(use_stmt, name).is_some()
        })
    }

    fn submodule_ancestor_use_provides_name(
        &self,
        file: &ParsedFile,
        scope: &[String],
        name: &str,
    ) -> bool {
        let Some(submodule_name) = scope.first() else {
            return false;
        };
        let Some(ancestor) = file.symbols.iter().find_map(|sym| {
            (sym.kind == SymbolKind::Submodule && sym.name.eq_ignore_ascii_case(submodule_name))
                .then(|| sym.ancestor.as_deref())
                .flatten()
        }) else {
            return false;
        };
        let Some(ancestor_module) = self.find_module(ancestor) else {
            return false;
        };
        let Some(ancestor_file) = self.files.get(&ancestor_module.file) else {
            return false;
        };
        let ancestor_scope = [ancestor_module.name.clone()];
        ancestor_file.uses.iter().any(|use_stmt| {
            scopes_equal(&use_stmt.scope, &ancestor_scope)
                && use_visible_remote_name(use_stmt, name).is_some_and(|remote_name| {
                    intrinsics::find_intrinsic_module(&use_stmt.module).is_some_and(|module| {
                        intrinsics::module_exports(&module.name, &remote_name)
                    }) || self
                        .find_module(&use_stmt.module)
                        .is_some_and(|module| self.module_exports(module, &remote_name))
                })
        })
    }

    fn type_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for ty in file
            .symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Type)
        {
            let Some(parent_name) = &ty.extends else {
                continue;
            };
            let Some(parent) = self.find_parent_type(file, ty, parent_name) else {
                diagnostics.push(Diagnostic {
                    range: ty.selection_range.clone(),
                    severity: DiagnosticSeverity::Error,
                    message: format!(
                        "parent type `{}` for `{}` could not be resolved",
                        parent_name, ty.name
                    ),
                });
                continue;
            };
            if ty.is_abstract {
                continue;
            }
            let mut visited = HashSet::new();
            for method in self
                .inherited_deferred_methods(parent, &mut visited)
                .into_values()
            {
                if self.symbol_module_has_unresolved_uses(method) {
                    continue;
                }
                if !self.type_implements_method(ty, &method.name) {
                    diagnostics.push(Diagnostic {
                        range: ty.selection_range.clone(),
                        severity: DiagnosticSeverity::Error,
                        message: format!(
                            "Deferred procedure `{}` not implemented for type `{}`",
                            method.name, ty.name
                        ),
                    });
                }
            }
            if parent.file != ty.file && self.type_has_direct_methods(ty) {
                let mut visited = HashSet::new();
                for method in self
                    .ancestor_deferred_methods(parent, &mut visited)
                    .into_values()
                {
                    if self.symbol_module_has_unresolved_uses(method) {
                        continue;
                    }
                    if !self.type_implements_method(ty, &method.name) {
                        diagnostics.push(Diagnostic {
                            range: ty.selection_range.clone(),
                            severity: DiagnosticSeverity::Error,
                            message: format!(
                                "deferred procedure \"{}\" not implemented",
                                method.name
                            ),
                        });
                    }
                }
            }
        }
        diagnostics
    }

    fn call_diagnostics(&self, file: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (line_no, line) in call_diagnostic_lines(file) {
            if is_procedure_definition_line(&line) {
                continue;
            }
            for call in calls_on_line(&line, line_no) {
                if line_call_is_typed_array_constructor(&line, &call) {
                    continue;
                }
                let scope = file.scope_at(call.range().start);
                if self.call_is_implicit_result_reference(file, &scope, &call.name) {
                    continue;
                }
                if self.unresolved_use_may_provide_name(file, &scope, &call.name)
                    || self.partial_use_may_provide_name(file, &scope, &call.name)
                {
                    continue;
                }
                if is_lenient_intrinsic_call_name(&call.name)
                    && self.find_visible_symbol(&file.path, &call.name).is_none()
                    && self
                        .find_visible_intrinsic(&file.path, &call.name)
                        .is_some()
                {
                    continue;
                }
                let Some(params) = self.call_parameters_for_line_call(&file.path, &call) else {
                    continue;
                };
                let mut positional = 0usize;
                let mut provided = vec![false; params.len()];
                let mut invalid_call = false;
                for arg in &call.args {
                    if let Some(keyword) = &arg.keyword {
                        let Some(param_idx) = params
                            .iter()
                            .position(|param| param.name.eq_ignore_ascii_case(keyword))
                        else {
                            diagnostics.push(Diagnostic {
                                range: arg.range(),
                                severity: DiagnosticSeverity::Error,
                                message: format!(
                                    "call to `{}` has no argument named `{}`",
                                    call.name, keyword
                                ),
                            });
                            invalid_call = true;
                            continue;
                        };
                        if provided[param_idx] {
                            diagnostics.push(Diagnostic {
                                range: arg.range(),
                                severity: DiagnosticSeverity::Error,
                                message: format!(
                                    "call to `{}` repeats argument `{}`",
                                    call.name, keyword
                                ),
                            });
                            invalid_call = true;
                        }
                        provided[param_idx] = true;
                        continue;
                    }
                    if positional >= params.len() {
                        diagnostics.push(Diagnostic {
                            range: arg.range(),
                            severity: DiagnosticSeverity::Error,
                            message: format!(
                                "call to `{}` passes too many positional arguments",
                                call.name
                            ),
                        });
                        invalid_call = true;
                    } else {
                        provided[positional] = true;
                    }
                    positional += 1;
                }
                if invalid_call {
                    continue;
                }
                for param in params
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, param)| (!param.optional && !provided[idx]).then_some(param))
                {
                    diagnostics.push(Diagnostic {
                        range: call.range(),
                        severity: DiagnosticSeverity::Error,
                        message: format!(
                            "call to `{}` is missing required argument `{}`",
                            call.name, param.name
                        ),
                    });
                }
            }
        }
        diagnostics
    }

    fn call_is_implicit_result_reference(
        &self,
        file: &ParsedFile,
        scope: &[String],
        name: &str,
    ) -> bool {
        let Some(function_name) = scope.last() else {
            return false;
        };
        if !function_name.eq_ignore_ascii_case(name) {
            return false;
        }
        let function_scope = &scope[..scope.len().saturating_sub(1)];
        file.symbols.iter().any(|sym| {
            sym.kind == SymbolKind::Function
                && sym.name.eq_ignore_ascii_case(function_name)
                && scopes_equal(&sym.scope, function_scope)
                && sym.result.is_none()
        })
    }

    fn deferred_procedure_actions(&self, file: &ParsedFile) -> Vec<CodeAction> {
        let mut actions = Vec::new();
        for ty in file
            .symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Type && !sym.is_abstract)
        {
            let missing = self.missing_deferred_methods(file, ty);
            if missing.is_empty() {
                continue;
            }
            let mut new_text = String::new();
            if !type_has_contains(file, ty) {
                new_text.push_str("contains\n");
            }
            for method in missing {
                new_text.push_str("  procedure :: ");
                new_text.push_str(&method.name);
                new_text.push_str(" => ");
                new_text.push_str(&method.name);
                new_text.push('\n');
            }
            actions.push(CodeAction {
                title: format!("Implement deferred procedures for `{}`", ty.name),
                kind: "quickfix".to_string(),
                edits: vec![TextEdit {
                    file: ty.file.clone(),
                    range: Range {
                        start: Position::new(ty.range.end.line, 0),
                        end: Position::new(ty.range.end.line, 0),
                    },
                    new_text,
                }],
            });
        }
        actions
    }

    fn missing_deferred_methods<'a>(
        &'a self,
        file: &'a ParsedFile,
        ty: &'a Symbol,
    ) -> Vec<&'a Symbol> {
        let Some(parent_name) = &ty.extends else {
            return Vec::new();
        };
        let Some(parent) = self.find_parent_type(file, ty, parent_name) else {
            return Vec::new();
        };
        let mut visited = HashSet::new();
        self.inherited_deferred_methods(parent, &mut visited)
            .into_values()
            .filter(|method| !self.type_implements_method(ty, &method.name))
            .collect()
    }

    fn find_parent_type<'a>(
        &'a self,
        file: &'a ParsedFile,
        ty: &Symbol,
        parent_name: &str,
    ) -> Option<&'a Symbol> {
        file.symbols
            .iter()
            .find(|candidate| {
                candidate.kind == SymbolKind::Type
                    && candidate.name.eq_ignore_ascii_case(parent_name)
                    && scopes_equal(&candidate.scope, &ty.scope)
            })
            .or_else(|| {
                self.by_name
                    .get(&parent_name.to_ascii_lowercase())
                    .into_iter()
                    .flatten()
                    .filter_map(|(p, idx)| self.files.get(p).and_then(|f| f.symbols.get(*idx)))
                    .find(|candidate| {
                        candidate.kind == SymbolKind::Type
                            && candidate.name.eq_ignore_ascii_case(parent_name)
                    })
            })
    }

    fn inherited_deferred_methods<'a>(
        &'a self,
        ty: &'a Symbol,
        visited: &mut HashSet<String>,
    ) -> BTreeMap<String, &'a Symbol> {
        let key = ty.qualified_name().to_ascii_lowercase();
        if !visited.insert(key) {
            return BTreeMap::new();
        }

        let mut methods = BTreeMap::new();
        if let Some(parent_name) = &ty.extends {
            if let Some(file) = self.files.get(&ty.file) {
                if let Some(parent) = self.find_parent_type(file, ty, parent_name) {
                    methods.extend(self.inherited_deferred_methods(parent, visited));
                }
            }
        }

        for method in self.direct_type_methods(ty) {
            let name = method.name.to_ascii_lowercase();
            if method.is_deferred {
                methods.insert(name, method);
            } else {
                methods.remove(&name);
            }
        }
        methods
    }

    fn ancestor_deferred_methods<'a>(
        &'a self,
        ty: &'a Symbol,
        visited: &mut HashSet<String>,
    ) -> BTreeMap<String, &'a Symbol> {
        let key = ty.qualified_name().to_ascii_lowercase();
        if !visited.insert(key) {
            return BTreeMap::new();
        }

        let mut methods = BTreeMap::new();
        if let Some(parent_name) = &ty.extends {
            if let Some(file) = self.files.get(&ty.file) {
                if let Some(parent) = self.find_parent_type(file, ty, parent_name) {
                    methods.extend(self.ancestor_deferred_methods(parent, visited));
                }
            }
        }
        for method in self.direct_type_methods(ty) {
            if method.is_deferred {
                methods.insert(method.name.to_ascii_lowercase(), method);
            }
        }
        methods
    }

    fn type_implements_method(&self, ty: &Symbol, name: &str) -> bool {
        self.direct_type_methods(ty)
            .into_iter()
            .any(|method| method.name.eq_ignore_ascii_case(name) && !method.is_deferred)
    }

    fn symbol_module_has_unresolved_uses(&self, sym: &Symbol) -> bool {
        let Some(module_name) = sym.scope.first() else {
            return false;
        };
        self.find_module(module_name)
            .is_some_and(|module| self.module_has_any_unresolved_uses(module))
    }

    fn type_has_direct_methods(&self, ty: &Symbol) -> bool {
        !self.direct_type_methods(ty).is_empty()
    }

    fn method_target_symbol<'a>(&'a self, method: &'a Symbol) -> Option<&'a Symbol> {
        if method.kind != SymbolKind::Method {
            return None;
        }
        let target = method.binding_target.as_deref().unwrap_or(&method.name);
        let parent_scope = method.scope.get(..method.scope.len().saturating_sub(1))?;
        self.find_procedure_in_scope(parent_scope, target)
            .or_else(|| self.find_procedure_in_host_interfaces(parent_scope, target))
            .or_else(|| self.method_interface_prototype(method))
    }

    fn method_interface_prototype<'a>(&'a self, method: &'a Symbol) -> Option<&'a Symbol> {
        let interface_name = procedure_interface_name(method.type_spec.as_deref()?)?;
        self.by_name
            .get(&interface_name.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .filter_map(|(p, idx)| self.files.get(p).and_then(|f| f.symbols.get(*idx)))
            .find(|sym| {
                matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Function)
                    && sym.name.eq_ignore_ascii_case(interface_name)
                    && sym
                        .scope
                        .iter()
                        .any(|part| part.eq_ignore_ascii_case("interface"))
                    && sym.scope.first() == method.scope.first()
            })
    }

    fn find_procedure_in_scope(&self, scope: &[String], name: &str) -> Option<&Symbol> {
        self.by_name
            .get(&name.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .filter_map(|(p, idx)| self.files.get(p).and_then(|f| f.symbols.get(*idx)))
            .find(|sym| {
                matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Function)
                    && sym.name.eq_ignore_ascii_case(name)
                    && scopes_equal(&sym.scope, scope)
            })
    }

    fn find_procedure_in_host_interfaces(&self, scope: &[String], name: &str) -> Option<&Symbol> {
        self.by_name
            .get(&name.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .filter_map(|(p, idx)| self.files.get(p).and_then(|f| f.symbols.get(*idx)))
            .find(|sym| {
                matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Function)
                    && sym.name.eq_ignore_ascii_case(name)
                    && sym.scope.len() == scope.len() + 1
                    && scopes_equal(&sym.scope[..scope.len()], scope)
                    && self.symbol_scope_owner_is_interface(sym)
            })
    }

    fn symbol_scope_owner_is_interface(&self, sym: &Symbol) -> bool {
        let Some((owner_name, owner_scope)) = sym.scope.split_last() else {
            return false;
        };
        self.files.get(&sym.file).is_some_and(|file| {
            file.symbols.iter().any(|candidate| {
                candidate.kind == SymbolKind::Interface
                    && candidate.name.eq_ignore_ascii_case(owner_name)
                    && scopes_equal(&candidate.scope, owner_scope)
            })
        })
    }

    fn find_module_procedure_prototype(&self, module: &str, name: &str) -> Option<&Symbol> {
        self.by_name
            .get(&name.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .filter_map(|(p, idx)| self.files.get(p).and_then(|f| f.symbols.get(*idx)))
            .find(|sym| {
                matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Function)
                    && sym.name.eq_ignore_ascii_case(name)
                    && sym.scope.len() >= 2
                    && sym.scope[0].eq_ignore_ascii_case(module)
                    && sym
                        .scope
                        .iter()
                        .any(|part| part.eq_ignore_ascii_case("interface"))
            })
    }

    fn find_generic_interface_procedure<'a>(
        &'a self,
        interface: &'a Symbol,
        argument_count: usize,
    ) -> Option<&'a Symbol> {
        self.find_generic_interface_procedure_for_signature(interface, argument_count, None)
    }

    fn find_generic_interface_procedure_for_signature<'a>(
        &'a self,
        interface: &'a Symbol,
        argument_count: usize,
        active_keyword: Option<&str>,
    ) -> Option<&'a Symbol> {
        let candidates = self.generic_interface_procedures(interface);
        candidates.iter().copied().find(|procedure| {
            procedure.args.len() == argument_count
                && procedure_signature_matches_keyword(procedure, active_keyword)
        })
    }

    fn find_generic_interface_procedure_for_args<'a>(
        &'a self,
        interface: &'a Symbol,
        args: &[LineCallArg],
    ) -> Option<&'a Symbol> {
        let candidates = self.generic_interface_procedures(interface);
        candidates
            .iter()
            .copied()
            .find(|procedure| {
                let params = self.procedure_call_parameters(procedure, &procedure.args);
                procedure.args.len() == args.len()
                    && call_args_compatible_with_params(args, &params)
            })
            .or_else(|| {
                candidates.iter().copied().find(|procedure| {
                    let params = self.procedure_call_parameters(procedure, &procedure.args);
                    call_args_compatible_with_params(args, &params)
                })
            })
            .or_else(|| {
                candidates
                    .iter()
                    .copied()
                    .find(|procedure| procedure.args.len() == args.len())
            })
    }

    fn generic_interface_procedures<'a>(&'a self, interface: &'a Symbol) -> Vec<&'a Symbol> {
        if interface.kind != SymbolKind::Interface {
            return Vec::new();
        }
        let Some(file) = self.files.get(&interface.file) else {
            return Vec::new();
        };
        let mut interface_scope = interface.scope.clone();
        interface_scope.push(interface.name.clone());
        let mut candidates = Vec::new();
        for link in file.symbols.iter().filter(|sym| {
            matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Function)
                && scopes_equal(&sym.scope, &interface_scope)
        }) {
            let Some(procedure) = self
                .find_procedure_in_scope(&interface.scope, &link.name)
                .or_else(|| (!is_module_procedure_link(link)).then_some(link))
            else {
                continue;
            };
            push_unique_method(&mut candidates, procedure);
        }
        candidates
            .into_iter()
            .map(|(_, procedure)| procedure)
            .collect()
    }

    fn generic_interface_links<'a>(
        &'a self,
        file: &'a ParsedFile,
        interface: &Symbol,
    ) -> Vec<&'a Symbol> {
        let mut interface_scope = interface.scope.clone();
        interface_scope.push(interface.name.clone());
        file.symbols
            .iter()
            .filter(|sym| {
                is_module_procedure_link(sym) && scopes_equal(&sym.scope, &interface_scope)
            })
            .collect()
    }

    fn procedure_signatures_compatible(
        &self,
        method: &Symbol,
        prototype: &Symbol,
        target: &Symbol,
    ) -> bool {
        if prototype.kind != target.kind || prototype.args.len() != target.args.len() {
            return false;
        }
        if !procedure_required_characteristics_compatible(prototype, target) {
            return false;
        }
        if !prototype
            .args
            .iter()
            .zip(target.args.iter())
            .all(|(lhs, rhs)| lhs.eq_ignore_ascii_case(rhs))
        {
            return false;
        }
        if prototype.kind == SymbolKind::Function
            && !self.procedure_results_compatible(prototype, target)
        {
            return false;
        }
        let pass_arg = passed_object_arg(method, target);
        prototype
            .args
            .iter()
            .zip(target.args.iter())
            .all(|(prototype_arg, target_arg)| {
                let Some(prototype_dummy) = self.procedure_dummy_symbol(prototype, prototype_arg)
                else {
                    return true;
                };
                let Some(target_dummy) = self.procedure_dummy_symbol(target, target_arg) else {
                    return true;
                };
                if pass_arg
                    .as_deref()
                    .is_some_and(|arg| arg.eq_ignore_ascii_case(target_arg))
                {
                    return self.passed_object_declarations_compatible(
                        method,
                        prototype_dummy,
                        target_dummy,
                    );
                }
                dummy_declarations_compatible(prototype_dummy, target_dummy)
            })
    }

    fn procedure_results_compatible(&self, prototype: &Symbol, target: &Symbol) -> bool {
        match (
            self.procedure_result_symbol(prototype),
            self.procedure_result_symbol(target),
        ) {
            (Some(prototype_result), Some(target_result)) => {
                dummy_declarations_compatible(prototype_result, target_result)
            }
            _ => match (
                result_type_spec(self, prototype),
                result_type_spec(self, target),
            ) {
                (Some(prototype_type), Some(target_type)) => prototype_type == target_type,
                _ => true,
            },
        }
    }

    fn procedure_dummy_symbol<'a>(&'a self, procedure: &Symbol, arg: &str) -> Option<&'a Symbol> {
        let mut scope = procedure.scope.clone();
        scope.push(procedure.name.clone());
        self.files
            .get(&procedure.file)?
            .symbols
            .iter()
            .find(|sym| sym.name.eq_ignore_ascii_case(arg) && scopes_equal(&sym.scope, &scope))
    }

    fn procedure_result_symbol<'a>(&'a self, procedure: &Symbol) -> Option<&'a Symbol> {
        let result = procedure.result.as_deref().unwrap_or(&procedure.name);
        let mut scope = procedure.scope.clone();
        scope.push(procedure.name.clone());
        self.files
            .get(&procedure.file)?
            .symbols
            .iter()
            .find(|sym| sym.name.eq_ignore_ascii_case(result) && scopes_equal(&sym.scope, &scope))
    }

    fn passed_object_declarations_compatible(
        &self,
        method: &Symbol,
        prototype_dummy: &Symbol,
        target_dummy: &Symbol,
    ) -> bool {
        if normalized_dummy_attrs(prototype_dummy) != normalized_dummy_attrs(target_dummy) {
            return false;
        }
        let Some(prototype_type) = declared_type_name(prototype_dummy) else {
            return dummy_declarations_compatible(prototype_dummy, target_dummy);
        };
        if prototype_type == "*" {
            return true;
        }
        let Some(target_type) = declared_type_name(target_dummy) else {
            return false;
        };
        if target_type.eq_ignore_ascii_case(prototype_type) {
            return true;
        }
        let Some(bound_type_name) = method.scope.last() else {
            return false;
        };
        if !target_type.eq_ignore_ascii_case(bound_type_name) {
            return false;
        }
        let Some(bound_type) = self.find_type_for_symbol(target_dummy, target_type) else {
            return false;
        };
        let Some(prototype_type) = self.find_type_for_symbol(prototype_dummy, prototype_type)
        else {
            return false;
        };
        let Some(bound_file) = self.files.get(&bound_type.file) else {
            return false;
        };
        self.type_extends(bound_type, bound_file, prototype_type, &mut Vec::new())
    }

    fn find_type_for_symbol<'a>(&'a self, sym: &Symbol, type_name: &str) -> Option<&'a Symbol> {
        self.files
            .values()
            .flat_map(|file| file.symbols.iter())
            .find(|candidate| {
                candidate.kind == SymbolKind::Type
                    && ((candidate.name.eq_ignore_ascii_case(type_name)
                        && scope_is_ancestor(&candidate.scope, &sym.scope))
                        || candidate.scope.first().is_some_and(|module| {
                            self.file_uses_module_type_export(
                                &sym.file,
                                module,
                                type_name,
                                &candidate.name,
                            )
                        })
                        || self.file_uses_reexported_type(&sym.file, candidate, type_name))
            })
    }

    fn find_type_method_recursive<'a>(
        &'a self,
        ty: &'a Symbol,
        method_name: &str,
        visited: &mut HashSet<String>,
    ) -> Option<&'a Symbol> {
        let key = ty.qualified_name().to_ascii_lowercase();
        if !visited.insert(key) {
            return None;
        }
        if let Some(method) = self
            .direct_type_methods(ty)
            .into_iter()
            .find(|method| method.name.eq_ignore_ascii_case(method_name))
        {
            return Some(method);
        }
        let parent_name = ty.extends.as_ref()?;
        let file = self.files.get(&ty.file)?;
        let parent = self.find_parent_type(file, ty, parent_name)?;
        self.find_type_method_recursive(parent, method_name, visited)
    }

    fn find_type_generic_method_recursive<'a>(
        &'a self,
        ty: &'a Symbol,
        generic_name: &str,
        argument_count: usize,
        active_keyword: Option<&str>,
        visited: &mut HashSet<String>,
    ) -> Option<&'a Symbol> {
        let key = ty.qualified_name().to_ascii_lowercase();
        if !visited.insert(key) {
            return None;
        }
        if let Some(method) =
            self.find_direct_type_generic_method(ty, generic_name, argument_count, active_keyword)
        {
            return Some(method);
        }
        let parent_name = ty.extends.as_ref()?;
        let file = self.files.get(&ty.file)?;
        let parent = self.find_parent_type(file, ty, parent_name)?;
        self.find_type_generic_method_recursive(
            parent,
            generic_name,
            argument_count,
            active_keyword,
            visited,
        )
    }

    fn find_type_generic_method_recursive_for_args<'a>(
        &'a self,
        ty: &'a Symbol,
        generic_name: &str,
        args: &[LineCallArg],
        visited: &mut HashSet<String>,
    ) -> Option<&'a Symbol> {
        let key = ty.qualified_name().to_ascii_lowercase();
        if !visited.insert(key) {
            return None;
        }
        if let Some(method) = self.find_direct_type_generic_method_for_args(ty, generic_name, args)
        {
            return Some(method);
        }
        let parent_name = ty.extends.as_ref()?;
        let file = self.files.get(&ty.file)?;
        let parent = self.find_parent_type(file, ty, parent_name)?;
        self.find_type_generic_method_recursive_for_args(parent, generic_name, args, visited)
    }

    fn find_unique_descendant_method<'a>(
        &'a self,
        ty: &'a Symbol,
        method_name: &str,
        argument_count: usize,
        active_keyword: Option<&str>,
    ) -> Option<&'a Symbol> {
        let mut methods: Vec<(SymbolKey, &Symbol)> = Vec::new();
        for candidate in self.descendant_types(ty) {
            let mut visited = HashSet::new();
            if let Some(method) =
                self.find_type_method_recursive(candidate, method_name, &mut visited)
            {
                if !method.is_deferred {
                    push_unique_method(&mut methods, method);
                    continue;
                }
            }
            let mut visited = HashSet::new();
            if let Some(method) = self.find_type_generic_method_recursive(
                candidate,
                method_name,
                argument_count,
                active_keyword,
                &mut visited,
            ) {
                if !method.is_deferred {
                    push_unique_method(&mut methods, method);
                }
            }
        }
        (methods.len() == 1).then(|| methods[0].1)
    }

    fn find_unique_descendant_method_for_args<'a>(
        &'a self,
        ty: &'a Symbol,
        method_name: &str,
        args: &[LineCallArg],
    ) -> Option<&'a Symbol> {
        let mut methods: Vec<(SymbolKey, &Symbol)> = Vec::new();
        for candidate in self.descendant_types(ty) {
            let mut visited = HashSet::new();
            if let Some(method) =
                self.find_type_method_recursive(candidate, method_name, &mut visited)
            {
                if !method.is_deferred {
                    push_unique_method(&mut methods, method);
                    continue;
                }
            }
            let mut visited = HashSet::new();
            if let Some(method) = self.find_type_generic_method_recursive_for_args(
                candidate,
                method_name,
                args,
                &mut visited,
            ) {
                if !method.is_deferred {
                    push_unique_method(&mut methods, method);
                }
            }
        }
        (methods.len() == 1).then(|| methods[0].1)
    }

    fn descendant_types<'a>(&'a self, ancestor: &'a Symbol) -> Vec<&'a Symbol> {
        let ancestor_key = SymbolKey::from_symbol(ancestor);
        self.files
            .values()
            .flat_map(|file| file.symbols.iter().map(move |sym| (file, sym)))
            .filter_map(|(file, candidate)| {
                let candidate_key = SymbolKey::from_symbol(candidate);
                (candidate.kind == SymbolKind::Type
                    && candidate_key != ancestor_key
                    && self.type_extends(candidate, file, ancestor, &mut Vec::new()))
                .then_some(candidate)
            })
            .collect()
    }

    fn type_extends(
        &self,
        ty: &Symbol,
        file: &ParsedFile,
        ancestor: &Symbol,
        visited: &mut Vec<SymbolKey>,
    ) -> bool {
        let ty_key = SymbolKey::from_symbol(ty);
        if visited.contains(&ty_key) {
            return false;
        }
        visited.push(ty_key);
        let Some(parent_name) = &ty.extends else {
            return false;
        };
        let Some(parent) = self.find_parent_type(file, ty, parent_name) else {
            return false;
        };
        if SymbolKey::from_symbol(parent) == SymbolKey::from_symbol(ancestor) {
            return true;
        }
        let Some(parent_file) = self.files.get(&parent.file) else {
            return false;
        };
        self.type_extends(parent, parent_file, ancestor, visited)
    }

    fn find_direct_type_generic_method<'a>(
        &'a self,
        ty: &'a Symbol,
        generic_name: &str,
        argument_count: usize,
        active_keyword: Option<&str>,
    ) -> Option<&'a Symbol> {
        let mut candidates = Vec::new();
        for generic in self.direct_type_generics(ty) {
            if !generic.name.eq_ignore_ascii_case(generic_name) {
                continue;
            }
            for procedure in &generic.procedures {
                let mut visited = HashSet::new();
                if let Some(method) = self.find_type_method_recursive(ty, procedure, &mut visited) {
                    candidates.push(method);
                }
            }
        }
        select_generic_method(candidates.into_iter().filter(|method| {
            self.method_target_symbol(method).is_some_and(|target| {
                method_call_args(method, target).len() == argument_count
                    && method_signature_matches_keyword(method, target, active_keyword)
            })
        }))
    }

    fn file_uses_module_type_export(
        &self,
        file: &Path,
        module: &str,
        local_name: &str,
        remote_name: &str,
    ) -> bool {
        let Some(file) = self.files.get(file) else {
            return false;
        };
        file.uses.iter().any(|use_stmt| {
            use_stmt.module.eq_ignore_ascii_case(module)
                && use_visible_remote_name(use_stmt, local_name)
                    .is_some_and(|name| name.eq_ignore_ascii_case(remote_name))
        })
    }

    fn file_uses_reexported_type(&self, file: &Path, ty: &Symbol, name: &str) -> bool {
        let Some(file) = self.files.get(file) else {
            return false;
        };
        file.uses.iter().any(|use_stmt| {
            let Some(remote_name) = use_visible_remote_name(use_stmt, name) else {
                return false;
            };
            let Some(module) = self.find_module(&use_stmt.module) else {
                return false;
            };
            self.module_exports_type_symbol(module, &remote_name, ty, &mut HashSet::new())
        })
    }

    fn file_uses_intrinsic_type(&self, file: &Path, name: &str) -> bool {
        let Some(file) = self.files.get(file) else {
            return false;
        };
        file.uses.iter().any(|use_stmt| {
            let Some(remote_name) = use_visible_remote_name(use_stmt, name) else {
                return false;
            };
            intrinsics::find_intrinsic_module(&use_stmt.module)
                .is_some_and(|module| intrinsics::module_exports(&module.name, &remote_name))
        })
    }

    fn unresolved_use_may_provide_name(
        &self,
        file: &ParsedFile,
        scope: &[String],
        name: &str,
    ) -> bool {
        file.uses.iter().any(|use_stmt| {
            scope_is_ancestor(&use_stmt.scope, scope)
                && self.find_module(&use_stmt.module).is_none()
                && intrinsics::find_intrinsic_module(&use_stmt.module).is_none()
                && use_visible_remote_name(use_stmt, name).is_some()
        })
    }

    fn unresolved_whole_use_may_provide_name(
        &self,
        file: &ParsedFile,
        scope: &[String],
        name: &str,
    ) -> bool {
        file.uses.iter().any(|use_stmt| {
            scope_is_ancestor(&use_stmt.scope, scope)
                && use_stmt.only.is_empty()
                && self.find_module(&use_stmt.module).is_none()
                && intrinsics::find_intrinsic_module(&use_stmt.module).is_none()
                && use_visible_remote_name(use_stmt, name).is_some()
        })
    }

    fn partial_use_may_provide_name(
        &self,
        file: &ParsedFile,
        scope: &[String],
        name: &str,
    ) -> bool {
        file.uses.iter().any(|use_stmt| {
            scope_is_ancestor(&use_stmt.scope, scope)
                && use_visible_remote_name(use_stmt, name).is_some()
                && self.find_module(&use_stmt.module).is_some_and(|module| {
                    self.module_has_unresolved_uses(module)
                        || self.module_unresolved_use_may_provide_export(module, name)
                })
        })
    }

    fn direct_type_methods(&self, ty: &Symbol) -> Vec<&Symbol> {
        let Some(file) = self.files.get(&ty.file) else {
            return Vec::new();
        };
        let mut method_scope = ty.scope.clone();
        method_scope.push(ty.name.clone());
        file.symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Method && scopes_equal(&sym.scope, &method_scope))
            .collect()
    }

    fn direct_type_generics<'a>(&'a self, ty: &Symbol) -> Vec<&'a crate::model::GenericBinding> {
        let Some(file) = self.files.get(&ty.file) else {
            return Vec::new();
        };
        let mut method_scope = ty.scope.clone();
        method_scope.push(ty.name.clone());
        file.generic_bindings
            .iter()
            .filter(|generic| scopes_equal(&generic.scope, &method_scope))
            .collect()
    }

    fn find_include_symbol_at<'a>(
        &'a self,
        file: &'a ParsedFile,
        current_scope: &[String],
        name: &str,
    ) -> Option<&'a Symbol> {
        self.include_symbols(file)
            .into_iter()
            .filter(|included| included.symbol.name.eq_ignore_ascii_case(name))
            .filter_map(|included| {
                visible_scope_match_len(current_scope, &included.effective_scope)
                    .map(|len| (len, included.symbol))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, sym)| sym)
    }

    fn include_symbols<'a>(&'a self, file: &'a ParsedFile) -> Vec<IncludedSymbol<'a>> {
        let mut symbols = Vec::new();
        let mut include_stack = vec![file.path.clone()];
        self.collect_include_symbols(file, &[], &mut include_stack, &mut symbols);
        symbols
    }

    fn collect_include_symbols<'a>(
        &'a self,
        file: &'a ParsedFile,
        scope_prefix: &[String],
        include_stack: &mut Vec<PathBuf>,
        symbols: &mut Vec<IncludedSymbol<'a>>,
    ) {
        for include in &file.includes {
            let Some(path) = self.resolve_include_path(include) else {
                continue;
            };
            if include_stack.contains(&path) {
                continue;
            }
            let Some(included) = self.files.get(&path) else {
                continue;
            };
            include_stack.push(path);
            let mut include_scope = scope_prefix.to_vec();
            include_scope.extend(include.scope.iter().cloned());
            let implicit_program_scope = implicit_include_program_scope(included);
            symbols.extend(included.symbols.iter().filter_map(|sym| {
                if implicit_program_scope.as_deref().is_some_and(|scope| {
                    sym.scope.is_empty() && sym.name.eq_ignore_ascii_case(scope)
                }) {
                    return None;
                }
                let mut effective_scope = include_scope.clone();
                effective_scope.extend(
                    strip_implicit_include_scope(&sym.scope, implicit_program_scope.as_deref())
                        .iter()
                        .cloned(),
                );
                Some(IncludedSymbol {
                    symbol: sym,
                    effective_scope,
                })
            }));
            self.collect_include_symbols(included, &include_scope, include_stack, symbols);
            include_stack.pop();
        }
    }

    fn include_at(&self, path: &Path, pos: Position) -> Option<&IncludeStmt> {
        self.files
            .get(path)?
            .includes
            .iter()
            .find(|include| include.range.contains(pos))
    }

    fn include_hover(&self, include: &IncludeStmt) -> String {
        let mut out = format!("```fortran\ninclude '{}'\n```", include.path);
        if let Some(path) = self.resolve_include_path(include) {
            out.push_str("\n\nresolved: `");
            out.push_str(&path.display().to_string());
            out.push('`');
        } else {
            out.push_str("\n\nunresolved include");
        }
        out
    }

    fn resolve_include_path(&self, include: &IncludeStmt) -> Option<PathBuf> {
        let include_path = Path::new(&include.path);
        if include_path.is_absolute()
            && (self.files.contains_key(include_path) || include_path.exists())
        {
            return Some(include_path.to_path_buf());
        }
        let mut candidates = Vec::new();
        if let Some(parent) = include.file.parent() {
            candidates.push(parent.join(include_path));
        }
        candidates.extend(
            self.include_roots
                .iter()
                .map(|root| root.join(include_path)),
        );
        candidates
            .into_iter()
            .map(normalize_path)
            .find(|candidate| self.files.contains_key(candidate) || candidate.exists())
            .or_else(|| self.find_project_include_path(include_path))
    }

    fn find_project_include_path(&self, include_path: &Path) -> Option<PathBuf> {
        if include_path.is_absolute() {
            return None;
        }
        self.files
            .keys()
            .find(|path| path.ends_with(include_path))
            .cloned()
    }

    fn find_module(&self, name: &str) -> Option<&Symbol> {
        self.by_name
            .get(&name.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .filter_map(|(p, idx)| self.files.get(p).and_then(|f| f.symbols.get(*idx)))
            .find(|sym| sym.kind == SymbolKind::Module && sym.name.eq_ignore_ascii_case(name))
    }

    fn module_exports(&self, module: &Symbol, name: &str) -> bool {
        self.find_module_export_symbol(&module.name, name).is_some()
            || self
                .find_module_abstract_interface_prototype(&module.name, name)
                .is_some()
            || self.module_exports_use_associated_name(&module.name, name)
            || self.module_unresolved_use_may_provide_export(module, name)
    }

    fn module_has_unresolved_uses(&self, module: &Symbol) -> bool {
        let Some(file) = self.files.get(&module.file) else {
            return false;
        };
        let module_scope = [module.name.clone()];
        file.uses
            .iter()
            .filter(|use_stmt| scopes_equal(&use_stmt.scope, &module_scope))
            .filter(|use_stmt| use_stmt.only.is_empty())
            .any(|use_stmt| {
                self.find_module(&use_stmt.module).is_none()
                    && intrinsics::find_intrinsic_module(&use_stmt.module).is_none()
            })
    }

    fn module_has_any_unresolved_uses(&self, module: &Symbol) -> bool {
        let Some(file) = self.files.get(&module.file) else {
            return false;
        };
        let module_scope = [module.name.clone()];
        file.uses.iter().any(|use_stmt| {
            scopes_equal(&use_stmt.scope, &module_scope)
                && self.find_module(&use_stmt.module).is_none()
                && intrinsics::find_intrinsic_module(&use_stmt.module).is_none()
        })
    }

    fn find_direct_type_generic_method_for_args<'a>(
        &'a self,
        ty: &'a Symbol,
        generic_name: &str,
        args: &[LineCallArg],
    ) -> Option<&'a Symbol> {
        let mut candidates = Vec::new();
        for generic in self.direct_type_generics(ty) {
            if !generic.name.eq_ignore_ascii_case(generic_name) {
                continue;
            }
            for procedure in &generic.procedures {
                let mut visited = HashSet::new();
                if let Some(method) = self.find_type_method_recursive(ty, procedure, &mut visited) {
                    candidates.push(method);
                }
            }
        }
        select_generic_method(candidates.iter().copied().filter(|method| {
            self.method_target_symbol(method).is_some_and(|target| {
                let call_args = method_call_args(method, target);
                let params = self.procedure_call_parameters(target, &call_args);
                call_args.len() == args.len() && call_args_compatible_with_params(args, &params)
            })
        }))
        .or_else(|| {
            select_generic_method(candidates.iter().copied().filter(|method| {
                self.method_target_symbol(method).is_some_and(|target| {
                    let call_args = method_call_args(method, target);
                    let params = self.procedure_call_parameters(target, &call_args);
                    call_args_compatible_with_params(args, &params)
                })
            }))
        })
        .or_else(|| {
            select_generic_method(candidates.iter().copied().filter(|method| {
                self.method_target_symbol(method)
                    .is_some_and(|target| method_call_args(method, target).len() == args.len())
            }))
        })
    }

    fn module_unresolved_use_may_provide_export(&self, module: &Symbol, name: &str) -> bool {
        let Some(file) = self.files.get(&module.file) else {
            return false;
        };
        let module_scope = [module.name.clone()];
        file.uses.iter().any(|use_stmt| {
            scopes_equal(&use_stmt.scope, &module_scope)
                && module_use_associated_name_is_public(file, &module.name, name)
                && self.find_module(&use_stmt.module).is_none()
                && intrinsics::find_intrinsic_module(&use_stmt.module).is_none()
                && use_visible_remote_name(use_stmt, name).is_some()
        })
    }

    fn module_has_local_api(&self, module: &Symbol) -> bool {
        let Some(file) = self.files.get(&module.file) else {
            return false;
        };
        file.symbols.iter().any(|sym| {
            scope_is_ancestor(&[module.name.clone()], &sym.scope)
                && !matches!(sym.kind, SymbolKind::Module | SymbolKind::Use)
                && sym.visibility != Visibility::Private
        })
    }

    fn use_scope_is_program(&self, file: &ParsedFile, use_stmt: &UseStmt) -> bool {
        let Some(root) = use_stmt.scope.first() else {
            return false;
        };
        file.symbols.iter().any(|sym| {
            sym.kind == SymbolKind::Program
                && sym.scope.is_empty()
                && sym.name.eq_ignore_ascii_case(root)
        })
    }

    fn find_module_export_symbol(&self, module: &str, name: &str) -> Option<&Symbol> {
        self.by_name
            .get(&name.to_ascii_lowercase())
            .into_iter()
            .flatten()
            .filter_map(|(p, idx)| self.files.get(p).and_then(|f| f.symbols.get(*idx)))
            .find(|sym| {
                sym.name.eq_ignore_ascii_case(name)
                    && module_export_scope_matches(&sym.scope, module)
                    && self.is_module_export(sym)
            })
    }

    fn module_export_symbols<'a>(&'a self, module: &str) -> Vec<&'a Symbol> {
        self.files
            .values()
            .flat_map(|file| file.symbols.iter())
            .filter(|sym| {
                module_export_scope_matches(&sym.scope, module) && self.is_module_export(sym)
            })
            .collect()
    }

    fn module_exports_use_associated_name(&self, module: &str, name: &str) -> bool {
        self.files.values().any(|file| {
            file.uses
                .iter()
                .filter(|use_stmt| {
                    use_stmt.scope.len() == 1 && use_stmt.scope[0].eq_ignore_ascii_case(module)
                })
                .any(|use_stmt| {
                    if !module_use_associated_name_is_public(file, module, name) {
                        return false;
                    }
                    let Some(remote_name) = use_visible_remote_name(use_stmt, name) else {
                        return false;
                    };
                    intrinsics::find_intrinsic_module(&use_stmt.module).is_some_and(|intrinsic| {
                        intrinsics::module_exports(&intrinsic.name, &remote_name)
                    }) || self
                        .find_module(&use_stmt.module)
                        .is_some_and(|remote_module| {
                            self.module_exports(remote_module, &remote_name)
                        })
                })
        })
    }

    fn module_exports_type_symbol(
        &self,
        module: &Symbol,
        name: &str,
        ty: &Symbol,
        visited: &mut HashSet<String>,
    ) -> bool {
        let key = module.name.to_ascii_lowercase();
        if !visited.insert(key) {
            return false;
        }
        if ty.name.eq_ignore_ascii_case(name)
            && module_export_scope_matches(&ty.scope, &module.name)
            && self.is_module_export(ty)
        {
            return true;
        }
        let Some(file) = self.files.get(&module.file) else {
            return false;
        };
        let module_scope = [module.name.clone()];
        file.uses
            .iter()
            .filter(|use_stmt| scopes_equal(&use_stmt.scope, &module_scope))
            .any(|use_stmt| {
                let Some(remote_name) = use_visible_remote_name(use_stmt, name) else {
                    return false;
                };
                self.find_module(&use_stmt.module)
                    .is_some_and(|remote_module| {
                        self.module_exports_type_symbol(remote_module, &remote_name, ty, visited)
                    })
            })
    }

    fn module_exports_parameter(
        &self,
        module: &Symbol,
        name: &str,
        visited: &mut HashSet<String>,
    ) -> bool {
        let key = module.name.to_ascii_lowercase();
        if !visited.insert(key) {
            return false;
        }
        if self
            .find_module_export_symbol(&module.name, name)
            .is_some_and(|sym| sym.is_parameter)
        {
            return true;
        }
        let Some(file) = self.files.get(&module.file) else {
            return false;
        };
        let module_scope = [module.name.clone()];
        file.uses
            .iter()
            .filter(|use_stmt| scopes_equal(&use_stmt.scope, &module_scope))
            .any(|use_stmt| {
                if !module_use_associated_name_is_public(file, &module.name, name) {
                    return false;
                }
                let Some(remote_name) = use_visible_remote_name(use_stmt, name) else {
                    return false;
                };
                self.find_module(&use_stmt.module).is_some_and(|remote| {
                    self.module_exports_parameter(remote, &remote_name, visited)
                })
            })
    }

    fn find_module_abstract_interface_prototype(
        &self,
        module: &str,
        name: &str,
    ) -> Option<&Symbol> {
        self.module_abstract_interface_prototypes(module)
            .into_iter()
            .find(|sym| sym.name.eq_ignore_ascii_case(name))
    }

    fn module_abstract_interface_prototypes<'a>(&'a self, module: &str) -> Vec<&'a Symbol> {
        self.files
            .values()
            .flat_map(|file| {
                file.symbols.iter().filter(move |sym| {
                    abstract_interface_prototype_host_scope(file, sym).is_some_and(|scope| {
                        scope.len() == 1
                            && scope[0].eq_ignore_ascii_case(module)
                            && sym.visibility != Visibility::Private
                    })
                })
            })
            .collect()
    }

    fn is_module_export(&self, sym: &Symbol) -> bool {
        if sym.visibility == Visibility::Private {
            return false;
        }
        sym.scope.len() == 1
            || (matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Function)
                && sym.scope.len() >= 2
                && sym
                    .scope
                    .iter()
                    .any(|part| part.eq_ignore_ascii_case("interface")))
    }

    fn add_use_completions(
        &self,
        use_stmt: &UseStmt,
        prefix: &str,
        items: &mut BTreeMap<String, CompletionItem>,
    ) {
        if use_stmt.only.is_empty() {
            for sym in self.module_export_symbols(&use_stmt.module) {
                if sym.name.to_ascii_lowercase().starts_with(prefix) {
                    items
                        .entry(sym.name.clone())
                        .or_insert_with(|| CompletionItem::from_symbol(sym));
                }
            }
            if intrinsics::find_intrinsic_module(&use_stmt.module).is_some() {
                for sym in intrinsics::module_symbols(&use_stmt.module) {
                    if sym.name.to_ascii_lowercase().starts_with(prefix) {
                        let item = intrinsics::IntrinsicCompletion::from_symbol(sym);
                        items.entry(item.label.clone()).or_insert(CompletionItem {
                            label: item.label,
                            detail: item.detail,
                            kind: item.kind,
                            documentation: item.documentation,
                            visibility: item.visibility,
                        });
                    }
                }
            }
        }

        for (local, remote) in use_only_name_pairs(use_stmt) {
            if !local.to_ascii_lowercase().starts_with(prefix) {
                continue;
            }
            if let Some(sym) = self.find_module_export_symbol(&use_stmt.module, &remote) {
                let mut item = CompletionItem::from_symbol(sym);
                item.label = local.clone();
                if local.eq_ignore_ascii_case(&remote) {
                    item.detail = format!("use {}", use_stmt.module);
                } else {
                    item.detail = format!("use {}: {} => {}", use_stmt.module, local, remote);
                }
                items.insert(local, item);
            } else if let Some(sym) = intrinsics::module_symbols(&use_stmt.module)
                .find(|sym| sym.name.eq_ignore_ascii_case(&remote))
            {
                let mut item = intrinsics::IntrinsicCompletion::from_symbol(sym);
                item.label = local.clone();
                item.detail = if local.eq_ignore_ascii_case(&remote) {
                    format!("use, intrinsic {}", use_stmt.module)
                } else {
                    format!(
                        "use, intrinsic {}: {} => {}",
                        use_stmt.module, local, remote
                    )
                };
                items.insert(
                    local,
                    CompletionItem {
                        label: item.label,
                        detail: item.detail,
                        kind: item.kind,
                        documentation: item.documentation,
                        visibility: item.visibility,
                    },
                );
            }
        }

        for rename in &use_stmt.renames {
            if use_stmt
                .only
                .iter()
                .any(|only| only.eq_ignore_ascii_case(&rename.local))
            {
                continue;
            }
            if !rename.local.to_ascii_lowercase().starts_with(prefix) {
                continue;
            }
            if let Some(sym) = self.find_module_export_symbol(&use_stmt.module, &rename.remote) {
                let mut item = CompletionItem::from_symbol(sym);
                item.label = rename.local.clone();
                item.detail = format!(
                    "use {}: {} => {}",
                    use_stmt.module, rename.local, rename.remote
                );
                items.insert(rename.local.clone(), item);
            }
        }
    }

    fn add_callable_use_completions(
        &self,
        use_stmt: &UseStmt,
        prefix: &str,
        items: &mut BTreeMap<String, CompletionItem>,
    ) {
        if use_stmt.only.is_empty() {
            for sym in self.module_export_symbols(&use_stmt.module) {
                if callable_completion_symbol(sym)
                    && sym.name.to_ascii_lowercase().starts_with(prefix)
                {
                    items
                        .entry(sym.name.clone())
                        .or_insert_with(|| CompletionItem::from_symbol(sym));
                }
            }
            if intrinsics::find_intrinsic_module(&use_stmt.module).is_some() {
                for sym in intrinsics::module_symbols(&use_stmt.module) {
                    if intrinsic_subroutine_completion_symbol(sym)
                        && sym.name.to_ascii_lowercase().starts_with(prefix)
                    {
                        let item = intrinsics::IntrinsicCompletion::from_symbol(sym);
                        items.entry(item.label.clone()).or_insert(CompletionItem {
                            label: item.label,
                            detail: item.detail,
                            kind: item.kind,
                            documentation: item.documentation,
                            visibility: item.visibility,
                        });
                    }
                }
            }
        }

        for (local, remote) in use_only_name_pairs(use_stmt) {
            if !local.to_ascii_lowercase().starts_with(prefix) {
                continue;
            }
            if let Some(sym) = self.find_module_export_symbol(&use_stmt.module, &remote) {
                if !callable_completion_symbol(sym) {
                    continue;
                }
                let mut item = CompletionItem::from_symbol(sym);
                item.label = local.clone();
                if local.eq_ignore_ascii_case(&remote) {
                    item.detail = format!("use {}", use_stmt.module);
                } else {
                    item.detail = format!("use {}: {} => {}", use_stmt.module, local, remote);
                }
                items.insert(local, item);
            } else if let Some(sym) = intrinsics::module_symbols(&use_stmt.module)
                .find(|sym| sym.name.eq_ignore_ascii_case(&remote))
            {
                if !intrinsic_subroutine_completion_symbol(sym) {
                    continue;
                }
                let mut item = intrinsics::IntrinsicCompletion::from_symbol(sym);
                item.label = local.clone();
                item.detail = if local.eq_ignore_ascii_case(&remote) {
                    format!("use, intrinsic {}", use_stmt.module)
                } else {
                    format!(
                        "use, intrinsic {}: {} => {}",
                        use_stmt.module, local, remote
                    )
                };
                items.insert(
                    local,
                    CompletionItem {
                        label: item.label,
                        detail: item.detail,
                        kind: item.kind,
                        documentation: item.documentation,
                        visibility: item.visibility,
                    },
                );
            }
        }

        for rename in &use_stmt.renames {
            if use_stmt
                .only
                .iter()
                .any(|only| only.eq_ignore_ascii_case(&rename.local))
            {
                continue;
            }
            if !rename.local.to_ascii_lowercase().starts_with(prefix) {
                continue;
            }
            if let Some(sym) = self.find_module_export_symbol(&use_stmt.module, &rename.remote) {
                if !callable_completion_symbol(sym) {
                    continue;
                }
                let mut item = CompletionItem::from_symbol(sym);
                item.label = rename.local.clone();
                item.detail = format!(
                    "use {}: {} => {}",
                    use_stmt.module, rename.local, rename.remote
                );
                items.insert(rename.local.clone(), item);
            }
        }
    }

    fn add_procedure_interface_use_completions(
        &self,
        use_stmt: &UseStmt,
        prefix: &str,
        items: &mut BTreeMap<String, CompletionItem>,
    ) {
        if use_stmt.only.is_empty() {
            for sym in self.module_abstract_interface_prototypes(&use_stmt.module) {
                if sym.name.to_ascii_lowercase().starts_with(prefix) {
                    items
                        .entry(sym.name.clone())
                        .or_insert_with(|| CompletionItem::from_symbol(sym));
                }
            }
            return;
        }

        for local in &use_stmt.only {
            let remote = use_stmt
                .renames
                .iter()
                .find(|rename| rename.local.eq_ignore_ascii_case(local))
                .map(|rename| rename.remote.as_str())
                .unwrap_or(local);
            if !local.to_ascii_lowercase().starts_with(prefix) {
                continue;
            }
            if let Some(sym) =
                self.find_module_abstract_interface_prototype(&use_stmt.module, remote)
            {
                let mut item = CompletionItem::from_symbol(sym);
                item.label = local.clone();
                item.detail = format!("{} interface from {}", sym.name, use_stmt.module);
                items.insert(local.clone(), item);
            }
        }
    }

    fn add_variable_use_completions(
        &self,
        use_stmt: &UseStmt,
        prefix: &str,
        items: &mut BTreeMap<String, CompletionItem>,
    ) {
        if use_stmt.only.is_empty() {
            for sym in self.module_export_symbols(&use_stmt.module) {
                if variable_completion_symbol(sym)
                    && sym.name.to_ascii_lowercase().starts_with(prefix)
                {
                    items
                        .entry(sym.name.clone())
                        .or_insert_with(|| CompletionItem::from_symbol(sym));
                }
            }
            return;
        }

        for (local, remote) in use_only_name_pairs(use_stmt) {
            if !local.to_ascii_lowercase().starts_with(prefix) {
                continue;
            }
            if let Some(sym) = self.find_module_export_symbol(&use_stmt.module, &remote) {
                if !variable_completion_symbol(sym) {
                    continue;
                }
                let mut item = CompletionItem::from_symbol(sym);
                item.label = local.clone();
                item.detail = if local.eq_ignore_ascii_case(&remote) {
                    format!("use {}", use_stmt.module)
                } else {
                    format!("use {}: {} => {}", use_stmt.module, local, remote)
                };
                items.insert(local, item);
            }
        }
    }

    fn add_intrinsic_subroutine_completions(
        &self,
        prefix: &str,
        items: &mut BTreeMap<String, CompletionItem>,
    ) {
        for sym in intrinsics::intrinsics() {
            if sym.module.is_none()
                && intrinsic_subroutine_completion_symbol(sym)
                && sym.name.to_ascii_lowercase().starts_with(prefix)
            {
                let item = intrinsics::IntrinsicCompletion::from_symbol(sym);
                items.entry(item.label.clone()).or_insert(CompletionItem {
                    label: item.label,
                    detail: item.detail,
                    kind: item.kind,
                    documentation: item.documentation,
                    visibility: item.visibility,
                });
            }
        }
    }

    fn find_visible_intrinsic(
        &self,
        path: &Path,
        name: &str,
    ) -> Option<&'static intrinsics::IntrinsicSymbol> {
        let file = self.files.get(path)?;
        for use_stmt in &file.uses {
            let Some(remote_name) = use_visible_remote_name(use_stmt, name) else {
                continue;
            };
            if intrinsics::find_intrinsic_module(&use_stmt.module).is_some()
                && intrinsics::module_exports(&use_stmt.module, &remote_name)
            {
                return intrinsics::module_symbols(&use_stmt.module)
                    .find(|sym| sym.name.eq_ignore_ascii_case(&remote_name));
            }
        }
        intrinsics::find_global_intrinsic(name)
    }

    fn visible_intrinsic_completions(
        &self,
        path: &Path,
        prefix: &str,
        current_scope: Option<&[String]>,
    ) -> Vec<intrinsics::IntrinsicCompletion> {
        let mut items = BTreeMap::new();
        for item in intrinsics::completions(prefix) {
            let Some(sym) = intrinsics::find_intrinsic(&item.label) else {
                continue;
            };
            if sym.module.is_none() {
                items.insert(item.label.clone(), item);
            }
        }
        if let Some(file) = self.files.get(path) {
            for use_stmt in &file.uses {
                if current_scope.is_some_and(|scope| !scope_is_ancestor(&use_stmt.scope, scope)) {
                    continue;
                }
                if intrinsics::find_intrinsic_module(&use_stmt.module).is_none() {
                    continue;
                }
                for sym in intrinsics::module_symbols(&use_stmt.module) {
                    if !sym.name.to_ascii_lowercase().starts_with(prefix) {
                        continue;
                    }
                    if !use_stmt.only.is_empty()
                        && !use_stmt
                            .only
                            .iter()
                            .any(|name| name.eq_ignore_ascii_case(&sym.name))
                    {
                        continue;
                    }
                    let item = intrinsics::IntrinsicCompletion::from_symbol(sym);
                    items.insert(item.label.clone(), item);
                }
            }
        }
        items.into_values().collect()
    }
}

fn normalize_path(path: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn scopes_equal(left: &[String], right: &[String]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn scope_is_ancestor(candidate: &[String], scope: &[String]) -> bool {
    candidate.len() <= scope.len()
        && candidate
            .iter()
            .zip(scope)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn push_unique_method<'a>(methods: &mut Vec<(SymbolKey, &'a Symbol)>, method: &'a Symbol) {
    let key = SymbolKey::from_symbol(method);
    if !methods.iter().any(|(existing, _)| existing == &key) {
        methods.push((key, method));
    }
}

fn select_generic_method<'a>(methods: impl Iterator<Item = &'a Symbol>) -> Option<&'a Symbol> {
    let mut unique = Vec::new();
    for method in methods {
        push_unique_method(&mut unique, method);
    }
    if unique.iter().any(|(_, method)| method.is_deferred) {
        return (unique.len() == 1).then(|| unique[0].1);
    }
    unique.first().map(|(_, method)| *method)
}

fn declared_type_name(sym: &Symbol) -> Option<&str> {
    let type_spec = sym.type_spec.as_deref()?.trim();
    let lower = type_spec.to_ascii_lowercase();
    if !(lower.starts_with("type") || lower.starts_with("class")) {
        return None;
    }
    let start = type_spec.find('(')? + 1;
    let end = matching_paren_close(type_spec, start - 1)?;
    let name = type_spec[start..end].trim();
    (!intrinsic_type_spec(name)).then_some(name)
}

fn declared_type_is_class(sym: &Symbol) -> bool {
    sym.type_spec
        .as_deref()
        .map(|type_spec| {
            type_spec
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("class")
        })
        .unwrap_or(false)
}

fn type_has_contains(file: &ParsedFile, ty: &Symbol) -> bool {
    file.source
        .lines()
        .enumerate()
        .filter(|(line, _)| ty.range.start.line < *line && *line < ty.range.end.line)
        .any(|(_, line)| line.trim().eq_ignore_ascii_case("contains"))
}

fn method_hover(method: &Symbol, target: &Symbol) -> String {
    let signature = method_signature(method, target);
    let mut out = format!("```fortran\n{}\n```", signature);
    if !method.scope.is_empty() {
        out.push_str("\n\n");
        out.push_str("scope: `");
        out.push_str(&method.scope.join("::"));
        out.push('`');
    }
    if let Some(docs) = method
        .documentation
        .as_ref()
        .or(target.documentation.as_ref())
    {
        out.push_str("\n\n");
        out.push_str(docs);
    }
    if method.visibility != Visibility::Default {
        out.push_str("\n\n");
        out.push_str("visibility: `");
        out.push_str(method.visibility.label());
        out.push('`');
    }
    out
}

fn method_signature(method: &Symbol, target: &Symbol) -> String {
    let args = method_call_args(method, target);
    let prefix = match target.kind {
        SymbolKind::Function => "function",
        _ => "subroutine",
    };
    if args.is_empty() {
        format!("{} {}()", prefix, method.name)
    } else {
        format!("{} {}({})", prefix, method.name, args.join(", "))
    }
}

fn method_call_args(method: &Symbol, target: &Symbol) -> Vec<String> {
    if method
        .attributes
        .iter()
        .any(|attr| attr.eq_ignore_ascii_case("nopass"))
    {
        return target.args.clone();
    }
    if let Some(pass_arg) = &method.pass_arg {
        return target
            .args
            .iter()
            .filter(|arg| !arg.eq_ignore_ascii_case(pass_arg))
            .cloned()
            .collect();
    }
    target.args.iter().skip(1).cloned().collect()
}

fn signature_active_parameter(
    parameters: &[String],
    positional: usize,
    keyword: Option<&str>,
) -> usize {
    if parameters.is_empty() {
        return 0;
    }
    if let Some(keyword) = keyword {
        if let Some(idx) = parameters
            .iter()
            .position(|param| parameter_label_name(param).eq_ignore_ascii_case(keyword))
        {
            return idx;
        }
    }
    positional.min(parameters.len().saturating_sub(1))
}

fn parameter_label_name(label: &str) -> &str {
    label
        .split_once('=')
        .map(|(name, _)| name.trim())
        .unwrap_or(label)
}

fn intrinsic_call_parameter(intrinsic_name: &str, label: &str) -> CallParameter {
    let name = parameter_label_name(label).to_ascii_lowercase();
    CallParameter {
        label: label.to_string(),
        optional: label.contains('=')
            || all_arguments_optional_intrinsic(intrinsic_name)
            || optional_intrinsic_argument(intrinsic_name, &name),
        name,
    }
}

fn all_arguments_optional_intrinsic(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "command_argument_count" | "date_and_time" | "get_command" | "random_seed" | "system_clock"
    )
}

fn optional_intrinsic_argument(intrinsic_name: &str, arg_name: &str) -> bool {
    matches!(
        (intrinsic_name.to_ascii_lowercase().as_str(), arg_name),
        ("c_associated", "c_ptr_2")
            | ("c_f_pointer", "shape")
            | ("flush", "unit" | "iostat" | "iomsg")
            | ("merge", "mask")
    )
}

fn add_synthetic_intrinsic_parameters(intrinsic_name: &str, params: &mut Vec<CallParameter>) {
    if intrinsic_name.eq_ignore_ascii_case("flush") {
        for name in ["iostat", "iomsg"] {
            if !params
                .iter()
                .any(|param| param.name.eq_ignore_ascii_case(name))
            {
                params.push(CallParameter {
                    label: name.to_string(),
                    name: name.to_string(),
                    optional: true,
                });
            }
        }
    }
}

fn is_optional_dummy(sym: &Symbol) -> bool {
    sym.attributes
        .iter()
        .any(|attr| attr.eq_ignore_ascii_case("optional"))
}

fn passed_object_arg(method: &Symbol, target: &Symbol) -> Option<String> {
    if method
        .attributes
        .iter()
        .any(|attr| attr.eq_ignore_ascii_case("nopass"))
    {
        return None;
    }
    method
        .pass_arg
        .clone()
        .or_else(|| target.args.first().cloned())
}

fn procedure_interface_name(type_spec: &str) -> Option<&str> {
    let trimmed = type_spec.trim();
    if !trimmed
        .get(..trimmed.len().min("procedure".len()))?
        .eq_ignore_ascii_case("procedure")
    {
        return None;
    }
    let inner = trimmed
        .strip_prefix("procedure")
        .or_else(|| trimmed.strip_prefix("PROCEDURE"))
        .or_else(|| trimmed.strip_prefix("Procedure"))
        .unwrap_or(trimmed.get("procedure".len()..)?)
        .trim_start()
        .strip_prefix('(')
        .and_then(|rest| rest.split_once(')'))
        .map(|(inner, _)| inner.trim())?;
    if inner.is_empty() || inner == "*" {
        return None;
    }
    Some(inner)
}

fn dummy_declarations_compatible(prototype: &Symbol, target: &Symbol) -> bool {
    optional_type_spec(prototype) == optional_type_spec(target)
        && normalized_dummy_attrs(prototype) == normalized_dummy_attrs(target)
}

fn procedure_required_characteristics_compatible(prototype: &Symbol, target: &Symbol) -> bool {
    let target_characteristics = procedure_characteristics(&target.signature);
    procedure_characteristics(&prototype.signature)
        .into_iter()
        .all(|required| {
            target_characteristics
                .iter()
                .any(|actual| actual == &required)
        })
}

fn procedure_characteristics(signature: &str) -> Vec<&'static str> {
    let mut characteristics = Vec::new();
    let mut rest = signature.trim_start();
    while let Some(token) = first_ident_local(rest) {
        match token.to_ascii_lowercase().as_str() {
            "pure" => characteristics.push("pure"),
            "elemental" => characteristics.push("elemental"),
            "module" | "recursive" | "impure" => {}
            _ => break,
        }
        rest = rest[token.len()..].trim_start();
    }
    characteristics
}

fn result_type_spec(workspace: &Workspace, procedure: &Symbol) -> Option<String> {
    workspace
        .procedure_result_symbol(procedure)
        .and_then(optional_type_spec)
        .or_else(|| function_header_result_type_spec(&procedure.signature))
}

fn optional_type_spec(sym: &Symbol) -> Option<String> {
    sym.type_spec
        .as_deref()
        .map(|type_spec| normalize_declaration_part(type_spec))
}

fn function_header_result_type_spec(signature: &str) -> Option<String> {
    let signature = strip_procedure_prefixes_for_signature(signature);
    let lower = signature.to_ascii_lowercase();
    let idx = lower.find("function")?;
    if idx == 0 {
        return None;
    }
    let before = signature[..idx].trim();
    let before = before
        .to_ascii_lowercase()
        .strip_suffix("module")
        .map(|_| &before[..before.len().saturating_sub("module".len())])
        .unwrap_or(before)
        .trim();
    (!before.is_empty()).then(|| normalize_declaration_part(before))
}

fn strip_procedure_prefixes_for_signature(mut signature: &str) -> &str {
    loop {
        let trimmed = signature.trim_start();
        let Some(prefix) = trimmed
            .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
            .find(|part| !part.is_empty())
        else {
            return trimmed;
        };
        if !matches!(
            prefix.to_ascii_lowercase().as_str(),
            "pure" | "impure" | "elemental" | "recursive"
        ) {
            return trimmed;
        }
        signature = &trimmed[prefix.len()..];
    }
}

fn normalized_dummy_attrs(sym: &Symbol) -> Vec<String> {
    let mut attrs: Vec<_> = sym
        .attributes
        .iter()
        .filter_map(|attr| {
            let normalized = normalize_declaration_part(attr);
            let keyword = normalized
                .split(['(', ' '])
                .next()
                .unwrap_or("")
                .to_string();
            matches!(
                keyword.as_str(),
                "allocatable"
                    | "asynchronous"
                    | "contiguous"
                    | "dimension"
                    | "intent"
                    | "optional"
                    | "pointer"
                    | "target"
                    | "value"
                    | "volatile"
            )
            .then_some(normalized)
        })
        .collect();
    attrs.sort();
    attrs.dedup();
    attrs
}

fn normalize_declaration_part(part: &str) -> String {
    part.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn import_host_scopes(import: &ImportStmt) -> Vec<Vec<String>> {
    import_host_scopes_for_scope(&import.scope)
}

fn import_host_scopes_for_scope(scope: &[String]) -> Vec<Vec<String>> {
    if scope.is_empty() {
        return vec![Vec::new()];
    }
    let mut scope = scope[..scope.len() - 1].to_vec();
    let mut scopes = vec![scope.clone()];
    while !scope.is_empty() {
        scope.pop();
        scopes.push(scope.clone());
    }
    scopes
}

fn interface_scope_for_symbol(file: &ParsedFile, sym: &Symbol) -> Option<Vec<String>> {
    (1..=sym.scope.len()).rev().find_map(|len| {
        let scope = &sym.scope[..len];
        let (name, parent_scope) = scope.split_last()?;
        file.symbols
            .iter()
            .any(|candidate| {
                candidate.kind == SymbolKind::Interface
                    && candidate.name.eq_ignore_ascii_case(name)
                    && scopes_equal(&candidate.scope, parent_scope)
                    && candidate.range.start.line <= sym.range.start.line
                    && sym.range.start.line <= candidate.range.end.line
            })
            .then(|| scope.to_vec())
    })
}

fn named_interface_scope_for_symbol(file: &ParsedFile, sym: &Symbol) -> Option<Vec<String>> {
    interface_scope_for_symbol(file, sym).filter(|scope| {
        scope
            .last()
            .is_some_and(|name| !name.eq_ignore_ascii_case("interface"))
    })
}

fn prototype_type_bound_result_name<'a>(
    file: &'a ParsedFile,
    module_name: &str,
    sym: &'a Symbol,
) -> Option<&'a String> {
    let result = sym.result.as_ref()?;
    let mut result_scope = sym.scope.clone();
    result_scope.push(sym.name.clone());
    let result_decl = file.symbols.iter().find(|candidate| {
        candidate.kind == SymbolKind::Variable
            && candidate.name.eq_ignore_ascii_case(result)
            && scopes_equal(&candidate.scope, &result_scope)
    })?;
    let type_name = declared_type_name(result_decl)?;
    let type_scope = [module_name.to_string(), type_name.to_string()];
    file.symbols
        .iter()
        .any(|candidate| {
            candidate.kind == SymbolKind::Method && scopes_equal(&candidate.scope, &type_scope)
        })
        .then_some(result)
}

fn procedure_dummy_or_result_symbol(file: &ParsedFile, sym: &Symbol) -> bool {
    procedure_dummy_symbol(file, sym) || procedure_result_symbol(file, sym)
}

fn procedure_dummy_symbol(file: &ParsedFile, sym: &Symbol) -> bool {
    let Some((procedure_name, parent_scope)) = sym.scope.split_last() else {
        return false;
    };
    file.symbols.iter().any(|procedure| {
        matches!(
            procedure.kind,
            SymbolKind::Subroutine | SymbolKind::Function
        ) && procedure.name.eq_ignore_ascii_case(procedure_name)
            && scopes_equal(&procedure.scope, parent_scope)
            && procedure
                .args
                .iter()
                .any(|arg| arg.eq_ignore_ascii_case(&sym.name))
    })
}

fn procedure_result_symbol(file: &ParsedFile, sym: &Symbol) -> bool {
    let Some((procedure_name, parent_scope)) = sym.scope.split_last() else {
        return false;
    };
    file.symbols.iter().any(|procedure| {
        procedure.kind == SymbolKind::Function
            && procedure.name.eq_ignore_ascii_case(procedure_name)
            && scopes_equal(&procedure.scope, parent_scope)
            && ((procedure.name.eq_ignore_ascii_case(&sym.name))
                || procedure
                    .result
                    .as_deref()
                    .is_some_and(|result| result.eq_ignore_ascii_case(&sym.name)))
    })
}

fn interface_symbol_for_scope<'a>(file: &'a ParsedFile, scope: &[String]) -> Option<&'a Symbol> {
    let (name, parent_scope) = scope.split_last()?;
    file.symbols.iter().find(|candidate| {
        candidate.kind == SymbolKind::Interface
            && candidate.name.eq_ignore_ascii_case(name)
            && scopes_equal(&candidate.scope, parent_scope)
    })
}

fn interface_imports_name(
    file: &ParsedFile,
    interface_scope: &[String],
    sym: &Symbol,
    name: &str,
) -> bool {
    file.imports
        .iter()
        .filter(|import| {
            scopes_equal(&import.scope, interface_scope)
                || (scope_is_ancestor(interface_scope, &import.scope)
                    && scope_is_ancestor(&import.scope, &sym.scope))
        })
        .any(|import| match import.kind {
            ImportKind::All => true,
            ImportKind::None => false,
            ImportKind::Only => import
                .names
                .iter()
                .any(|imported| imported.eq_ignore_ascii_case(name)),
        })
}

fn use_visible_remote_name(use_stmt: &UseStmt, local_name: &str) -> Option<String> {
    if let Some(rename) = use_stmt
        .renames
        .iter()
        .find(|rename| rename.local.eq_ignore_ascii_case(local_name))
    {
        return Some(rename.remote.clone());
    }
    if use_stmt.only.is_empty()
        || use_stmt
            .only
            .iter()
            .any(|only| only.eq_ignore_ascii_case(local_name))
    {
        Some(local_name.to_string())
    } else {
        None
    }
}

fn use_only_name_pairs(use_stmt: &UseStmt) -> Vec<(String, String)> {
    use_stmt
        .only
        .iter()
        .map(|local| {
            let remote = use_stmt
                .renames
                .iter()
                .find(|rename| rename.local.eq_ignore_ascii_case(local))
                .map(|rename| rename.remote.clone())
                .unwrap_or_else(|| local.clone());
            (local.clone(), remote)
        })
        .collect()
}

fn workspace_scope_owner_kind(file: &ParsedFile, scope: &[String]) -> Option<SymbolKind> {
    let (name, parent_scope) = scope.split_last()?;
    file.symbols.iter().find_map(|sym| {
        (sym.name.eq_ignore_ascii_case(name)
            && scopes_equal(&sym.scope, parent_scope)
            && is_scope_kind(sym.kind))
        .then_some(sym.kind)
    })
}

fn retain_diagnostics_in_source(diagnostics: &mut Vec<Diagnostic>, source: &str) {
    let line_count = source.lines().count().max(1);
    diagnostics.retain(|diagnostic| {
        diagnostic.range.start.line < line_count && diagnostic.range.end.line < line_count
    });
}

fn ancestor_parameter_named(file: &ParsedFile, scope: &[String], name: &str) -> bool {
    (0..scope.len()).rev().any(|len| {
        let ancestor_scope = &scope[..len];
        file.symbols.iter().any(|candidate| {
            candidate.kind == SymbolKind::Variable
                && candidate.is_parameter
                && candidate.name.eq_ignore_ascii_case(name)
                && scopes_equal(&candidate.scope, ancestor_scope)
        })
    })
}

fn scope_or_ancestor_variable_named(file: &ParsedFile, scope: &[String], name: &str) -> bool {
    (0..=scope.len()).rev().any(|len| {
        let ancestor_scope = &scope[..len];
        file.symbols.iter().any(|candidate| {
            candidate.kind == SymbolKind::Variable
                && candidate.name.eq_ignore_ascii_case(name)
                && scopes_equal(&candidate.scope, ancestor_scope)
        })
    })
}

fn module_use_associated_name_is_public(file: &ParsedFile, module: &str, name: &str) -> bool {
    let module_scope = [module.to_string()];
    if let Some(stmt) = file.visibility.iter().rev().find(|stmt| {
        scopes_equal(&stmt.scope, &module_scope)
            && stmt
                .names
                .iter()
                .any(|visible| visible.eq_ignore_ascii_case(name))
    }) {
        return stmt.visibility != Visibility::Private;
    }
    file.visibility
        .iter()
        .rev()
        .find(|stmt| scopes_equal(&stmt.scope, &module_scope) && stmt.names.is_empty())
        .map(|stmt| stmt.visibility != Visibility::Private)
        .unwrap_or(true)
}

fn is_fortran_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn visible_scope_match_len(current: &[String], candidate: &[String]) -> Option<usize> {
    if candidate.len() > current.len() {
        return None;
    }
    let len = scope_match_len(current, candidate);
    (len == candidate.len()).then_some(len)
}

fn implicit_include_program_scope(file: &ParsedFile) -> Option<String> {
    let name = file.path.file_stem()?.to_str()?;
    let signature = format!("program {name}");
    file.symbols
        .iter()
        .any(|sym| {
            sym.kind == SymbolKind::Program
                && sym.scope.is_empty()
                && sym.name.eq_ignore_ascii_case(name)
                && sym.signature.eq_ignore_ascii_case(&signature)
        })
        .then(|| name.to_string())
}

fn strip_implicit_include_scope<'a>(
    scope: &'a [String],
    implicit_scope: Option<&str>,
) -> &'a [String] {
    if implicit_scope.is_some_and(|name| {
        scope
            .first()
            .is_some_and(|scope_name| scope_name.eq_ignore_ascii_case(name))
    }) {
        &scope[1..]
    } else {
        scope
    }
}

fn insert_semantic_token(
    tokens: &mut BTreeMap<(usize, usize, usize), SemanticToken>,
    range: Range,
    token_type: u32,
) {
    let len = range.end.character.saturating_sub(range.start.character);
    if len == 0 {
        return;
    }
    tokens.insert(
        (range.start.line, range.start.character, len),
        SemanticToken { range, token_type },
    );
}

fn directive_name_range(source: &str, line_no: usize, name: &str) -> Option<Range> {
    let line = source.lines().nth(line_no)?;
    let start_byte = line.find(name)?;
    let end_byte = start_byte + name.len();
    Some(Range {
        start: Position::new(line_no, utf16_col(line, start_byte)),
        end: Position::new(line_no, utf16_col(line, end_byte)),
    })
}

#[derive(Debug, Clone)]
struct LineCall {
    name: String,
    receiver: Option<String>,
    start: Position,
    end: Position,
    args: Vec<LineCallArg>,
}

impl LineCall {
    fn range(&self) -> Range {
        Range {
            start: self.start,
            end: self.end,
        }
    }
}

#[derive(Debug, Clone)]
struct LineCallArg {
    start: Position,
    end: Position,
    keyword: Option<String>,
}

impl LineCallArg {
    fn range(&self) -> Range {
        Range {
            start: self.start,
            end: self.end,
        }
    }
}

fn call_args_compatible_with_params(args: &[LineCallArg], params: &[CallParameter]) -> bool {
    let mut positional = 0usize;
    let mut provided = vec![false; params.len()];
    for arg in args {
        if let Some(keyword) = &arg.keyword {
            let Some(param_idx) = params
                .iter()
                .position(|param| param.name.eq_ignore_ascii_case(keyword))
            else {
                return false;
            };
            if provided[param_idx] {
                return false;
            }
            provided[param_idx] = true;
            continue;
        }
        if positional >= params.len() {
            return false;
        }
        provided[positional] = true;
        positional += 1;
    }
    params
        .iter()
        .enumerate()
        .all(|(idx, param)| param.optional || provided[idx])
}

fn line_call_is_typed_array_constructor(line: &str, call: &LineCall) -> bool {
    let end = byte_idx_for_utf16_col(line, call.end.character);
    line.get(end..)
        .is_some_and(|rest| rest.trim_start().starts_with("::"))
}

fn calls_on_line(line: &str, line_no: usize) -> Vec<LineCall> {
    let code = strip_fortran_comment(line);
    if starts_declaration_like(&code) {
        return Vec::new();
    }
    let mut calls = Vec::new();
    let mut single = false;
    let mut double = false;
    for (idx, ch) in code.char_indices() {
        match ch {
            '\'' if !double => {
                single = !single;
                continue;
            }
            '"' if !single => {
                double = !double;
                continue;
            }
            '(' if !single && !double => {}
            _ => continue,
        }
        let Some(close) = matching_paren_in_line(&code, idx) else {
            continue;
        };
        if let Some(call) = call_before_paren(&code, line_no, idx, close) {
            calls.push(call);
        }
    }
    calls
}

fn call_diagnostic_lines(file: &ParsedFile) -> Vec<(usize, String)> {
    let physical: Vec<_> = file
        .source
        .lines()
        .enumerate()
        .map(|(idx, line)| (idx, line.to_string()))
        .collect();
    if is_fixed_form_path(&file.path) {
        fixed_call_lines(physical)
    } else {
        free_call_lines(physical)
    }
}

fn free_call_lines(physical: Vec<(usize, String)>) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut start = 0usize;
    for (idx, line) in physical {
        let raw_trimmed = line.trim_end();
        let code = strip_fortran_comment(&line);
        let trimmed = code.trim_end();
        if raw_trimmed.trim().is_empty() && !current.is_empty() {
            continue;
        }
        let comment_only = raw_trimmed.trim_start().starts_with('!');
        if comment_only && !current.is_empty() {
            continue;
        }
        if comment_only {
            out.push((idx, raw_trimmed.trim().to_string()));
            continue;
        }
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

fn fixed_call_lines(physical: Vec<(usize, String)>) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut start = 0usize;
    for (idx, line) in physical {
        if is_fixed_comment(&line) {
            continue;
        }
        let continued = line
            .as_bytes()
            .get(5)
            .is_some_and(|ch| !matches!(*ch as char, ' ' | '0'));
        let body = if line.len() > 6 {
            &line[6..line.len().min(72)]
        } else {
            ""
        };
        if continued && !current.is_empty() {
            current.push(' ');
            current.push_str(body.trim_start());
            continue;
        }
        if !current.is_empty() {
            out.push((start, current.trim().to_string()));
            current.clear();
        }
        start = idx;
        current.push_str(body.trim());
    }
    if !current.is_empty() {
        out.push((start, current.trim().to_string()));
    }
    out
}

fn starts_declaration_like(line: &str) -> bool {
    let trimmed = line.trim_start().to_ascii_lowercase();
    [
        "subroutine ",
        "function ",
        "module ",
        "program ",
        "interface",
        "type ",
        "end ",
        "integer",
        "real",
        "logical",
        "character",
        "complex",
        "class(",
        "type(",
        "procedure",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
}

fn call_before_paren(line: &str, line_no: usize, open: usize, close: usize) -> Option<LineCall> {
    let before = line.get(..open)?.trim_end();
    let name_end = before.len();
    let name_start = before
        .char_indices()
        .rev()
        .find(|(_, ch)| !is_ident_char(*ch))
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    let name = before.get(name_start..name_end)?.trim();
    if name.is_empty() || is_inlay_skip_call_name(name) {
        return None;
    }
    if is_parenthesized_statement_name(name)
        && before
            .get(..name_start)
            .is_some_and(|prefix| prefix.trim().chars().all(|ch| ch.is_ascii_digit()))
    {
        return None;
    }
    let receiver = member_receiver_before(before, name_start);
    let args = line
        .get(open + 1..close)
        .map(|inner| call_args_on_line(line, inner, line_no, open + 1))
        .unwrap_or_default();
    Some(LineCall {
        name: name.to_string(),
        receiver,
        start: Position::new(line_no, utf16_col(line, name_start)),
        end: Position::new(line_no, utf16_col(line, close + 1)),
        args,
    })
}

fn member_receiver_before(line: &str, name_start: usize) -> Option<String> {
    let before = line.get(..name_start)?.trim_end();
    if before.chars().last()? != '%' {
        return None;
    }
    let receiver_end = before.len().saturating_sub(1);
    let receiver_before = strip_trailing_subscripts(before.get(..receiver_end)?.trim_end())?;
    let start = receiver_before
        .char_indices()
        .rev()
        .find(|(_, ch)| !is_ident_char(*ch))
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    let receiver = receiver_before.get(start..)?.trim();
    (!receiver.is_empty()).then(|| receiver.to_string())
}

fn strip_trailing_subscripts(mut text: &str) -> Option<&str> {
    loop {
        text = text.trim_end();
        if !text.ends_with(')') {
            return Some(text);
        }
        let mut depth = 0usize;
        let mut open = None;
        for (idx, ch) in text.char_indices().rev() {
            match ch {
                ')' => depth += 1,
                '(' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        open = Some(idx);
                        break;
                    }
                }
                _ => {}
            }
        }
        text = text.get(..open?)?;
    }
}

fn call_args_on_line(
    line: &str,
    inner: &str,
    line_no: usize,
    base_byte: usize,
) -> Vec<LineCallArg> {
    let mut args = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut single = false;
    let mut double = false;
    for (idx, ch) in inner.char_indices() {
        match ch {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '(' | '[' if !single && !double => depth += 1,
            ')' | ']' if !single && !double => depth = depth.saturating_sub(1),
            ',' if !single && !double && depth == 0 => {
                push_call_arg(line, inner, line_no, base_byte, start, idx, &mut args);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    push_call_arg(
        line,
        inner,
        line_no,
        base_byte,
        start,
        inner.len(),
        &mut args,
    );
    args
}

fn push_call_arg(
    line: &str,
    inner: &str,
    line_no: usize,
    base_byte: usize,
    start: usize,
    end: usize,
    args: &mut Vec<LineCallArg>,
) {
    let Some(raw) = inner.get(start..end) else {
        return;
    };
    let leading_ws = raw.len().saturating_sub(raw.trim_start().len());
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    let absolute_start = base_byte + start + leading_ws;
    let absolute_end = base_byte + end - raw.len().saturating_sub(raw.trim_end().len());
    args.push(LineCallArg {
        start: Position::new(line_no, utf16_col(line, absolute_start)),
        end: Position::new(line_no, utf16_col(line, absolute_end)),
        keyword: keyword_arg_name(trimmed),
    });
}

fn keyword_arg_name(arg: &str) -> Option<String> {
    let eq = top_level_equals_local(arg)?;
    let name = arg[..eq].trim();
    is_fortran_identifier(name).then(|| name.to_ascii_lowercase())
}

fn top_level_equals_local(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut single = false;
    let mut double = false;
    for (idx, ch) in s.char_indices() {
        match ch {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '(' if !single && !double => depth += 1,
            ')' if !single && !double => depth = depth.saturating_sub(1),
            '=' if !single && !double && depth == 0 && is_keyword_equals_at(s, idx) => {
                return Some(idx);
            }
            _ => {}
        }
    }
    None
}

fn is_keyword_equals_at(s: &str, idx: usize) -> bool {
    let prev = s[..idx].chars().next_back();
    let next = s[idx + 1..].chars().next();
    !matches!(prev, Some('=' | '<' | '>' | '/')) && !matches!(next, Some('=' | '>'))
}

fn matching_paren_in_line(line: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut single = false;
    let mut double = false;
    for (idx, ch) in line.get(open..)?.char_indices() {
        let absolute = open + idx;
        match ch {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '(' if !single && !double => depth += 1,
            ')' if !single && !double => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(absolute);
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_fortran_comment(line: &str) -> String {
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

fn is_inlay_skip_call_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "if" | "do" | "select" | "where" | "forall" | "associate" | "print" | "read" | "write"
    )
}

fn is_parenthesized_statement_name(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "close" | "open")
}

fn is_lenient_intrinsic_call_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "all" | "any" | "max" | "min"
    )
}

fn is_procedure_definition_line(line: &str) -> bool {
    let code = strip_fortran_comment(line).trim().to_string();
    let code = strip_procedure_prefixes_ws(&code);
    after_keyword_ws(code, "subroutine").is_some()
        || after_keyword_ws(code, "function").is_some()
        || after_keyword_ws(code, "module subroutine").is_some()
        || after_keyword_ws(code, "module function").is_some()
}

fn strip_procedure_prefixes_ws(mut code: &str) -> &str {
    loop {
        let trimmed = code.trim_start();
        let Some(prefix) = first_ident_local(trimmed) else {
            return trimmed;
        };
        if !matches!(
            prefix.to_ascii_lowercase().as_str(),
            "pure" | "impure" | "elemental" | "recursive"
        ) {
            return trimmed;
        }
        code = &trimmed[prefix.len()..];
    }
}

fn after_keyword_ws<'a>(code: &'a str, keyword: &str) -> Option<&'a str> {
    let trimmed = code.trim_start();
    let keyword_len = keyword.len();
    let prefix = trimmed.get(..keyword_len)?;
    if !prefix.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = trimmed.get(keyword_len..)?;
    if rest
        .chars()
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(rest)
}

fn is_ident_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn literal_hover_at_source(source: &str, pos: Position) -> Option<String> {
    let line = source.lines().nth(pos.line)?;
    let byte = byte_idx_for_utf16_col(line, pos.character);
    if let Some(len) = character_literal_len_at(line, byte) {
        return Some(fortran_hover_type(&format!("CHARACTER(LEN={len})")));
    }
    if logical_literal_at(line, byte).is_some() {
        return Some(fortran_hover_type("LOGICAL"));
    }
    numeric_literal_kind_at(line, byte).map(fortran_hover_type)
}

fn fortran_hover_type(kind: &str) -> String {
    format!("```fortran\n{kind}\n```")
}

fn character_literal_len_at(line: &str, byte: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        let quote = bytes[idx];
        if quote != b'\'' && quote != b'"' {
            idx += 1;
            continue;
        }
        let start = idx;
        idx += 1;
        let mut len = 0usize;
        while idx < bytes.len() {
            if bytes[idx] == quote {
                if idx + 1 < bytes.len() && bytes[idx + 1] == quote {
                    len += 1;
                    idx += 2;
                    continue;
                }
                let end = idx + 1;
                if start <= byte && byte <= end {
                    return Some(len);
                }
                idx = end;
                break;
            }
            let ch = line[idx..].chars().next()?;
            len += 1;
            idx += ch.len_utf8();
        }
    }
    None
}

fn logical_literal_at(line: &str, byte: usize) -> Option<&str> {
    let lower = line.to_ascii_lowercase();
    [".true.", ".false."].into_iter().find(|literal| {
        lower.match_indices(literal).any(|(start, _)| {
            let end = start + literal.len();
            start <= byte
                && byte <= end
                && literal_start_boundary(line, start)
                && literal_end_boundary(line, end)
        })
    })
}

fn numeric_literal_kind_at(line: &str, byte: usize) -> Option<&'static str> {
    let bytes = line.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut idx = byte.min(bytes.len().saturating_sub(1));
    if !is_number_token_byte(bytes[idx]) && idx > 0 {
        idx -= 1;
    }
    if !is_number_token_byte(bytes[idx]) {
        return None;
    }
    let mut start = idx;
    while start > 0 && is_number_token_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = idx + 1;
    while end < bytes.len() && is_number_token_byte(bytes[end]) {
        end += 1;
    }
    if !literal_start_boundary(line, start) || !literal_end_boundary(line, end) {
        return None;
    }
    let token = line[start..end].trim_matches(['+', '-']);
    let value = token
        .split_once('_')
        .map(|(value, _)| value)
        .unwrap_or(token);
    if value.is_empty() || value == "." {
        return None;
    }
    let lower = value.to_ascii_lowercase();
    let starts_numeric = lower.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        || lower
            .strip_prefix('.')
            .and_then(|rest| rest.chars().next())
            .is_some_and(|ch| ch.is_ascii_digit());
    if !starts_numeric {
        return None;
    }
    if lower.contains('.') || lower.contains('e') || lower.contains('d') {
        Some("REAL")
    } else if lower.chars().all(|ch| ch.is_ascii_digit()) {
        Some("INTEGER")
    } else {
        None
    }
}

fn is_number_token_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'0'..=b'9' | b'.' | b'+' | b'-' | b'e' | b'E' | b'd' | b'D' | b'_'
    ) || byte.is_ascii_alphabetic()
}

fn literal_start_boundary(line: &str, start: usize) -> bool {
    if start == 0 {
        return true;
    }
    !line[..start]
        .chars()
        .next_back()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn literal_end_boundary(line: &str, end: usize) -> bool {
    if end >= line.len() {
        return true;
    }
    !line[end..]
        .chars()
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn member_completion_receiver(source: &str, pos: Position, prefix: &str) -> Option<String> {
    let line = source.lines().nth(pos.line)?;
    let cursor = byte_idx_for_utf16_col(line, pos.character);
    let mut before = line.get(..cursor)?.trim_end();
    if !prefix.is_empty() && before.ends_with(prefix) {
        before = before.get(..before.len().saturating_sub(prefix.len()))?;
    }
    before = before.trim_end();
    let sep = before.chars().last()?;
    if sep != '%' && sep != '.' {
        return None;
    }
    let receiver_end = before.len().saturating_sub(sep.len_utf8());
    let receiver_prefix = before.get(..receiver_end)?.trim_end();
    let receiver_start = receiver_prefix
        .char_indices()
        .rev()
        .find(|(_, ch)| !is_ident_char(*ch))
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    let receiver = receiver_prefix.get(receiver_start..)?.trim();
    (!receiver.is_empty()).then(|| receiver.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UseCompletionContext {
    Module,
    Only { module: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclarationKeywordScope {
    Module,
    Procedure,
    Type,
    Other,
}

const DECLARATION_VAR_KEYWORDS: &[(&str, &str)] = &[
    ("allocatable", "Declaration attribute"),
    ("asynchronous", "Declaration attribute"),
    ("bind", "Declaration attribute"),
    ("codimension", "Declaration attribute"),
    ("contiguous", "Declaration attribute"),
    ("dimension", "Declaration attribute"),
    ("external", "Declaration attribute"),
    ("intrinsic", "Declaration attribute"),
    ("pointer", "Declaration attribute"),
    ("protected", "Declaration attribute"),
    ("target", "Declaration attribute"),
    ("volatile", "Declaration attribute"),
];

const DECLARATION_ARG_KEYWORDS: &[(&str, &str)] = &[
    ("intent(in)", "Dummy argument intent attribute"),
    ("intent(inout)", "Dummy argument intent attribute"),
    ("intent(out)", "Dummy argument intent attribute"),
    ("optional", "Dummy argument attribute"),
    ("save", "Declaration attribute"),
    ("value", "Dummy argument attribute"),
];

const DECLARATION_TYPE_MEMBER_KEYWORDS: &[(&str, &str)] = &[
    ("deferred", "Type-bound procedure attribute"),
    ("non_overridable", "Type-bound procedure attribute"),
    ("nopass", "Type-bound procedure attribute"),
    ("pass", "Type-bound procedure attribute"),
];

const DECLARATION_VISIBILITY_KEYWORDS: &[(&str, &str)] = &[
    ("private", "Visibility attribute"),
    ("public", "Visibility attribute"),
];

const DECLARATION_PARAMETER_KEYWORDS: &[(&str, &str)] =
    &[("parameter", "Named constant attribute")];

const FORTRAN_STATEMENT_KEYWORDS: &[(&str, &str)] = &[
    ("allocate", "Fortran statement"),
    ("backspace", "Fortran statement"),
    ("call", "Fortran statement"),
    ("character", "Declaration statement"),
    ("class", "Declaration statement"),
    ("close", "Fortran statement"),
    ("complex", "Declaration statement"),
    ("continue", "Fortran statement"),
    ("cycle", "Fortran statement"),
    ("deallocate", "Fortran statement"),
    ("double complex", "Declaration statement"),
    ("double precision", "Declaration statement"),
    ("endfile", "Fortran statement"),
    ("error stop", "Fortran statement"),
    ("event post", "Fortran statement"),
    ("event wait", "Fortran statement"),
    ("fail image", "Fortran statement"),
    ("flush", "Fortran statement"),
    ("form team", "Fortran statement"),
    ("format", "Fortran statement"),
    ("inquire", "Fortran statement"),
    ("integer", "Declaration statement"),
    ("lock", "Fortran statement"),
    ("logical", "Declaration statement"),
    ("namelist", "Fortran statement"),
    ("open", "Fortran statement"),
    ("print", "Fortran statement"),
    ("private", "Visibility statement"),
    ("public", "Visibility statement"),
    ("read", "Fortran statement"),
    ("real", "Declaration statement"),
    ("return", "Fortran statement"),
    ("rewind", "Fortran statement"),
    ("stop", "Fortran statement"),
    ("sync all", "Fortran statement"),
    ("sync images", "Fortran statement"),
    ("sync memory", "Fortran statement"),
    ("sync team", "Fortran statement"),
    ("type", "Declaration statement"),
    ("unlock", "Fortran statement"),
    ("wait", "Fortran statement"),
    ("write", "Fortran statement"),
];

fn first_word_statement_completion_context(line: &str, character: usize) -> Option<()> {
    let code = strip_fortran_comment(line);
    let cursor = byte_idx_for_utf16_col(&code, character);
    let before = code.get(..cursor.min(code.len()))?.trim_start();
    before
        .chars()
        .all(|ch| ch.is_ascii_alphabetic())
        .then_some(())
}

fn skip_completion_context(line: &str, character: usize) -> bool {
    let code = strip_fortran_comment(line);
    let cursor = byte_idx_for_utf16_col(&code, character);
    let before = code
        .get(..cursor.min(code.len()))
        .unwrap_or("")
        .trim_start();
    end_statement_context(before) || scope_declaration_context(before)
}

fn end_statement_context(before: &str) -> bool {
    let Some(rest) = keyword_rest_ci(before, "end") else {
        return false;
    };
    let rest = rest.trim_start();
    rest.is_empty()
        || matches!(
            first_ident_local(rest)
                .map(|ident| ident.to_ascii_lowercase())
                .as_deref(),
            Some(
                "associate"
                    | "block"
                    | "critical"
                    | "do"
                    | "function"
                    | "if"
                    | "interface"
                    | "module"
                    | "procedure"
                    | "program"
                    | "select"
                    | "submodule"
                    | "subroutine"
                    | "type"
                    | "where"
            )
        )
}

fn scope_declaration_context(before: &str) -> bool {
    if keyword_rest_ci(before, "module procedure").is_some() {
        return false;
    }
    for keyword in [
        "module",
        "program",
        "submodule",
        "subroutine",
        "function",
        "abstract interface",
        "interface",
        "block",
        "associate",
        "select type",
    ] {
        if keyword_rest_ci(before, keyword).is_some() {
            return true;
        }
    }
    derived_type_declaration_context(before)
}

fn derived_type_declaration_context(before: &str) -> bool {
    let Some(rest) = keyword_rest_ci(before, "type") else {
        return false;
    };
    let trimmed = rest.trim_start();
    trimmed.starts_with("::") || trimmed.starts_with(',') || first_ident_local(trimmed).is_some()
}

fn declaration_keyword_context(line: &str, character: usize) -> Option<()> {
    let code = strip_fortran_comment(line);
    let cursor = byte_idx_for_utf16_col(&code, character);
    let before = code.get(..cursor.min(code.len()))?.trim_start();
    if before.contains("::") {
        return None;
    }
    let first = first_ident_local(before)?;
    let lower = first.to_ascii_lowercase();
    if !matches!(
        lower.as_str(),
        "integer"
            | "real"
            | "double"
            | "doubleprecision"
            | "complex"
            | "character"
            | "logical"
            | "type"
            | "class"
            | "procedure"
    ) {
        return None;
    }
    let rest = before.get(first.len()..)?.trim_start();
    if rest.is_empty() || rest.starts_with('(') || rest.starts_with(',') {
        Some(())
    } else {
        None
    }
}

fn declaration_variable_completion_context(line: &str, character: usize) -> Option<()> {
    let code = strip_fortran_comment(line);
    let cursor = byte_idx_for_utf16_col(&code, character);
    let before = code.get(..cursor.min(code.len()))?.trim_start();
    let (lhs, rhs) = before.split_once("::")?;
    if !declaration_type_keyword(lhs)? {
        return None;
    }
    if rhs.contains("=>") {
        return None;
    }
    Some(())
}

fn declaration_type_keyword(lhs: &str) -> Option<bool> {
    let first = first_ident_local(lhs)?;
    Some(matches!(
        first.to_ascii_lowercase().as_str(),
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
    ))
}

fn type_name_completion_context(line: &str, character: usize) -> Option<()> {
    let code = strip_fortran_comment(line);
    let cursor = byte_idx_for_utf16_col(&code, character);
    let before = code.get(..cursor.min(code.len()))?;
    let open = before.rfind('(')?;
    if before[open + 1..].contains(')') {
        return None;
    }
    let keyword_end = before[..open].trim_end().len();
    let keyword_start = before[..keyword_end]
        .char_indices()
        .rev()
        .find(|(_, ch)| !is_ident_char(*ch))
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    let keyword = before[keyword_start..keyword_end].trim();
    matches_ignore_ascii_case(keyword, &["type", "class", "extends"]).then_some(())
}

fn call_statement_completion_context(line: &str, character: usize) -> Option<()> {
    let code = strip_fortran_comment(line);
    let cursor = byte_idx_for_utf16_col(&code, character);
    let before = code.get(..cursor.min(code.len()))?.trim_start();
    let rest = keyword_rest_ci(before, "call")?.trim_start();
    if rest.contains('(') || rest.contains(['%', '.']) {
        return None;
    }
    Some(())
}

fn module_procedure_link_completion_context(line: &str, character: usize) -> Option<()> {
    let code = strip_fortran_comment(line);
    let cursor = byte_idx_for_utf16_col(&code, character);
    let before = code.get(..cursor.min(code.len()))?.trim_start();
    let rest = keyword_rest_ci(before, "module")?.trim_start();
    keyword_rest_ci(rest, "procedure").map(|_| ())
}

fn procedure_interface_completion_context(line: &str, character: usize) -> Option<()> {
    let code = strip_fortran_comment(line);
    let cursor = byte_idx_for_utf16_col(&code, character);
    let before = code.get(..cursor.min(code.len()))?;
    let open = before.rfind('(')?;
    if before[open + 1..].contains(')') {
        return None;
    }
    let keyword_end = before[..open].trim_end().len();
    let keyword_start = before[..keyword_end]
        .char_indices()
        .rev()
        .find(|(_, ch)| !is_ident_char(*ch))
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    let keyword = before[keyword_start..keyword_end].trim();
    keyword.eq_ignore_ascii_case("procedure").then_some(())
}

fn visibility_statement_completion_context(line: &str, character: usize) -> Option<()> {
    let code = strip_fortran_comment(line);
    let cursor = byte_idx_for_utf16_col(&code, character);
    let before = code.get(..cursor.min(code.len()))?.trim_start();
    keyword_rest_ci(before, "public")
        .or_else(|| keyword_rest_ci(before, "private"))
        .map(|_| ())
}

fn import_statement_completion_context(line: &str, character: usize) -> Option<()> {
    let code = strip_fortran_comment(line);
    let cursor = byte_idx_for_utf16_col(&code, character);
    let before = code.get(..cursor.min(code.len()))?.trim_start();
    keyword_rest_ci(before, "import").map(|_| ())
}

fn import_completion_host_scopes(current_scope: &[String]) -> Vec<Vec<String>> {
    if current_scope.is_empty() {
        return vec![Vec::new()];
    }
    let mut scope = current_scope[..current_scope.len() - 1].to_vec();
    let mut scopes = vec![scope.clone()];
    while !scope.is_empty() {
        scope.pop();
        scopes.push(scope.clone());
    }
    scopes
}

fn import_completion_symbol(sym: &Symbol) -> bool {
    matches!(sym.kind, SymbolKind::Type | SymbolKind::Variable)
}

fn visibility_completion_symbol(sym: &Symbol) -> bool {
    matches!(
        sym.kind,
        SymbolKind::Type | SymbolKind::Variable | SymbolKind::Subroutine | SymbolKind::Function
    )
}

fn variable_completion_symbol(sym: &Symbol) -> bool {
    sym.kind == SymbolKind::Variable
}

fn same_module_callable_named(file: &ParsedFile, sym: &Symbol) -> bool {
    let Some(module) = sym.scope.first() else {
        return false;
    };
    file.symbols.iter().any(|candidate| {
        matches!(
            candidate.kind,
            SymbolKind::Function | SymbolKind::Subroutine | SymbolKind::Interface
        ) && candidate.name.eq_ignore_ascii_case(&sym.name)
            && candidate.scope.len() == 1
            && candidate.scope[0].eq_ignore_ascii_case(module)
    })
}

fn abstract_interface_prototype_host_scope<'a>(
    file: &'a ParsedFile,
    sym: &'a Symbol,
) -> Option<&'a [String]> {
    if !matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Function) {
        return None;
    }
    let (interface_name, host_scope) = sym.scope.split_last()?;
    file.symbols
        .iter()
        .any(|candidate| {
            candidate.kind == SymbolKind::Interface
                && candidate.is_abstract
                && candidate.name.eq_ignore_ascii_case(interface_name)
                && scopes_equal(&candidate.scope, host_scope)
        })
        .then_some(host_scope)
}

fn callable_completion_symbol(sym: &Symbol) -> bool {
    matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Interface)
}

fn is_module_procedure_link(sym: &Symbol) -> bool {
    matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Function)
        && sym
            .signature
            .get(..sym.signature.len().min("module procedure".len()))
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("module procedure"))
}

fn is_interface_module_procedure_prototype(sym: &Symbol) -> bool {
    matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Function)
        && sym
            .scope
            .iter()
            .any(|part| part.eq_ignore_ascii_case("interface"))
        && signature_has_module_procedure_prefix(&sym.signature)
}

fn is_interface_module_procedure_prototype_in_file(file: &ParsedFile, sym: &Symbol) -> bool {
    matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Function)
        && signature_has_module_procedure_prefix(&sym.signature)
        && symbol_has_interface_parent(file, sym)
}

fn symbol_has_interface_parent(file: &ParsedFile, sym: &Symbol) -> bool {
    if sym
        .scope
        .iter()
        .any(|part| part.eq_ignore_ascii_case("interface"))
    {
        return true;
    }
    let Some((interface_name, parent_scope)) = sym.scope.split_last() else {
        return false;
    };
    file.symbols.iter().any(|candidate| {
        candidate.kind == SymbolKind::Interface
            && candidate.name.eq_ignore_ascii_case(interface_name)
            && scopes_equal(&candidate.scope, parent_scope)
    })
}

fn module_export_scope_matches(scope: &[String], module: &str) -> bool {
    scope
        .first()
        .is_some_and(|part| part.eq_ignore_ascii_case(module))
        && (scope.len() == 1
            || scope
                .iter()
                .any(|part| part.eq_ignore_ascii_case("interface")))
}

fn method_signature_matches_keyword(
    method: &Symbol,
    target: &Symbol,
    active_keyword: Option<&str>,
) -> bool {
    active_keyword.is_none_or(|keyword| {
        method_call_args(method, target)
            .iter()
            .any(|arg| parameter_label_name(arg).eq_ignore_ascii_case(keyword))
    })
}

fn procedure_signature_matches_keyword(procedure: &Symbol, active_keyword: Option<&str>) -> bool {
    active_keyword.is_none_or(|keyword| {
        procedure
            .args
            .iter()
            .any(|arg| parameter_label_name(arg).eq_ignore_ascii_case(keyword))
    })
}

fn procedure_signature_label(sym: &Symbol) -> String {
    if is_interface_module_procedure_prototype(sym) {
        return format!("{}({})", sym.name, sym.args.join(", "));
    }
    sym.signature.clone()
}

fn signature_has_module_procedure_prefix(signature: &str) -> bool {
    let lower = signature.to_ascii_lowercase();
    let tokens: Vec<_> = lower
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .filter(|token| !token.is_empty())
        .collect();
    tokens
        .iter()
        .position(|token| *token == "module")
        .is_some_and(|idx| {
            tokens[idx + 1..]
                .iter()
                .any(|token| *token == "subroutine" || *token == "function")
        })
}

fn source_line_is_comment(path: &Path, line: &str) -> bool {
    if is_fixed_form_source(path) {
        return line
            .as_bytes()
            .first()
            .is_some_and(|ch| matches!(*ch as char, 'c' | 'C' | '*' | '!'));
    }
    line.trim_start().starts_with('!')
}

fn is_fixed_form_source(path: &Path) -> bool {
    if path_has_free_form_hint(path) {
        return false;
    }
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| matches!(ext.as_str(), "f" | "for" | "ftn" | "f77"))
}

fn path_has_free_form_hint(path: &Path) -> bool {
    path.components().any(|component| {
        component.as_os_str().to_str().is_some_and(|part| {
            let part = part.to_ascii_lowercase();
            part == "free-form" || part == "free_form" || part == "freeform"
        })
    })
}

fn intrinsic_subroutine_completion_symbol(sym: &intrinsics::IntrinsicSymbol) -> bool {
    sym.kind == intrinsics::IntrinsicKind::Subroutine
}

fn declaration_keyword_scope(file: &ParsedFile, pos: Position) -> DeclarationKeywordScope {
    let scope = file.scope_at(pos);
    let Some((name, parent_scope)) = scope.split_last() else {
        return DeclarationKeywordScope::Other;
    };
    file.symbols
        .iter()
        .find(|sym| sym.name.eq_ignore_ascii_case(name) && scopes_equal(&sym.scope, parent_scope))
        .map(|sym| match sym.kind {
            SymbolKind::Module => DeclarationKeywordScope::Module,
            SymbolKind::Subroutine | SymbolKind::Function => DeclarationKeywordScope::Procedure,
            SymbolKind::Type => DeclarationKeywordScope::Type,
            _ => DeclarationKeywordScope::Other,
        })
        .unwrap_or(DeclarationKeywordScope::Other)
}

fn declaration_keyword_completions(
    prefix: &str,
    scope: DeclarationKeywordScope,
) -> Vec<CompletionItem> {
    let mut candidates = Vec::new();
    candidates.extend_from_slice(DECLARATION_VAR_KEYWORDS);
    match scope {
        DeclarationKeywordScope::Module => {
            candidates.extend_from_slice(DECLARATION_VISIBILITY_KEYWORDS);
            candidates.extend_from_slice(DECLARATION_PARAMETER_KEYWORDS);
        }
        DeclarationKeywordScope::Procedure => {
            candidates.extend_from_slice(DECLARATION_ARG_KEYWORDS);
            candidates.extend_from_slice(DECLARATION_PARAMETER_KEYWORDS);
        }
        DeclarationKeywordScope::Type => {
            candidates.extend_from_slice(DECLARATION_TYPE_MEMBER_KEYWORDS);
            candidates.extend_from_slice(DECLARATION_VISIBILITY_KEYWORDS);
        }
        DeclarationKeywordScope::Other => {
            candidates.extend_from_slice(DECLARATION_PARAMETER_KEYWORDS);
        }
    }
    candidates
        .into_iter()
        .filter(|(label, _)| label.starts_with(prefix))
        .map(|(label, detail)| CompletionItem {
            label: (*label).to_string(),
            detail: (*detail).to_string(),
            kind: SymbolKind::Variable,
            documentation: None,
            visibility: Visibility::Public,
        })
        .collect()
}

fn add_preprocessor_completions(
    file: &ParsedFile,
    prefix: &str,
    items: &mut BTreeMap<String, CompletionItem>,
) {
    for (name, definition) in &file.preprocessor_definitions {
        if !name.to_ascii_lowercase().starts_with(prefix) {
            continue;
        }
        let detail = if definition.is_empty() {
            format!("#define {name}")
        } else {
            format!("#define {name} {definition}")
        };
        items.entry(name.clone()).or_insert_with(|| CompletionItem {
            label: name.clone(),
            detail: detail.clone(),
            kind: SymbolKind::Variable,
            documentation: Some(format!("```fortran\n{detail}\n```")),
            visibility: Visibility::Default,
        });
    }
}

fn fortran_statement_completions(prefix: &str) -> Vec<CompletionItem> {
    FORTRAN_STATEMENT_KEYWORDS
        .iter()
        .filter(|(label, _)| label.starts_with(prefix))
        .map(|(label, detail)| CompletionItem {
            label: (*label).to_string(),
            detail: (*detail).to_string(),
            kind: SymbolKind::Variable,
            documentation: None,
            visibility: Visibility::Public,
        })
        .collect()
}

fn use_completion_context(line: &str, character: usize) -> Option<UseCompletionContext> {
    let cursor = byte_idx_for_utf16_col(line, character);
    let code = strip_fortran_comment(line);
    let before = code.get(..cursor.min(code.len()))?.trim_start();
    let rest = keyword_rest_ci(before, "use")?.trim_start();
    if rest.is_empty() {
        return Some(UseCompletionContext::Module);
    }
    let rest = if rest.starts_with(',') {
        let (_, rhs) = rest.split_once("::")?;
        rhs.trim_start()
    } else {
        rest
    };
    let only_idx = find_ci(rest, "only");
    if let Some(idx) = only_idx {
        let before_only = rest[..idx].trim_end();
        let module = first_ident_local(before_only)?;
        let after_only = rest[idx + "only".len()..].trim_start();
        if after_only.starts_with(':') || after_only.starts_with("::") {
            return Some(UseCompletionContext::Only {
                module: module.to_string(),
            });
        }
    }
    if rest.contains(',') || rest.contains(':') {
        return None;
    }
    Some(UseCompletionContext::Module)
}

fn keyword_rest_ci<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let prefix = line.get(..keyword.len())?;
    if !prefix.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = line.get(keyword.len()..)?;
    if rest.chars().next().is_some_and(is_ident_char) {
        return None;
    }
    Some(rest)
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .to_ascii_lowercase()
        .find(&needle.to_ascii_lowercase())
}

fn first_ident_local(s: &str) -> Option<&str> {
    let start = s.find(|ch: char| ch == '_' || ch.is_ascii_alphabetic())?;
    let tail = &s[start..];
    let end = tail
        .find(|ch: char| !is_ident_char(ch))
        .unwrap_or(tail.len());
    Some(&tail[..end])
}

fn intrinsic_type_spec(spec: &str) -> bool {
    first_ident_local(spec).is_some_and(|name| {
        matches_ignore_ascii_case(
            name,
            &[
                "integer",
                "real",
                "double",
                "doubleprecision",
                "complex",
                "character",
                "logical",
            ],
        )
    })
}

struct KindSelectorName<'a> {
    name: &'a str,
    explicit_kind_keyword: bool,
}

fn declaration_kind_selector_names(type_spec: &str) -> Vec<KindSelectorName<'_>> {
    let Some(open) = type_spec.find('(') else {
        return Vec::new();
    };
    let Some(close) = matching_paren_close(type_spec, open) else {
        return Vec::new();
    };
    let keyword = type_spec[..open].trim();
    if !matches_ignore_ascii_case(
        keyword,
        &["integer", "real", "complex", "logical", "character"],
    ) {
        return Vec::new();
    }
    let content = &type_spec[open + 1..close];
    let lower = content.to_ascii_lowercase();
    if let Some(kind_idx) = lower.find("kind") {
        let after_kind = &content[kind_idx + "kind".len()..];
        if let Some(eq_idx) = after_kind.find('=') {
            return first_ident_local(&after_kind[eq_idx + 1..])
                .into_iter()
                .filter(|name| !kind_selector_keyword(name))
                .map(|name| KindSelectorName {
                    name,
                    explicit_kind_keyword: true,
                })
                .collect();
        }
    }
    let trimmed = content.trim();
    if trimmed.contains('=') || trimmed.contains(':') || trimmed.contains('*') {
        return Vec::new();
    }
    first_ident_local(trimmed)
        .into_iter()
        .filter(|name| trimmed.eq_ignore_ascii_case(name) && !kind_selector_keyword(name))
        .map(|name| KindSelectorName {
            name,
            explicit_kind_keyword: false,
        })
        .collect()
}

fn matching_paren_close(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, ch) in text[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn kind_selector_keyword(name: &str) -> bool {
    matches_ignore_ascii_case(name, &["kind", "len"])
}

fn kind_selector_builtin_name(name: &str) -> bool {
    matches_ignore_ascii_case(
        name,
        &[
            "int8",
            "int16",
            "int32",
            "int64",
            "real32",
            "real64",
            "real128",
            "character_storage_size",
            "error_unit",
            "file_storage_size",
            "input_unit",
            "iostat_end",
            "iostat_eor",
            "numeric_storage_size",
            "output_unit",
        ],
    )
}

fn external_include_is_allowed(include: &IncludeStmt) -> bool {
    include.path.eq_ignore_ascii_case("mpif.h")
}

fn matches_ignore_ascii_case(value: &str, choices: &[&str]) -> bool {
    choices
        .iter()
        .any(|choice| value.eq_ignore_ascii_case(choice))
}

fn is_scope_selection_candidate(kind: SymbolKind) -> bool {
    is_scope_kind(kind)
}

fn push_unique_range(ranges: &mut Vec<Range>, range: Range) {
    if ranges.last() != Some(&range) {
        ranges.push(range);
    }
}

fn range_size(range: &Range) -> usize {
    (range.end.line.saturating_sub(range.start.line) * 1_000_000)
        + range.end.character.saturating_sub(range.start.character)
}

fn selection_range_chain(ranges: Vec<Range>) -> Option<SelectionRange> {
    let mut parent = None;
    for range in ranges.into_iter().rev() {
        parent = Some(SelectionRange {
            range,
            parent: parent.map(Box::new),
        });
    }
    parent
}

fn byte_idx_for_utf16_col(line: &str, character: usize) -> usize {
    let mut utf16 = 0usize;
    for (idx, ch) in line.char_indices() {
        if utf16 >= character {
            return idx;
        }
        utf16 += ch.len_utf16();
    }
    line.len()
}

fn utf16_col(line: &str, byte_idx: usize) -> usize {
    let mut idx = byte_idx.min(line.len());
    while idx > 0 && !line.is_char_boundary(idx) {
        idx -= 1;
    }
    line[..idx].encode_utf16().count()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub detail: String,
    pub kind: SymbolKind,
    pub documentation: Option<String>,
    pub visibility: Visibility,
}

impl CompletionItem {
    fn from_symbol(sym: &Symbol) -> Self {
        Self {
            label: sym.name.clone(),
            detail: sym.signature.clone(),
            kind: sym.kind,
            documentation: sym.documentation.clone(),
            visibility: sym.visibility,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureHelp {
    pub label: String,
    pub parameters: Vec<String>,
    pub active_parameter: usize,
    pub documentation: Option<String>,
}
