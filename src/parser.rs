use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::model::{
    Diagnostic, DiagnosticSeverity, GenericBinding, GenericBindingKind, ImportKind, ImportStmt,
    IncludeStmt, ParsedFile, Position, PreprocessorDirective, PreprocessorKind, PreprocessorRegion,
    Range, Symbol, SymbolKind, UseRename, UseStmt, Visibility, VisibilityStmt,
};

pub(crate) struct Parser<'a> {
    path: PathBuf,
    source: &'a str,
    parsed: ParsedFile,
    scopes: Vec<ScopeFrame>,
    pending_doc: Vec<String>,
    default_visibility: HashMap<Vec<String>, Visibility>,
    explicit_visibility: HashMap<(Vec<String>, String), Visibility>,
    preprocessor_stack: Vec<PreprocessorFrame>,
    preprocessor_macros: HashMap<String, MacroDefinition>,
    /// Symbols from legacy storage statements (`common`, `namelist`) that are
    /// only added after the whole file is parsed, and only for names with no
    /// other symbol in the same scope: legacy code redeclares COMMON members
    /// with explicit types, and repeated NAMELIST statements extend one group.
    pending_legacy_symbols: Vec<Symbol>,
    /// Lazily-built per-line interface nesting states (see
    /// [`Parser::line_interface_state`]).
    interface_line_states: std::cell::OnceCell<Vec<Option<bool>>>,
}

#[derive(Debug, Clone)]
struct ScopeFrame {
    name: String,
    kind: SymbolKind,
    symbol_idx: usize,
    default_visibility: Visibility,
    implicit_line: Option<usize>,
    implicit_none: bool,
    use_after_implicit: bool,
    contains_line: Option<usize>,
    declared_variables: HashSet<String>,
    seen_executable: bool,
}

#[derive(Debug, Clone)]
struct PreprocessorFrame {
    start: Position,
    range: Range,
    saw_else: bool,
    parent_active: bool,
    branch_active: bool,
    any_taken: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MacroDefinition {
    Object(String),
    Function { params: Vec<String>, body: String },
}

impl<'a> Parser<'a> {
    /// A parser whose preprocessor starts with externally predefined object
    /// macros (a build system's `-D` set). An empty value means the C
    /// convention `1`.
    pub(crate) fn with_defines(
        path: PathBuf,
        source: &'a str,
        defines: &[(String, String)],
    ) -> Self {
        let mut preprocessor_macros = HashMap::new();
        let mut preprocessor_definitions = HashMap::new();
        for (name, value) in defines {
            let value = if value.is_empty() { "1" } else { value };
            preprocessor_macros.insert(name.clone(), MacroDefinition::Object(value.to_string()));
            preprocessor_definitions.insert(name.clone(), value.to_string());
        }
        Self {
            parsed: ParsedFile {
                path: path.clone(),
                source: source.to_string(),
                preprocessor_definitions,
                ..ParsedFile::default()
            },
            path,
            source,
            scopes: Vec::new(),
            pending_doc: Vec::new(),
            default_visibility: HashMap::new(),
            explicit_visibility: HashMap::new(),
            preprocessor_stack: Vec::new(),
            preprocessor_macros,
            pending_legacy_symbols: Vec::new(),
            interface_line_states: std::cell::OnceCell::new(),
        }
    }

    pub(crate) fn parse(mut self) -> ParsedFile {
        // The fold-stage preprocessor filter starts from the same predefined
        // macro set the parser was seeded with.
        let predefined = self.parsed.preprocessor_definitions.clone();
        for (line_no, raw_line) in logical_lines(&self.path, self.source, &predefined) {
            let raw_code = raw_line.trim();
            if raw_code.is_empty() {
                continue;
            }
            if let Some(directive) =
                parse_preprocessor(raw_code, line_no, &self.path, &self.current_scope())
            {
                let was_active = self.preprocessor_active();
                self.apply_preprocessor(&directive);
                if was_active && directive.kind == PreprocessorKind::Include {
                    if let Some(path) = directive.argument.clone() {
                        self.parsed.includes.push(IncludeStmt {
                            path,
                            file: self.path.clone(),
                            range: directive.range.clone(),
                            scope: directive.scope.clone(),
                        });
                    }
                    self.apply_preprocessor_include_macros(&directive);
                }
                self.parsed.preprocessor.push(directive);
                continue;
            }
            if !self.preprocessor_active() {
                continue;
            }
            if let Some(doc) = parse_doc_comment(&raw_line) {
                self.pending_doc.push(doc);
                continue;
            }
            let code = strip_inline_comment(&raw_line);
            let expanded = self.expand_macros(&code);
            for statement in split_statement_line(&expanded) {
                self.parse_statement(line_no, &statement);
            }
        }
        while let Some(scope) = self.scopes.pop() {
            if let Some(diagnostic_line) = use_after_implicit_line(&scope) {
                self.use_after_implicit_diagnostic(diagnostic_line);
            }
            self.add_argument_diagnostics(&scope);
            let range = self
                .parsed
                .symbols
                .get(scope.symbol_idx)
                .map(|sym| sym.selection_range.clone())
                .unwrap_or_else(|| Range {
                    start: source_end(self.source),
                    end: source_end(self.source),
                });
            self.parsed.diagnostics.push(Diagnostic {
                range,
                severity: DiagnosticSeverity::Warning,
                message: format!("unterminated {} scope `{}`", scope.kind.label(), scope.name),
            });
            if let Some(sym) = self.parsed.symbols.get_mut(scope.symbol_idx) {
                sym.range.end = source_end(self.source);
            }
        }
        while let Some(frame) = self.preprocessor_stack.pop() {
            self.parsed.diagnostics.push(Diagnostic {
                range: frame.range,
                severity: DiagnosticSeverity::Error,
                message: "unterminated preprocessor conditional".to_string(),
            });
            self.parsed.preprocessor_regions.push(PreprocessorRegion {
                start: frame.start,
                end: source_end(self.source),
            });
        }
        self.apply_symbol_visibility();
        self.flush_pending_legacy_symbols();
        self.add_duplicate_diagnostics();
        self.add_parent_masking_diagnostics();
        self.add_use_after_implicit_diagnostics();
        self.parsed
    }

    fn parse_statement(&mut self, line_no: usize, code: &str) {
        let code = code.trim_end();
        if code.trim().is_empty() {
            return;
        }
        let lower = code.trim_start().to_ascii_lowercase();
        if lower.starts_with("end ") || lower == "end" {
            self.close_scope(line_no, &lower);
            return;
        }
        if lower == "contains" {
            self.mark_contains_statement(line_no, code);
            return;
        }
        if after_keyword(code, "implicit").is_some() {
            self.mark_implicit_statement(line_no, code);
            return;
        }
        let scope = self.current_scope();
        if let Some(stmt) = parse_use(code, line_no, &self.path, &scope) {
            self.mark_use_statement(line_no);
            self.parsed.uses.push(stmt);
            return;
        }
        if after_keyword(code, "use").is_some() {
            self.statement_diagnostic(line_no, code, "incomplete or invalid use statement");
            return;
        }
        if let Some(stmt) = parse_import(code, line_no, &self.path, &scope) {
            if !self.in_interface_scope() {
                self.statement_diagnostic(line_no, code, "IMPORT statement outside of interface");
            }
            self.parsed.imports.push(stmt);
            return;
        }
        if let Some(stmt) = parse_include(code, line_no, &self.path, &scope) {
            self.parsed.includes.push(stmt);
            return;
        }
        if let Some(stmt) = parse_generic_binding(code, line_no, &self.path, &scope) {
            self.parsed.generic_bindings.push(stmt);
            return;
        }
        if let Some(stmt) = parse_visibility(code, line_no, &self.path, &scope) {
            self.apply_visibility_stmt(&stmt);
            self.parsed.visibility.push(stmt);
            return;
        }
        let enumerators = parse_enumerators(code, line_no, &self.path, &scope);
        if !enumerators.is_empty() {
            self.record_variable_declarations(&enumerators);
            self.parsed.symbols.extend(enumerators);
            return;
        }
        if let Some(entries) = self.parse_entry_statement(line_no, code) {
            self.parsed.symbols.extend(entries);
            return;
        }
        if let Some(pending) = parse_common(code, line_no, &self.path, &scope) {
            self.pending_legacy_symbols.extend(pending);
            return;
        }
        if let Some(pending) = parse_equivalence(code, line_no, &self.path, &scope) {
            self.pending_legacy_symbols.extend(pending);
            return;
        }
        if let Some(pending) = parse_namelist(code, line_no, &self.path, &scope) {
            self.pending_legacy_symbols.extend(pending);
            return;
        }
        if self.in_interface_scope() {
            let module_procedures =
                parse_module_procedure_prototypes(code, line_no, &self.path, &scope);
            if !module_procedures.is_empty() {
                self.parsed.symbols.extend(module_procedures);
                return;
            }
        }
        if let Some(start) = parse_scope_start(code) {
            self.open_scope(line_no, start);
            return;
        }
        if self.parse_statement_function(line_no, code) {
            return;
        }
        let in_type_binding_part = self.in_type_binding_part();
        if let Some(vars) = parse_variables(code, line_no, &self.path, &scope, in_type_binding_part)
        {
            self.record_variable_declarations(&vars);
            self.parsed.symbols.extend(vars);
            return;
        }
        self.mark_executable_statement();
    }

    fn mark_executable_statement(&mut self) {
        if let Some(frame) = self.scopes.last_mut() {
            frame.seen_executable = true;
        }
    }

    fn parse_statement_function(&mut self, line: usize, code: &str) -> bool {
        let Some(frame) = self.scopes.last() else {
            return false;
        };
        if frame.seen_executable
            || !matches!(
                frame.kind,
                SymbolKind::Program | SymbolKind::Subroutine | SymbolKind::Function
            )
        {
            return false;
        }
        let Some((name, args)) = parse_statement_function_lhs(code) else {
            return false;
        };
        let scope = self.current_scope();
        let col = find_ci(code, &name).unwrap_or(0);
        let range = Range {
            start: Position::new(line, col),
            end: Position::new(line, col + name.len()),
        };
        if let Some(sym) = self.parsed.symbols.iter_mut().find(|sym| {
            sym.kind == SymbolKind::Variable
                && sym.name.eq_ignore_ascii_case(&name)
                && sym.scope == scope
                && !sym
                    .attributes
                    .iter()
                    .any(|attr| attr.eq_ignore_ascii_case("dimension"))
        }) {
            sym.kind = SymbolKind::Function;
            sym.args = args;
            sym.signature = code.trim().to_string();
            sym.range = range.clone();
            sym.selection_range = range;
        } else {
            let mut sym = legacy_symbol(
                &name,
                SymbolKind::Function,
                code,
                line,
                &self.path,
                &scope,
                code.trim().to_string(),
            );
            sym.args = args;
            self.parsed.symbols.push(sym);
        }
        true
    }

    fn open_scope(&mut self, line: usize, start: ScopeStart) {
        self.diagnose_scope_before_contains(line, start.kind);
        let scope = self.current_scope();
        let source_line = self.source.lines().nth(line).unwrap_or_default();
        let scope_name = if is_construct_kind(start.kind) {
            if construct_label(source_line.trim()).is_some() {
                start.name.clone()
            } else {
                format!("{}@{}", start.name, line + 1)
            }
        } else {
            start.name.clone()
        };
        let col = find_ci(source_line, &start.selection)
            .map(|idx| utf16_col(source_line, idx))
            .unwrap_or(0);
        let selection_len = start.selection.encode_utf16().count();
        let idx = self.parsed.symbols.len();
        let selection_range = Range {
            start: Position::new(line, col),
            end: Position::new(line, col + selection_len),
        };
        let documentation = self.take_pending_doc();
        let default_visibility = default_visibility_for_scope(start.kind);
        let result = parse_result_name(&start.signature);
        let mut child_scope = self.current_scope();
        child_scope.push(scope_name.clone());
        self.parsed.symbols.push(Symbol {
            name: scope_name.clone(),
            kind: start.kind,
            file: self.path.clone(),
            range: Range {
                start: Position::new(line, 0),
                end: line_end(self.source, line),
            },
            selection_range,
            scope,
            signature: start.signature,
            args: start.args,
            documentation,
            visibility: start.visibility.unwrap_or(Visibility::Default),
            type_spec: None,
            attributes: start.attributes,
            result,
            is_parameter: false,
            is_external: false,
            extends: start.extends,
            is_abstract: start.is_abstract,
            binding_target: None,
            pass_arg: None,
            is_deferred: false,
            is_module_procedure: start.is_module_procedure,
            ancestor: start.ancestor,
        });
        for associate_name in &start.associate_names {
            let col = find_ci(source_line, associate_name)
                .map(|idx| utf16_col(source_line, idx))
                .unwrap_or(0);
            let len = associate_name.encode_utf16().count();
            self.parsed.symbols.push(Symbol {
                name: associate_name.clone(),
                kind: SymbolKind::Variable,
                file: self.path.clone(),
                range: Range {
                    start: Position::new(line, col),
                    end: Position::new(line, col + len),
                },
                selection_range: Range {
                    start: Position::new(line, col),
                    end: Position::new(line, col + len),
                },
                scope: child_scope.clone(),
                signature: format!("associate :: {}", associate_name),
                args: Vec::new(),
                documentation: None,
                visibility: Visibility::Default,
                type_spec: None,
                attributes: Vec::new(),
                result: None,
                is_parameter: false,
                is_external: false,
                extends: None,
                is_abstract: false,
                binding_target: None,
                pass_arg: None,
                is_deferred: false,
                is_module_procedure: false,
                ancestor: None,
            });
        }
        self.default_visibility
            .insert(child_scope, default_visibility);
        self.scopes.push(ScopeFrame {
            name: scope_name,
            kind: start.kind,
            symbol_idx: idx,
            default_visibility,
            implicit_line: None,
            implicit_none: false,
            use_after_implicit: false,
            contains_line: None,
            declared_variables: HashSet::new(),
            seen_executable: false,
        });
    }

    fn mark_implicit_statement(&mut self, line: usize, code: &str) {
        let Some(frame) = self.scopes.last_mut() else {
            self.statement_diagnostic(line, code, "IMPLICIT statement without enclosing scope");
            return;
        };
        frame.implicit_line.get_or_insert(line);
        frame.implicit_none = implicit_none_statement(code);
    }

    fn mark_use_statement(&mut self, line: usize) {
        if let Some(frame) = self.scopes.last_mut() {
            if frame
                .implicit_line
                .is_some_and(|implicit_line| line >= implicit_line)
            {
                frame.use_after_implicit = true;
            }
        }
    }

    fn mark_contains_statement(&mut self, line: usize, code: &str) {
        let Some(frame) = self.scopes.last_mut() else {
            self.statement_diagnostic(line, code, "CONTAINS statement without enclosing scope");
            return;
        };
        if frame.contains_line.is_some() {
            self.statement_diagnostic(line, code, "Multiple CONTAINS statements in scope");
            return;
        }
        frame.contains_line = Some(line);
    }

    fn diagnose_scope_before_contains(&mut self, line: usize, kind: SymbolKind) {
        if !matches!(kind, SymbolKind::Subroutine | SymbolKind::Function) {
            return;
        }
        let Some(parent) = self.scopes.last() else {
            return;
        };
        if !matches!(
            parent.kind,
            SymbolKind::Module
                | SymbolKind::Submodule
                | SymbolKind::Subroutine
                | SymbolKind::Function
        ) || parent.contains_line.is_some()
        {
            return;
        }
        let code = self.source.lines().nth(line).unwrap_or_default();
        self.statement_diagnostic(
            line,
            code,
            "Subroutine/Function definition before CONTAINS statement",
        );
    }

    fn add_use_after_implicit_diagnostics(&mut self) {
        let lines: Vec<_> = self
            .scopes
            .iter()
            .filter_map(use_after_implicit_line)
            .collect();
        for line in lines {
            self.use_after_implicit_diagnostic(line);
        }
    }

    fn use_after_implicit_diagnostic(&mut self, line: usize) {
        let code = self.source.lines().nth(line).unwrap_or("implicit");
        self.statement_diagnostic(line, code, "USE statements after IMPLICIT statement");
    }

    fn record_variable_declarations(&mut self, vars: &[Symbol]) {
        let Some(frame) = self.scopes.last() else {
            return;
        };
        let frame_args = self
            .parsed
            .symbols
            .get(frame.symbol_idx)
            .map(|sym| sym.args.clone())
            .unwrap_or_default();
        let procedure_scope = matches!(frame.kind, SymbolKind::Subroutine | SymbolKind::Function);
        let missing_intent_diagnostics: Vec<_> = vars
            .iter()
            .filter(|var| {
                procedure_scope
                    && has_intent_attribute(var)
                    && !frame_args
                        .iter()
                        .any(|arg| arg.eq_ignore_ascii_case(&var.name))
            })
            .map(|var| Diagnostic {
                range: var.range.clone(),
                severity: DiagnosticSeverity::Error,
                message: format!(
                    "Variable \"{}\" with INTENT keyword not found in argument list",
                    var.name
                ),
            })
            .collect();
        if let Some(frame) = self.scopes.last_mut() {
            for var in vars {
                frame
                    .declared_variables
                    .insert(var.name.to_ascii_lowercase());
            }
        }
        self.parsed.diagnostics.extend(missing_intent_diagnostics);
    }

    fn add_argument_diagnostics(&mut self, frame: &ScopeFrame) {
        if !matches!(frame.kind, SymbolKind::Subroutine | SymbolKind::Function) {
            return;
        }
        if !self.effective_implicit_none(frame) {
            return;
        }
        let Some(procedure) = self.parsed.symbols.get(frame.symbol_idx) else {
            return;
        };
        let missing_args: Vec<_> = procedure
            .args
            .iter()
            .filter(|arg| !frame.declared_variables.contains(&arg.to_ascii_lowercase()))
            .cloned()
            .collect();
        let range = procedure.selection_range.clone();
        for arg in missing_args {
            self.parsed.diagnostics.push(Diagnostic {
                range: range.clone(),
                severity: DiagnosticSeverity::Error,
                message: format!("No matching declaration found for argument \"{}\"", arg),
            });
        }
    }

    fn effective_implicit_none(&self, frame: &ScopeFrame) -> bool {
        frame.implicit_none || self.scopes.iter().any(|scope| scope.implicit_none)
    }

    fn close_scope(&mut self, line: usize, lower: &str) {
        let target_kind = if lower == "end" {
            None
        } else if lower.starts_with("end block data") || lower.starts_with("end blockdata") {
            // A `block data` unit opens a Program-kind scope, and `end block
            // data` must not be read as ending a `block` construct.
            Some(SymbolKind::Program)
        } else {
            let Some(kind_word) = lower.split_whitespace().nth(1) else {
                return;
            };
            let Some(kind) = end_kind(kind_word) else {
                return;
            };
            Some(kind)
        };
        if let Some(pos) = self
            .scopes
            .iter()
            .rposition(|scope| target_kind.is_none_or(|kind| scope.kind == kind))
        {
            let closing = self.scopes.split_off(pos);
            for scope in closing.into_iter().rev() {
                if let Some(diagnostic_line) = use_after_implicit_line(&scope) {
                    self.use_after_implicit_diagnostic(diagnostic_line);
                }
                self.add_argument_diagnostics(&scope);
                if let Some(sym) = self.parsed.symbols.get_mut(scope.symbol_idx) {
                    sym.range.end = line_end(self.source, line);
                }
            }
        } else {
            self.statement_diagnostic(
                line,
                lower,
                &format!(
                    "end statement has no matching {}scope",
                    target_kind
                        .map(|kind| format!("{} ", kind.label()))
                        .unwrap_or_default()
                ),
            );
        }
    }

    fn statement_diagnostic(&mut self, line: usize, code: &str, message: &str) {
        self.parsed.diagnostics.push(Diagnostic {
            range: Range {
                start: Position::new(line, 0),
                end: Position::new(line, code.encode_utf16().count()),
            },
            severity: DiagnosticSeverity::Error,
            message: message.to_string(),
        });
    }

    fn current_scope(&self) -> Vec<String> {
        self.scopes.iter().map(|scope| scope.name.clone()).collect()
    }

    fn take_pending_doc(&mut self) -> Option<String> {
        (!self.pending_doc.is_empty())
            .then(|| self.pending_doc.drain(..).collect::<Vec<_>>().join("\n"))
    }

    fn apply_visibility_stmt(&mut self, stmt: &VisibilityStmt) {
        if stmt.names.is_empty() {
            self.default_visibility
                .insert(stmt.scope.clone(), stmt.visibility);
            if let Some(frame) = self.scopes.last_mut() {
                frame.default_visibility = stmt.visibility;
            }
            return;
        }
        for name in &stmt.names {
            self.explicit_visibility.insert(
                (stmt.scope.clone(), name.to_ascii_lowercase()),
                stmt.visibility,
            );
        }
    }

    fn apply_symbol_visibility(&mut self) {
        for sym in &mut self.parsed.symbols {
            if let Some(vis) = self
                .explicit_visibility
                .get(&(sym.scope.clone(), sym.name.to_ascii_lowercase()))
            {
                sym.visibility = *vis;
                continue;
            }
            if sym.scope.len() == 1 && sym.visibility == Visibility::Default {
                sym.visibility = self
                    .default_visibility
                    .get(&sym.scope)
                    .copied()
                    .unwrap_or(Visibility::Public);
            }
        }
    }

    fn apply_preprocessor(&mut self, directive: &PreprocessorDirective) {
        match directive.kind {
            PreprocessorKind::If | PreprocessorKind::Ifdef | PreprocessorKind::Ifndef => {
                let parent_active = self.preprocessor_active();
                let condition = self.eval_preprocessor_directive(directive);
                self.preprocessor_stack.push(PreprocessorFrame {
                    start: directive.range.start,
                    range: directive.range.clone(),
                    saw_else: false,
                    parent_active,
                    branch_active: parent_active && condition,
                    any_taken: condition,
                });
            }
            PreprocessorKind::Elif => {
                let condition = self.eval_preprocessor_directive(directive);
                if let Some(frame) = self.preprocessor_stack.last_mut() {
                    if frame.saw_else {
                        self.parsed.diagnostics.push(Diagnostic {
                            range: directive.range.clone(),
                            severity: DiagnosticSeverity::Error,
                            message: "`#elif` cannot appear after `#else`".to_string(),
                        });
                    }
                    self.parsed.preprocessor_regions.push(PreprocessorRegion {
                        start: frame.start,
                        end: directive.range.start,
                    });
                    frame.start = directive.range.start;
                    if frame.any_taken {
                        frame.branch_active = false;
                    } else {
                        frame.branch_active = frame.parent_active && condition;
                        frame.any_taken = condition;
                    }
                } else {
                    self.unmatched_preprocessor_diagnostic(directive, "`#elif` without `#if`");
                }
            }
            PreprocessorKind::Else => {
                if let Some(frame) = self.preprocessor_stack.last_mut() {
                    if frame.saw_else {
                        self.parsed.diagnostics.push(Diagnostic {
                            range: directive.range.clone(),
                            severity: DiagnosticSeverity::Error,
                            message: "duplicate `#else` in preprocessor conditional".to_string(),
                        });
                    }
                    self.parsed.preprocessor_regions.push(PreprocessorRegion {
                        start: frame.start,
                        end: directive.range.start,
                    });
                    frame.start = directive.range.start;
                    frame.branch_active = frame.parent_active && !frame.any_taken;
                    frame.any_taken = true;
                    frame.saw_else = true;
                } else {
                    self.unmatched_preprocessor_diagnostic(directive, "`#else` without `#if`");
                }
            }
            PreprocessorKind::Endif => {
                if let Some(frame) = self.preprocessor_stack.pop() {
                    self.parsed.preprocessor_regions.push(PreprocessorRegion {
                        start: frame.start,
                        end: directive.range.end,
                    });
                } else {
                    self.unmatched_preprocessor_diagnostic(directive, "`#endif` without `#if`");
                }
            }
            PreprocessorKind::Define => {
                if self.preprocessor_active() {
                    if let Some(name) = &directive.name {
                        self.parsed
                            .preprocessor_definitions
                            .insert(name.clone(), directive.argument.clone().unwrap_or_default());
                        self.preprocessor_macros.insert(
                            name.clone(),
                            macro_definition(directive.argument.as_deref()),
                        );
                    }
                }
            }
            PreprocessorKind::Undef => {
                if self.preprocessor_active() {
                    if let Some(name) = &directive.name {
                        self.parsed.preprocessor_definitions.remove(name);
                        self.preprocessor_macros.remove(name);
                    }
                }
            }
            PreprocessorKind::Include => {}
        }
    }

    fn apply_preprocessor_include_macros(&mut self, directive: &PreprocessorDirective) {
        let Some(path) = directive
            .argument
            .as_deref()
            .and_then(|path| self.resolve_preprocessor_include_path(path))
        else {
            return;
        };
        let mut visited = HashSet::from([self.path.clone()]);
        self.load_preprocessor_include_macros(&path, &mut visited);
    }

    fn load_preprocessor_include_macros(&mut self, path: &Path, visited: &mut HashSet<PathBuf>) {
        let path = normalize_path(path);
        if !visited.insert(path.clone()) {
            return;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            return;
        };
        let known = self.parsed.preprocessor_definitions.clone();
        for (line_no, raw_line) in logical_lines(&path, &source, &known) {
            let raw_code = raw_line.trim();
            let Some(directive) =
                parse_preprocessor(raw_code, line_no, &path, &self.current_scope())
            else {
                continue;
            };
            let was_active = self.preprocessor_active();
            self.apply_preprocessor(&directive);
            if was_active && directive.kind == PreprocessorKind::Include {
                if let Some(path) = directive.argument.as_deref().and_then(|path| {
                    self.resolve_preprocessor_include_path_from(&directive.file, path)
                }) {
                    self.load_preprocessor_include_macros(&path, visited);
                }
            }
        }
    }

    fn resolve_preprocessor_include_path(&self, include: &str) -> Option<PathBuf> {
        self.resolve_preprocessor_include_path_from(&self.path, include)
    }

    fn resolve_preprocessor_include_path_from(
        &self,
        from: &Path,
        include: &str,
    ) -> Option<PathBuf> {
        let include = Path::new(include);
        if include.is_absolute() {
            return include.exists().then(|| include.to_path_buf());
        }
        from.parent()
            .map(|parent| parent.join(include))
            .filter(|path| path.exists())
    }

    fn unmatched_preprocessor_diagnostic(&mut self, directive: &PreprocessorDirective, msg: &str) {
        self.parsed.diagnostics.push(Diagnostic {
            range: directive.range.clone(),
            severity: DiagnosticSeverity::Error,
            message: msg.to_string(),
        });
    }

    fn preprocessor_active(&self) -> bool {
        self.preprocessor_stack
            .iter()
            .all(|frame| frame.branch_active)
    }

    fn in_interface_scope(&self) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope.kind == SymbolKind::Interface)
    }

    fn in_type_binding_part(&self) -> bool {
        self.scopes
            .last()
            .is_some_and(|scope| scope.kind == SymbolKind::Type && scope.contains_line.is_some())
    }

    fn eval_preprocessor_directive(&self, directive: &PreprocessorDirective) -> bool {
        match directive.kind {
            PreprocessorKind::If => directive.argument.as_deref().is_some_and(|expr| {
                eval_preprocessor_expr(expr, &self.parsed.preprocessor_definitions)
            }),
            PreprocessorKind::Ifdef => directive
                .name
                .as_ref()
                .is_some_and(|name| self.parsed.preprocessor_definitions.contains_key(name)),
            PreprocessorKind::Ifndef => directive
                .name
                .as_ref()
                .is_some_and(|name| !self.parsed.preprocessor_definitions.contains_key(name)),
            PreprocessorKind::Elif => directive.argument.as_deref().is_some_and(|expr| {
                eval_preprocessor_expr(expr, &self.parsed.preprocessor_definitions)
            }),
            _ => false,
        }
    }

    fn expand_macros(&self, line: &str) -> String {
        let mut expanded = line.to_string();
        for _ in 0..8 {
            let next = expand_macro_once(&expanded, &self.preprocessor_macros);
            if next == expanded {
                return next;
            }
            expanded = next;
        }
        expanded
    }

    /// `entry name[(args)]` inside a procedure defines an additional external
    /// entry point — a sibling of the enclosing procedure, with its kind.
    fn parse_entry_statement(&self, line: usize, code: &str) -> Option<Vec<Symbol>> {
        let rest = after_keyword(code, "entry")?;
        let enclosing = self.scopes.last()?;
        if !matches!(
            enclosing.kind,
            SymbolKind::Function | SymbolKind::Subroutine
        ) {
            return None;
        }
        let name = first_ident(rest)?;
        if !rest.trim_start().starts_with(name) {
            return None;
        }
        let scope = self.current_scope();
        let parent_scope = &scope[..scope.len() - 1];
        let args = arg_list(rest).unwrap_or_default();
        let signature = if args.is_empty() {
            format!("entry {name}()")
        } else {
            format!("entry {name}({})", args.join(", "))
        };
        let mut sym = legacy_symbol(
            name,
            enclosing.kind,
            code,
            line,
            &self.path,
            parent_scope,
            signature,
        );
        sym.args = args;
        Some(vec![sym])
    }

    /// Add pending `common`/`namelist` symbols for names that got no other
    /// symbol in the same scope (see `pending_legacy_symbols`).
    fn flush_pending_legacy_symbols(&mut self) {
        if self.pending_legacy_symbols.is_empty() {
            return;
        }
        let mut seen: HashSet<(Vec<String>, String)> = self
            .parsed
            .symbols
            .iter()
            .map(|sym| (sym.scope.clone(), sym.name.to_ascii_lowercase()))
            .collect();
        for sym in std::mem::take(&mut self.pending_legacy_symbols) {
            let key = (sym.scope.clone(), sym.name.to_ascii_lowercase());
            if seen.insert(key) {
                self.parsed.symbols.push(sym);
            }
        }
    }

    fn add_duplicate_diagnostics(&mut self) {
        let mut seen: HashMap<(Vec<String>, String), Position> = HashMap::new();
        for sym in &self.parsed.symbols {
            if is_interface_symbol(sym) || is_module_procedure_link_symbol(sym) {
                continue;
            }
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

    fn add_parent_masking_diagnostics(&mut self) {
        // Cheap prefilter: a variable can only mask something if another
        // symbol anywhere shares its name, or a use-only list imports it —
        // every expensive check below matches candidates by name. Skipping
        // unique names avoids the O(symbols) ancestor scans, which are
        // quadratic on large legacy files without this.
        let mut name_counts: HashMap<String, usize> = HashMap::new();
        for sym in &self.parsed.symbols {
            *name_counts
                .entry(sym.name.to_ascii_lowercase())
                .or_default() += 1;
        }
        let use_only_names: HashSet<String> = self
            .parsed
            .uses
            .iter()
            .flat_map(|use_stmt| use_stmt.only.iter())
            .map(|name| name.to_ascii_lowercase())
            .collect();
        let may_mask = |sym: &Symbol| {
            let name = sym.name.to_ascii_lowercase();
            name_counts.get(&name).copied().unwrap_or(0) > 1 || use_only_names.contains(&name)
        };
        let mut diagnostics: Vec<_> = self
            .parsed
            .symbols
            .iter()
            .filter(|sym| sym.kind == SymbolKind::Variable)
            .filter(|sym| may_mask(sym))
            .filter(|sym| {
                matches!(
                    self.scope_owner_kind(&sym.scope),
                    Some(SymbolKind::Subroutine | SymbolKind::Function | SymbolKind::Block)
                )
            })
            .filter(|sym| self.parent_variable(sym).is_some() || self.parent_use_only_name(sym))
            .map(|sym| Diagnostic {
                range: sym.range.clone(),
                severity: DiagnosticSeverity::Warning,
                message: format!("Variable \"{}\" masks variable in parent scope", sym.name),
            })
            .collect();
        diagnostics.extend(
            self.parsed
                .symbols
                .iter()
                .filter(|sym| sym.kind == SymbolKind::Function)
                .filter(|sym| {
                    self.module_type_member_named(sym, &sym.name, type_member_mask_candidate)
                        .is_some()
                })
                .filter(|sym| !self.is_direct_type_bound_target(sym))
                .map(|sym| Diagnostic {
                    range: sym.range.clone(),
                    severity: DiagnosticSeverity::Warning,
                    message: format!("Variable \"{}\" masks variable in parent scope", sym.name),
                }),
        );
        diagnostics.extend(
            self.parsed
                .symbols
                .iter()
                .filter(|sym| sym.kind == SymbolKind::Function)
                .filter(|sym| self.line_interface_state(sym.range.start.line) == Some(false))
                .filter_map(|sym| {
                    let result = sym.result.as_deref()?;
                    if !self.function_has_explicit_result_declaration(sym, result) {
                        return None;
                    }
                    self.module_type_member_named(sym, result, |candidate| {
                        candidate.kind == SymbolKind::Method
                            && candidate
                                .binding_target
                                .as_deref()
                                .is_none_or(|target| target.eq_ignore_ascii_case(&candidate.name))
                    })?;
                    Some(Diagnostic {
                        range: sym.range.clone(),
                        severity: DiagnosticSeverity::Warning,
                        message: format!("Variable \"{}\" masks variable in parent scope", result),
                    })
                }),
        );
        self.parsed.diagnostics.extend(diagnostics);
    }

    fn scope_owner_kind(&self, scope: &[String]) -> Option<SymbolKind> {
        let (name, parent_scope) = scope.split_last()?;
        self.parsed.symbols.iter().find_map(|sym| {
            (sym.name.eq_ignore_ascii_case(name)
                && scopes_equal_case_insensitive(&sym.scope, parent_scope)
                && is_scope_kind(sym.kind))
            .then_some(sym.kind)
        })
    }

    fn scope_contains_abstract_interface(&self, scope: &[String]) -> bool {
        (0..=scope.len()).any(|len| {
            let prefix = &scope[..len];
            let Some((name, parent_scope)) = prefix.split_last() else {
                return false;
            };
            self.parsed.symbols.iter().any(|sym| {
                sym.kind == SymbolKind::Interface
                    && sym.is_abstract
                    && sym.name.eq_ignore_ascii_case(name)
                    && scopes_equal_case_insensitive(&sym.scope, parent_scope)
            })
        })
    }

    fn line_is_inside_abstract_interface(&self, line: usize) -> bool {
        self.line_interface_state(line)
            .is_some_and(|is_abstract| is_abstract)
    }

    /// Interface nesting state just before `line`: `Some(true)` inside an
    /// abstract interface, `Some(false)` inside a plain one, `None` outside.
    /// Computed once for the whole file — the masking pass queries this per
    /// candidate, and rescanning the source each call was quadratic.
    fn line_interface_state(&self, line: usize) -> Option<bool> {
        let states = self.interface_line_states.get_or_init(|| {
            let mut states = Vec::new();
            let mut interfaces: Vec<bool> = Vec::new();
            for source_line in self.source.lines() {
                states.push(interfaces.last().copied());
                let code = strip_inline_comment(source_line);
                let code = code.trim();
                let lower = code.to_ascii_lowercase();
                if lower.starts_with("end interface") {
                    interfaces.pop();
                } else if lower.starts_with("abstract interface") {
                    interfaces.push(true);
                } else if lower.starts_with("interface") {
                    interfaces.push(false);
                }
            }
            states
        });
        states.get(line).copied().flatten()
    }

    fn parent_variable(&self, sym: &Symbol) -> Option<&Symbol> {
        if sym.scope.is_empty() {
            return None;
        }
        if self.is_function_result(sym)
            && (self.scope_contains_abstract_interface(&sym.scope)
                || self.line_is_inside_abstract_interface(sym.range.start.line))
        {
            return None;
        }
        (0..sym.scope.len())
            .rev()
            .find_map(|len| {
                let ancestor_scope = &sym.scope[..len];
                self.parsed.symbols.iter().find(|candidate| {
                    candidate.kind == SymbolKind::Variable
                        && candidate.name.eq_ignore_ascii_case(&sym.name)
                        && scopes_equal_case_insensitive(&candidate.scope, ancestor_scope)
                        && (!self.symbol_parent_is_type(candidate)
                            || self.symbol_line_inside_parent_type(sym, candidate))
                })
            })
            .or_else(|| self.module_callable_or_interface_named(sym, &sym.name))
            .or_else(|| {
                let in_abstract_interface = self.scope_contains_abstract_interface(&sym.scope)
                    || self.line_is_inside_abstract_interface(sym.range.start.line);
                self.module_type_member_named(sym, &sym.name, |candidate| {
                    (candidate.kind == SymbolKind::Method
                        && candidate
                            .binding_target
                            .as_deref()
                            .is_none_or(|target| target.eq_ignore_ascii_case(&candidate.name))
                        && (!in_abstract_interface
                            || candidate
                                .binding_target
                                .as_deref()
                                .is_none_or(|target| target.eq_ignore_ascii_case(&candidate.name))))
                        || (candidate.kind == SymbolKind::Variable
                            && self.is_implicit_function_result(sym))
                })
                .filter(|_| !self.is_direct_type_bound_result(sym))
            })
    }

    fn parent_use_only_name(&self, sym: &Symbol) -> bool {
        self.parsed.uses.iter().any(|use_stmt| {
            !use_stmt.only.is_empty()
                && use_stmt.scope.len() < sym.scope.len()
                && scopes_equal_case_insensitive(
                    &use_stmt.scope,
                    &sym.scope[..use_stmt.scope.len()],
                )
                && use_stmt
                    .only
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&sym.name))
        })
    }

    fn module_callable_or_interface_named(&self, sym: &Symbol, name: &str) -> Option<&Symbol> {
        if self.is_direct_type_bound_result(sym) {
            return None;
        }
        if self.is_implicit_function_result(sym) {
            return None;
        }
        if self.scope_contains_abstract_interface(&sym.scope)
            || self.line_is_inside_abstract_interface(sym.range.start.line)
            || self.line_interface_state(sym.range.start.line).is_some()
        {
            return None;
        }
        let module = sym.scope.first()?;
        self.parsed.symbols.iter().find(|candidate| {
            matches!(
                candidate.kind,
                SymbolKind::Function | SymbolKind::Subroutine | SymbolKind::Interface
            ) && candidate.name.eq_ignore_ascii_case(name)
                && candidate.scope.len() == 1
                && candidate.scope[0].eq_ignore_ascii_case(module)
        })
    }

    fn module_type_member_named(
        &self,
        sym: &Symbol,
        name: &str,
        mut candidate_filter: impl FnMut(&Symbol) -> bool,
    ) -> Option<&Symbol> {
        let module = sym.scope.first()?;
        self.parsed.symbols.iter().find(|candidate| {
            candidate_filter(candidate)
                && candidate.name.eq_ignore_ascii_case(name)
                && candidate.scope.len() >= 2
                && candidate.scope[0].eq_ignore_ascii_case(module)
                && self.symbol_parent_is_type(candidate)
        })
    }

    fn symbol_parent_is_type(&self, sym: &Symbol) -> bool {
        let Some((parent_name, parent_scope)) = sym.scope.split_last() else {
            return false;
        };
        self.parsed.symbols.iter().any(|candidate| {
            candidate.kind == SymbolKind::Type
                && candidate.name.eq_ignore_ascii_case(parent_name)
                && scopes_equal_case_insensitive(&candidate.scope, parent_scope)
                && sym.range.start.line >= candidate.range.start.line
                && sym.range.start.line <= candidate.range.end.line
        })
    }

    fn symbol_line_inside_parent_type(&self, sym: &Symbol, parent_sym: &Symbol) -> bool {
        let Some((parent_name, parent_scope)) = parent_sym.scope.split_last() else {
            return false;
        };
        self.parsed.symbols.iter().any(|candidate| {
            candidate.kind == SymbolKind::Type
                && candidate.name.eq_ignore_ascii_case(parent_name)
                && scopes_equal_case_insensitive(&candidate.scope, parent_scope)
                && sym.range.start.line >= candidate.range.start.line
                && sym.range.start.line <= candidate.range.end.line
        })
    }

    fn is_function_result(&self, sym: &Symbol) -> bool {
        let Some(function_name) = sym.scope.last() else {
            return false;
        };
        if self.scope_owner_kind(&sym.scope) != Some(SymbolKind::Function) {
            return false;
        }
        if sym.name.eq_ignore_ascii_case(function_name) {
            return true;
        }
        let function_scope = &sym.scope[..sym.scope.len() - 1];
        self.parsed.symbols.iter().any(|candidate| {
            candidate.kind == SymbolKind::Function
                && candidate.name.eq_ignore_ascii_case(function_name)
                && scopes_equal_case_insensitive(&candidate.scope, function_scope)
                && candidate
                    .result
                    .as_deref()
                    .is_some_and(|result| result.eq_ignore_ascii_case(&sym.name))
        })
    }

    fn is_implicit_function_result(&self, sym: &Symbol) -> bool {
        sym.scope
            .last()
            .is_some_and(|function_name| sym.name.eq_ignore_ascii_case(function_name))
            && self.is_function_result(sym)
    }

    fn function_has_explicit_result_declaration(&self, sym: &Symbol, result: &str) -> bool {
        let start = sym.range.start.line.saturating_add(1);
        for source_line in self.source.lines().skip(start) {
            let code = strip_inline_comment(source_line);
            let code = code.trim();
            let lower = code.to_ascii_lowercase();
            if lower.starts_with("end function") {
                return false;
            }
            if !code.contains("::") {
                continue;
            }
            let Some((_, rhs)) = code.split_once("::") else {
                continue;
            };
            if rhs.split(',').any(|item| {
                first_ident(item.trim()).is_some_and(|name| name.eq_ignore_ascii_case(result))
            }) {
                return true;
            }
        }
        false
    }

    fn is_direct_type_bound_result(&self, sym: &Symbol) -> bool {
        let Some(function_name) = sym.scope.last() else {
            return false;
        };
        if !sym.name.eq_ignore_ascii_case(function_name) {
            return false;
        }
        self.type_bound_methods_for_target(function_name)
            .into_iter()
            .any(|method| method.name.eq_ignore_ascii_case(function_name))
    }

    fn is_direct_type_bound_target(&self, sym: &Symbol) -> bool {
        self.type_bound_methods_for_target(&sym.name)
            .into_iter()
            .any(|method| method.name.eq_ignore_ascii_case(&sym.name))
    }

    fn type_bound_methods_for_target(&self, target: &str) -> Vec<&Symbol> {
        self.parsed
            .symbols
            .iter()
            .filter(|candidate| candidate.kind == SymbolKind::Method)
            .filter(|method| {
                method
                    .binding_target
                    .as_deref()
                    .unwrap_or(&method.name)
                    .eq_ignore_ascii_case(target)
            })
            .collect()
    }
}

fn is_interface_symbol(sym: &Symbol) -> bool {
    sym.kind == SymbolKind::Interface
}

fn is_module_procedure_link_symbol(sym: &Symbol) -> bool {
    matches!(sym.kind, SymbolKind::Subroutine | SymbolKind::Function)
        && sym
            .signature
            .get(..sym.signature.len().min("module procedure".len()))
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("module procedure"))
}

fn type_member_mask_candidate(sym: &Symbol) -> bool {
    matches!(sym.kind, SymbolKind::Variable | SymbolKind::Method)
}

fn use_after_implicit_line(frame: &ScopeFrame) -> Option<usize> {
    frame.use_after_implicit.then_some(frame.implicit_line?)
}

pub(crate) fn scopes_equal_case_insensitive(left: &[String], right: &[String]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn implicit_none_statement(code: &str) -> bool {
    after_keyword(code, "implicit")
        .is_some_and(|rest| rest.trim_start().to_ascii_lowercase().starts_with("none"))
}

fn has_intent_attribute(sym: &Symbol) -> bool {
    sym.attributes
        .iter()
        .any(|attr| attr.to_ascii_lowercase().starts_with("intent"))
}

fn logical_lines(
    path: &Path,
    source: &str,
    predefined: &HashMap<String, String>,
) -> Vec<(usize, String)> {
    let physical_lines = filter_inactive_preprocessor_lines(
        expand_preprocessor_include_lines(path, source, &mut HashSet::new()),
        predefined,
    );
    if is_fixed_form_path(path) {
        fixed_logical_lines(physical_lines)
    } else {
        free_logical_lines(physical_lines)
    }
}

fn free_logical_lines(physical_lines: Vec<(usize, String)>) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut start = 0;
    for (idx, line) in physical_lines {
        let raw_trimmed = line.trim_end();
        let code = strip_inline_comment(&line);
        let trimmed = code.trim_end();
        if raw_trimmed.trim().is_empty() && !current.is_empty() {
            continue;
        }
        if raw_trimmed.trim_start().starts_with('#') {
            if !current.is_empty() {
                out.push((idx, raw_trimmed.trim().to_string()));
                continue;
            }
            out.push((idx, raw_trimmed.trim().to_string()));
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

fn fixed_logical_lines(physical_lines: Vec<(usize, String)>) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut start = 0usize;
    for (idx, line) in physical_lines {
        if is_fixed_comment(&line) {
            continue;
        }
        let raw_trimmed = line.trim_end();
        if raw_trimmed.trim_start().starts_with('#') {
            if !current.is_empty() {
                out.push((idx, raw_trimmed.trim().to_string()));
                continue;
            }
            out.push((idx, raw_trimmed.trim().to_string()));
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

fn expand_preprocessor_include_lines(
    path: &Path,
    source: &str,
    visited: &mut HashSet<PathBuf>,
) -> Vec<(usize, String)> {
    let normalized = normalize_path(path);
    if !visited.insert(normalized) {
        return source
            .lines()
            .enumerate()
            .map(|(idx, line)| (idx, line.to_string()))
            .collect();
    }

    let mut out = Vec::new();
    for (idx, line) in source.lines().enumerate() {
        out.push((idx, line.to_string()));
        let trimmed = line.trim();
        let Some(include) = trimmed
            .strip_prefix("#include")
            .and_then(quoted_path)
            .and_then(|include| resolve_include_path_from(path, &include))
        else {
            continue;
        };
        let Ok(included_source) = std::fs::read_to_string(&include) else {
            continue;
        };
        out.extend(expand_preprocessor_include_lines(
            &include,
            &included_source,
            visited,
        ));
    }
    visited.remove(&normalize_path(path));
    out
}

fn resolve_include_path_from(from: &Path, include: &str) -> Option<PathBuf> {
    let include = Path::new(include);
    if include.is_absolute() && include.exists() {
        return Some(include.to_path_buf());
    }
    from.parent()
        .map(|parent| parent.join(include))
        .filter(|candidate| candidate.exists())
}

fn filter_inactive_preprocessor_lines(
    lines: Vec<(usize, String)>,
    predefined: &HashMap<String, String>,
) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut state = FoldPreprocessorState {
        definitions: predefined.clone(),
        ..FoldPreprocessorState::default()
    };
    for (idx, line) in lines {
        let trimmed = line.trim();
        if let Some(directive) = parse_preprocessor(trimmed, idx, Path::new("<fold>"), &[]) {
            state.apply(&directive);
            out.push((idx, line));
        } else if state.active() {
            out.push((idx, line));
        }
    }
    out
}

#[derive(Default)]
struct FoldPreprocessorState {
    stack: Vec<FoldPreprocessorFrame>,
    definitions: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct FoldPreprocessorFrame {
    parent_active: bool,
    branch_active: bool,
    any_taken: bool,
    saw_else: bool,
}

impl FoldPreprocessorState {
    fn active(&self) -> bool {
        self.stack.iter().all(|frame| frame.branch_active)
    }

    fn apply(&mut self, directive: &PreprocessorDirective) {
        match directive.kind {
            PreprocessorKind::If | PreprocessorKind::Ifdef | PreprocessorKind::Ifndef => {
                let parent_active = self.active();
                let condition = match directive.kind {
                    PreprocessorKind::If => directive
                        .argument
                        .as_deref()
                        .is_some_and(|expr| eval_preprocessor_expr(expr, &self.definitions)),
                    PreprocessorKind::Ifdef => directive
                        .name
                        .as_ref()
                        .is_some_and(|name| self.definitions.contains_key(name)),
                    PreprocessorKind::Ifndef => directive
                        .name
                        .as_ref()
                        .is_some_and(|name| !self.definitions.contains_key(name)),
                    _ => false,
                };
                let branch_active = parent_active && condition;
                self.stack.push(FoldPreprocessorFrame {
                    parent_active,
                    branch_active,
                    any_taken: branch_active,
                    saw_else: false,
                });
            }
            PreprocessorKind::Elif => {
                let Some(frame) = self.stack.last_mut() else {
                    return;
                };
                let condition = directive
                    .argument
                    .as_deref()
                    .is_some_and(|expr| eval_preprocessor_expr(expr, &self.definitions));
                frame.branch_active =
                    frame.parent_active && !frame.saw_else && !frame.any_taken && condition;
                frame.any_taken |= frame.branch_active;
            }
            PreprocessorKind::Else => {
                let Some(frame) = self.stack.last_mut() else {
                    return;
                };
                frame.branch_active = frame.parent_active && !frame.saw_else && !frame.any_taken;
                frame.any_taken |= frame.branch_active;
                frame.saw_else = true;
            }
            PreprocessorKind::Endif => {
                self.stack.pop();
            }
            PreprocessorKind::Define => {
                if self.active() {
                    if let Some(name) = &directive.name {
                        self.definitions.insert(
                            name.clone(),
                            directive
                                .argument
                                .clone()
                                .unwrap_or_else(|| "1".to_string()),
                        );
                    }
                }
            }
            PreprocessorKind::Undef => {
                if self.active() {
                    if let Some(name) = &directive.name {
                        self.definitions.remove(name);
                    }
                }
            }
            PreprocessorKind::Include => {}
        }
    }
}

pub(crate) fn is_fixed_form_path(path: &Path) -> bool {
    if path_has_free_form_hint(path) {
        return false;
    }
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext,
                "f" | "for" | "ftn" | "f77" | "F" | "FOR" | "FTN" | "F77"
            )
        })
}

fn path_has_free_form_hint(path: &Path) -> bool {
    path.components().any(|component| {
        component.as_os_str().to_str().is_some_and(|part| {
            let part = part.to_ascii_lowercase();
            part == "free-form" || part == "free_form" || part == "freeform"
        })
    })
}

pub(crate) fn is_fixed_comment(line: &str) -> bool {
    line.as_bytes()
        .first()
        .is_some_and(|ch| matches!(*ch as char, 'c' | 'C' | '*' | '!'))
}

#[derive(Debug, Clone)]
struct ScopeStart {
    kind: SymbolKind,
    name: String,
    args: Vec<String>,
    signature: String,
    selection: String,
    attributes: Vec<String>,
    visibility: Option<Visibility>,
    extends: Option<String>,
    is_abstract: bool,
    is_module_procedure: bool,
    ancestor: Option<String>,
    associate_names: Vec<String>,
}

fn parse_scope_start(code: &str) -> Option<ScopeStart> {
    let code = strip_procedure_prefixes(code);
    let lower = code.to_ascii_lowercase();
    if let Some(rest) = after_keyword_words(code, "module procedure") {
        let name = first_ident(rest)?.to_string();
        return Some(ScopeStart {
            kind: SymbolKind::Subroutine,
            selection: name.clone(),
            name,
            args: Vec::new(),
            signature: code.trim().to_string(),
            attributes: Vec::new(),
            visibility: None,
            extends: None,
            is_abstract: false,
            is_module_procedure: true,
            ancestor: None,
            associate_names: Vec::new(),
        });
    }
    if let Some(submodule) = parse_submodule_definition(code) {
        return Some(submodule);
    }
    if let Some(rest) = after_keyword_words(code, "module subroutine") {
        let name = first_ident(rest)?.to_string();
        return Some(ScopeStart {
            kind: SymbolKind::Subroutine,
            selection: name.clone(),
            name,
            args: arg_list(rest).unwrap_or_default(),
            signature: code.trim().to_string(),
            attributes: Vec::new(),
            visibility: None,
            extends: None,
            is_abstract: false,
            is_module_procedure: false,
            ancestor: None,
            associate_names: Vec::new(),
        });
    }
    if let Some(rest) = after_keyword_words(code, "module function") {
        let name = first_ident(rest)?.to_string();
        return Some(ScopeStart {
            kind: SymbolKind::Function,
            selection: name.clone(),
            name,
            args: arg_list(rest).unwrap_or_default(),
            signature: code.trim().to_string(),
            attributes: Vec::new(),
            visibility: None,
            extends: None,
            is_abstract: false,
            is_module_procedure: false,
            ancestor: None,
            associate_names: Vec::new(),
        });
    }
    if let Some(rest) = typed_module_function_rest(code) {
        let name = first_ident(rest)?.to_string();
        return Some(ScopeStart {
            kind: SymbolKind::Function,
            selection: name.clone(),
            name,
            args: arg_list(rest).unwrap_or_default(),
            signature: code.trim().to_string(),
            attributes: Vec::new(),
            visibility: None,
            extends: None,
            is_abstract: false,
            is_module_procedure: false,
            ancestor: None,
            associate_names: Vec::new(),
        });
    }
    if let Some(rest) = typed_function_rest(code) {
        let name = first_ident(rest)?.to_string();
        return Some(ScopeStart {
            kind: SymbolKind::Function,
            selection: name.clone(),
            name,
            args: arg_list(rest).unwrap_or_default(),
            signature: code.trim().to_string(),
            attributes: Vec::new(),
            visibility: None,
            extends: None,
            is_abstract: false,
            is_module_procedure: false,
            ancestor: None,
            associate_names: Vec::new(),
        });
    }
    if let Some(scope) = parse_construct_scope(code) {
        return Some(scope);
    }
    if let Some(scope) = parse_abstract_interface_scope(code) {
        return Some(scope);
    }
    if let Some(scope) = parse_generic_interface_scope(code) {
        return Some(scope);
    }
    for (keyword, kind) in [
        ("module", SymbolKind::Module),
        ("program", SymbolKind::Program),
        ("subroutine", SymbolKind::Subroutine),
        ("function", SymbolKind::Function),
        ("interface", SymbolKind::Interface),
    ] {
        if let Some(rest) = after_keyword(code, keyword) {
            let name = first_ident(rest).unwrap_or(keyword);
            let args = arg_list(rest).unwrap_or_default();
            return Some(ScopeStart {
                kind,
                name: name.to_string(),
                selection: name.to_string(),
                args,
                signature: code.trim().to_string(),
                attributes: Vec::new(),
                visibility: None,
                extends: None,
                is_abstract: lower.contains("abstract interface"),
                is_module_procedure: false,
                ancestor: None,
                associate_names: Vec::new(),
            });
        }
    }
    if is_derived_type_definition(code) {
        let type_info = parse_type_definition(code)?;
        return Some(ScopeStart {
            kind: SymbolKind::Type,
            selection: type_info.name.clone(),
            name: type_info.name,
            args: Vec::new(),
            signature: code.trim().to_string(),
            attributes: type_info.attributes,
            visibility: type_info.visibility,
            extends: type_info.extends,
            is_abstract: type_info.is_abstract,
            is_module_procedure: false,
            ancestor: None,
            associate_names: Vec::new(),
        });
    }
    None
}

fn parse_module_procedure_prototypes(
    code: &str,
    line: usize,
    file: &Path,
    scope: &[String],
) -> Vec<Symbol> {
    let Some(rest) = after_keyword_words(code, "module procedure") else {
        return Vec::new();
    };
    split_names(rest)
        .into_iter()
        .map(|name| {
            let start_col = find_ci(code, &name)
                .map(|idx| utf16_col(code, idx))
                .unwrap_or(0);
            let len = name.encode_utf16().count();
            Symbol {
                name: name.clone(),
                kind: SymbolKind::Subroutine,
                file: file.to_path_buf(),
                range: Range {
                    start: Position::new(line, 0),
                    end: Position::new(line, code.encode_utf16().count()),
                },
                selection_range: Range {
                    start: Position::new(line, start_col),
                    end: Position::new(line, start_col + len),
                },
                scope: scope.to_vec(),
                signature: format!("module procedure {name}"),
                args: Vec::new(),
                documentation: None,
                visibility: Visibility::Default,
                type_spec: None,
                attributes: Vec::new(),
                result: None,
                is_parameter: false,
                is_external: false,
                extends: None,
                is_abstract: false,
                binding_target: None,
                pass_arg: None,
                is_deferred: false,
                is_module_procedure: false,
                ancestor: None,
            }
        })
        .collect()
}

fn parse_abstract_interface_scope(code: &str) -> Option<ScopeStart> {
    after_keyword(code, "abstract interface")?;
    Some(ScopeStart {
        kind: SymbolKind::Interface,
        selection: "interface".to_string(),
        name: "interface".to_string(),
        args: Vec::new(),
        signature: code.trim().to_string(),
        attributes: Vec::new(),
        visibility: None,
        extends: None,
        is_abstract: true,
        is_module_procedure: false,
        ancestor: None,
        associate_names: Vec::new(),
    })
}

fn parse_generic_interface_scope(code: &str) -> Option<ScopeStart> {
    let rest = after_keyword(code, "interface")?.trim_start();
    let name = generic_name(rest)?;
    Some(ScopeStart {
        kind: SymbolKind::Interface,
        selection: name.clone(),
        name,
        args: Vec::new(),
        signature: code.trim().to_string(),
        attributes: Vec::new(),
        visibility: None,
        extends: None,
        is_abstract: false,
        is_module_procedure: false,
        ancestor: None,
        associate_names: Vec::new(),
    })
}

fn parse_construct_scope(code: &str) -> Option<ScopeStart> {
    let trimmed = code.trim();
    let construct = strip_construct_label(trimmed);
    let label = construct_label(trimmed);
    // `block data [name]` is a program unit, not a `block` construct.
    if let Some(rest) = after_keyword_words(construct, "block data") {
        let name = first_ident(rest)
            .map(str::to_string)
            .unwrap_or_else(|| "block data".to_string());
        return Some(ScopeStart {
            kind: SymbolKind::Program,
            selection: name.clone(),
            name,
            args: Vec::new(),
            signature: trimmed.to_string(),
            attributes: Vec::new(),
            visibility: None,
            extends: None,
            is_abstract: false,
            is_module_procedure: false,
            ancestor: None,
            associate_names: Vec::new(),
        });
    }
    if after_keyword(construct, "block").is_some() {
        return Some(construct_scope(
            SymbolKind::Block,
            label.unwrap_or("block"),
            label.unwrap_or("block"),
            trimmed,
            Vec::new(),
        ));
    }
    if after_keyword(construct, "associate").is_some() {
        return Some(construct_scope(
            SymbolKind::Associate,
            "associate",
            "associate",
            trimmed,
            associate_names(construct),
        ));
    }
    if after_keyword(construct, "select type").is_some()
        || after_keyword(construct, "select rank").is_some()
        || after_keyword(construct, "select case").is_some()
    {
        return Some(construct_scope(
            SymbolKind::SelectType,
            "select_type",
            first_ident(construct).unwrap_or("select"),
            trimmed,
            Vec::new(),
        ));
    }
    None
}

fn strip_construct_label(code: &str) -> &str {
    let Some((label, rest)) = code.split_once(':') else {
        return code;
    };
    let label = label.trim();
    if !label.is_empty() && first_ident(label).is_some_and(|name| name.eq_ignore_ascii_case(label))
    {
        rest.trim_start()
    } else {
        code
    }
}

fn construct_label(code: &str) -> Option<&str> {
    let (label, _) = code.split_once(':')?;
    let label = label.trim();
    (!label.is_empty() && first_ident(label).is_some_and(|name| name.eq_ignore_ascii_case(label)))
        .then_some(label)
}

fn construct_scope(
    kind: SymbolKind,
    prefix: &str,
    selection: &str,
    signature: &str,
    associate_names: Vec<String>,
) -> ScopeStart {
    ScopeStart {
        kind,
        name: prefix.to_string(),
        args: Vec::new(),
        signature: signature.to_string(),
        selection: selection.to_string(),
        attributes: Vec::new(),
        visibility: None,
        extends: None,
        is_abstract: false,
        is_module_procedure: false,
        ancestor: None,
        associate_names,
    }
}

fn associate_names(code: &str) -> Vec<String> {
    let Some(inner) = paren_content(code) else {
        return Vec::new();
    };
    split_top_level_commas(inner)
        .into_iter()
        .filter_map(|item| {
            let (lhs, _) = item.split_once("=>")?;
            first_ident(lhs.trim()).map(ToString::to_string)
        })
        .collect()
}

fn parse_submodule_definition(code: &str) -> Option<ScopeStart> {
    let rest = after_keyword(code, "submodule")?.trim_start();
    let ancestor = rest
        .strip_prefix('(')
        .and_then(|after| after.split_once(')'))
        .and_then(|(inner, _)| first_ident(inner))
        .map(ToString::to_string);
    let name_source = if let Some((_, after)) = rest
        .strip_prefix('(')
        .and_then(|after| after.split_once(')'))
    {
        after
    } else {
        rest
    };
    let name = first_ident(name_source)?.to_string();
    Some(ScopeStart {
        kind: SymbolKind::Submodule,
        selection: name.clone(),
        name,
        args: Vec::new(),
        signature: code.trim().to_string(),
        attributes: Vec::new(),
        visibility: None,
        extends: None,
        is_abstract: false,
        is_module_procedure: false,
        ancestor,
        associate_names: Vec::new(),
    })
}

fn is_derived_type_definition(code: &str) -> bool {
    let Some(rest) = after_keyword(code, "type") else {
        return false;
    };
    let rest = rest.trim_start();
    if first_ident(rest).is_some_and(|ident| ident.eq_ignore_ascii_case("is")) {
        return false;
    }
    !rest.starts_with('(') && (rest.contains("::") || first_ident(rest).is_some())
}

#[derive(Debug, Clone)]
struct TypeDefinition {
    name: String,
    attributes: Vec<String>,
    visibility: Option<Visibility>,
    extends: Option<String>,
    is_abstract: bool,
}

fn parse_type_definition(code: &str) -> Option<TypeDefinition> {
    let rest = after_keyword(code, "type")?.trim_start();
    let (lhs, rhs) = rest.split_once("::").unwrap_or(("", rest));
    let mut attributes = Vec::new();
    let mut visibility = None;
    let mut extends = None;
    let mut is_abstract = false;
    for attr in split_top_level_commas(lhs) {
        let attr = attr.trim();
        if attr.is_empty() {
            continue;
        }
        if attr.eq_ignore_ascii_case("public") {
            visibility = Some(Visibility::Public);
        } else if attr.eq_ignore_ascii_case("private") {
            visibility = Some(Visibility::Private);
        } else if attr.eq_ignore_ascii_case("abstract") {
            is_abstract = true;
        } else if attr.to_ascii_lowercase().starts_with("extends") {
            extends =
                paren_content(attr).and_then(|value| first_ident(value).map(ToString::to_string));
        }
        attributes.push(attr.to_string());
    }
    Some(TypeDefinition {
        name: first_ident(rhs)?.to_string(),
        attributes,
        visibility,
        extends,
        is_abstract,
    })
}

fn strip_procedure_prefixes(mut code: &str) -> &str {
    loop {
        let trimmed = code.trim_start();
        let Some(prefix) = first_ident(trimmed) else {
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

fn typed_function_rest(code: &str) -> Option<&str> {
    let trimmed = code.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    let idx = lower.find("function")?;
    if idx == 0 {
        return None;
    }
    let before = trimmed[..idx].trim();
    let after = &trimmed[idx + "function".len()..];
    if before.contains("::") {
        return None;
    }
    let prev = trimmed[..idx].chars().next_back()?;
    if !(prev.is_whitespace() || prev == ')') {
        return None;
    }
    if after
        .chars()
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphanumeric())
    {
        return None;
    }
    let type_name = first_ident(before)?.to_ascii_lowercase();
    matches!(
        type_name.as_str(),
        "integer" | "real" | "logical" | "complex" | "character" | "double" | "type" | "class"
    )
    .then_some(after)
}

fn typed_module_function_rest(code: &str) -> Option<&str> {
    let rest = after_keyword(code, "module")?;
    typed_function_rest(rest)
}

fn parse_use(code: &str, line: usize, file: &Path, scope: &[String]) -> Option<UseStmt> {
    let rest = after_keyword(code, "use")?;
    let mut rest = rest.trim_start();
    let after_comma = rest.strip_prefix(',').map(str::trim_start);
    let qualifier = after_comma.and_then(first_ident);
    let intrinsic = qualifier.is_some_and(|name| name.eq_ignore_ascii_case("intrinsic"));
    if qualifier.is_some_and(|name| {
        name.eq_ignore_ascii_case("intrinsic") || name.eq_ignore_ascii_case("non_intrinsic")
    }) {
        rest = rest.split_once("::").map(|(_, rhs)| rhs).unwrap_or(rest);
    }
    let module = first_ident(rest)?.to_string();
    let only_part = rest
        .to_ascii_lowercase()
        .find("only")
        .and_then(|idx| rest[idx..].split_once(':').map(|(_, rhs)| rhs));
    let only = only_part.map(split_use_only_names).unwrap_or_default();
    let renames = only_part.map(split_use_renames).unwrap_or_default();
    let col = find_ci(code, &module).unwrap_or(0);
    Some(UseStmt {
        module,
        only,
        renames,
        intrinsic,
        file: file.to_path_buf(),
        range: Range {
            start: Position::new(line, col),
            end: Position::new(line, col + rest.len()),
        },
        scope: scope.to_vec(),
    })
}

fn parse_import(code: &str, line: usize, file: &Path, scope: &[String]) -> Option<ImportStmt> {
    let rest = after_keyword(code, "import")?.trim_start();
    let (kind, names) = if rest.is_empty() {
        (ImportKind::All, Vec::new())
    } else if let Some(rest) = rest.strip_prefix(',') {
        let rest = rest.trim_start();
        if let Some(spec) = first_ident(rest) {
            if spec.eq_ignore_ascii_case("all") {
                (ImportKind::All, Vec::new())
            } else if spec.eq_ignore_ascii_case("none") {
                (ImportKind::None, Vec::new())
            } else if spec.eq_ignore_ascii_case("only") {
                let rhs = rest
                    .split_once(':')
                    .map(|(_, rhs)| rhs)
                    .unwrap_or("")
                    .trim_start();
                (ImportKind::Only, split_names(rhs))
            } else {
                (ImportKind::Only, split_names(rest))
            }
        } else {
            return None;
        }
    } else {
        let rhs = rest.strip_prefix("::").unwrap_or(rest).trim_start();
        (ImportKind::Only, split_names(rhs))
    };
    Some(ImportStmt {
        kind,
        names,
        file: file.to_path_buf(),
        range: Range {
            start: Position::new(line, 0),
            end: Position::new(line, code.len()),
        },
        scope: scope.to_vec(),
    })
}

fn parse_visibility(
    code: &str,
    line: usize,
    file: &Path,
    scope: &[String],
) -> Option<VisibilityStmt> {
    let trimmed = code.trim();
    let (visibility, rest) = if let Some(rest) = after_keyword(trimmed, "public") {
        (Visibility::Public, rest)
    } else if let Some(rest) = after_keyword(trimmed, "private") {
        (Visibility::Private, rest)
    } else {
        return None;
    };
    let rest = rest.trim_start();
    let names = if rest.is_empty() {
        Vec::new()
    } else {
        let rhs = rest.strip_prefix("::").unwrap_or(rest).trim_start();
        split_names(rhs)
    };
    Some(VisibilityStmt {
        visibility,
        names,
        file: file.to_path_buf(),
        range: Range {
            start: Position::new(line, 0),
            end: Position::new(line, code.len()),
        },
        scope: scope.to_vec(),
    })
}

fn parse_generic_binding(
    code: &str,
    line: usize,
    file: &Path,
    scope: &[String],
) -> Option<GenericBinding> {
    let rest = after_keyword(code, "generic")?.trim_start();
    let (lhs, rhs) = rest.split_once("::")?;
    let visibility = if lhs.to_ascii_lowercase().contains("private") {
        Visibility::Private
    } else if lhs.to_ascii_lowercase().contains("public") {
        Visibility::Public
    } else {
        Visibility::Default
    };
    let (name, procedures) = rhs.split_once("=>")?;
    let name = name.trim();
    let (name, kind) = generic_binding_name(name)?;
    let procedures = split_names(procedures);
    if procedures.is_empty() {
        return None;
    }
    Some(GenericBinding {
        name,
        kind,
        procedures,
        visibility,
        file: file.to_path_buf(),
        range: Range {
            start: Position::new(line, 0),
            end: Position::new(line, code.len()),
        },
        scope: scope.to_vec(),
    })
}

fn generic_binding_name(name: &str) -> Option<(String, GenericBindingKind)> {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("operator") {
        return Some((generic_name(name)?, GenericBindingKind::Operator));
    }
    if lower.starts_with("assignment") {
        return Some((generic_name(name)?, GenericBindingKind::Assignment));
    }
    Some((first_ident(name)?.to_string(), GenericBindingKind::Named))
}

fn generic_name(s: &str) -> Option<String> {
    let trimmed = s.trim();
    let lower = trimmed.to_ascii_lowercase();
    for keyword in ["operator", "assignment"] {
        if !lower.starts_with(keyword) {
            continue;
        }
        let open = trimmed.find('(')?;
        let close = trimmed[open + 1..].find(')')? + open + 1;
        return Some(format!("{}({})", keyword, trimmed[open + 1..close].trim()));
    }
    None
}

fn parse_include(code: &str, line: usize, file: &Path, scope: &[String]) -> Option<IncludeStmt> {
    let trimmed = code.trim_start();
    let path = if let Some(rest) = after_keyword(trimmed, "include") {
        quoted_path(rest)?
    } else if let Some(rest) = trimmed.strip_prefix("#include") {
        quoted_path(rest)?
    } else {
        return None;
    };
    let col = find_ci(code, &path).unwrap_or(0);
    Some(IncludeStmt {
        path,
        file: file.to_path_buf(),
        range: Range {
            start: Position::new(line, col),
            end: Position::new(line, col + code.len()),
        },
        scope: scope.to_vec(),
    })
}

fn parse_preprocessor(
    code: &str,
    line: usize,
    file: &Path,
    scope: &[String],
) -> Option<PreprocessorDirective> {
    let rest = code.trim_start().strip_prefix('#')?.trim_start();
    let keyword = first_ident(rest)?.to_ascii_lowercase();
    let after = rest[keyword.len()..].trim_start();
    let (kind, name, argument) = match keyword.as_str() {
        "if" => (
            PreprocessorKind::If,
            None,
            (!after.is_empty()).then(|| after.to_string()),
        ),
        "ifdef" => (
            PreprocessorKind::Ifdef,
            first_ident(after).map(ToString::to_string),
            None,
        ),
        "ifndef" => (
            PreprocessorKind::Ifndef,
            first_ident(after).map(ToString::to_string),
            None,
        ),
        "elif" => (
            PreprocessorKind::Elif,
            None,
            (!after.is_empty()).then(|| after.to_string()),
        ),
        "else" => (PreprocessorKind::Else, None, None),
        "endif" => (PreprocessorKind::Endif, None, None),
        "define" => {
            let name = first_ident(after).map(ToString::to_string);
            let argument = name
                .as_ref()
                .and_then(|name| after.get(name.len()..))
                .map(str::trim)
                .filter(|arg| !arg.is_empty())
                .map(ToString::to_string);
            (PreprocessorKind::Define, name, argument)
        }
        "undef" => (
            PreprocessorKind::Undef,
            first_ident(after).map(ToString::to_string),
            None,
        ),
        "include" => (PreprocessorKind::Include, None, quoted_path(after)),
        _ => return None,
    };
    Some(PreprocessorDirective {
        kind,
        name,
        argument,
        file: file.to_path_buf(),
        range: Range {
            start: Position::new(line, 0),
            end: Position::new(line, code.len()),
        },
        scope: scope.to_vec(),
    })
}

fn eval_preprocessor_expr(expr: &str, defs: &HashMap<String, String>) -> bool {
    eval_or(expr.trim(), defs)
}

fn macro_definition(argument: Option<&str>) -> MacroDefinition {
    let Some(argument) = argument else {
        return MacroDefinition::Object("1".to_string());
    };
    let argument = argument.trim();
    if let Some(rest) = argument.strip_prefix('(') {
        if let Some(close) = matching_paren_index(argument, 0) {
            let params = split_top_level_commas(&rest[..close - 1])
                .into_iter()
                .filter_map(|param| first_ident(param.trim()).map(ToString::to_string))
                .collect();
            let body = argument[close + 1..].trim().to_string();
            return MacroDefinition::Function { params, body };
        }
    }
    let value = if argument.is_empty() { "1" } else { argument };
    MacroDefinition::Object(value.to_string())
}

fn expand_macro_once(line: &str, macros: &HashMap<String, MacroDefinition>) -> String {
    let mut out = String::new();
    let mut idx = 0usize;
    while idx < line.len() {
        let rest = &line[idx..];
        let Some((name_start, name)) = next_identifier(rest) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..name_start]);
        let absolute_name_start = idx + name_start;
        let name_end = absolute_name_start + name.len();
        match macros.get(name) {
            Some(MacroDefinition::Function { params, body }) => {
                let after_name = &line[name_end..];
                let leading_ws = after_name.len() - after_name.trim_start().len();
                let call_start = name_end + leading_ws;
                if line[call_start..].starts_with('(') {
                    if let Some(close) = matching_paren_index(line, call_start) {
                        let args = split_top_level_commas(&line[call_start + 1..close]);
                        out.push_str(&expand_function_macro(params, body, &args));
                        idx = close + 1;
                        continue;
                    }
                }
                out.push_str(name);
                idx = name_end;
            }
            Some(MacroDefinition::Object(value)) => {
                out.push_str(value);
                idx = name_end;
            }
            None => {
                out.push_str(name);
                idx = name_end;
            }
        }
    }
    out
}

fn expand_function_macro(params: &[String], body: &str, args: &[String]) -> String {
    let mut expanded = body.to_string();
    for (param, arg) in params.iter().zip(args.iter()) {
        expanded = replace_identifier(&expanded, param, arg.trim());
    }
    expanded.replace("/**/", "")
}

fn replace_identifier(source: &str, ident: &str, replacement: &str) -> String {
    let mut out = String::new();
    let mut idx = 0usize;
    while idx < source.len() {
        let rest = &source[idx..];
        let Some((name_start, name)) = next_identifier(rest) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..name_start]);
        if name == ident {
            out.push_str(replacement);
        } else {
            out.push_str(name);
        }
        idx += name_start + name.len();
    }
    out
}

fn next_identifier(source: &str) -> Option<(usize, &str)> {
    let start = source.find(|ch: char| ch == '_' || ch.is_ascii_alphabetic())?;
    let tail = &source[start..];
    let end = tail
        .find(|ch: char| !(ch == '_' || ch.is_ascii_alphanumeric()))
        .unwrap_or(tail.len());
    Some((start, &tail[..end]))
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn matching_paren_index(source: &str, open_idx: usize) -> Option<usize> {
    if source.as_bytes().get(open_idx).copied()? != b'(' {
        return None;
    }
    let mut depth = 0usize;
    for (offset, ch) in source[open_idx..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open_idx + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn eval_or(expr: &str, defs: &HashMap<String, String>) -> bool {
    split_top_level_operator(expr, "||")
        .map(|parts| parts.into_iter().any(|part| eval_and(part, defs)))
        .unwrap_or_else(|| eval_and(expr, defs))
}

fn eval_and(expr: &str, defs: &HashMap<String, String>) -> bool {
    split_top_level_operator(expr, "&&")
        .map(|parts| parts.into_iter().all(|part| eval_not(part, defs)))
        .unwrap_or_else(|| eval_not(expr, defs))
}

fn eval_not(expr: &str, defs: &HashMap<String, String>) -> bool {
    let expr = expr.trim();
    if let Some(rest) = expr.strip_prefix('!') {
        !eval_not(rest, defs)
    } else {
        eval_comparison(expr, defs)
    }
}

fn eval_comparison(expr: &str, defs: &HashMap<String, String>) -> bool {
    let expr = strip_outer_parens(expr.trim());
    for op in ["<=", ">=", "!=", "==", "<", ">"] {
        if let Some((lhs, rhs)) = split_top_level_once(expr, op) {
            let lhs = eval_atom_value(lhs, defs);
            let rhs = eval_atom_value(rhs, defs);
            return match op {
                "<=" => lhs <= rhs,
                ">=" => lhs >= rhs,
                "!=" => lhs != rhs,
                "==" => lhs == rhs,
                "<" => lhs < rhs,
                ">" => lhs > rhs,
                _ => false,
            };
        }
    }
    eval_atom_bool(expr, defs)
}

fn eval_atom_bool(expr: &str, defs: &HashMap<String, String>) -> bool {
    eval_atom_value(expr, defs) != 0
}

fn eval_atom_value(expr: &str, defs: &HashMap<String, String>) -> i64 {
    let expr = strip_outer_parens(expr.trim());
    if let Some(name) = parse_defined(expr) {
        return i64::from(defs.contains_key(name));
    }
    if let Some(value) = parse_preprocessor_integer(expr) {
        return value;
    }
    if let Some(value) = defs.get(expr) {
        let value = value.trim();
        if value.is_empty() {
            return 1;
        }
        return eval_atom_value(value, defs);
    }
    eval_integer_expr(expr, defs)
}

fn eval_integer_expr(expr: &str, defs: &HashMap<String, String>) -> i64 {
    let expr = strip_outer_parens(expr.trim());
    for op in ["|", "^", "&", "<<", ">>", "+", "-", "*", "/", "%"] {
        if let Some(parts) = split_top_level_integer_operator(expr, op) {
            let mut values = parts.into_iter().map(|part| eval_atom_value(part, defs));
            let Some(first) = values.next() else {
                return 0;
            };
            return values.fold(first, |lhs, rhs| match op {
                "|" => lhs | rhs,
                "^" => lhs ^ rhs,
                "&" => lhs & rhs,
                "<<" => lhs.checked_shl(rhs.max(0) as u32).unwrap_or(0),
                ">>" => lhs.checked_shr(rhs.max(0) as u32).unwrap_or(0),
                "+" => lhs + rhs,
                "-" => lhs - rhs,
                "*" => lhs * rhs,
                "/" => {
                    if rhs == 0 {
                        0
                    } else {
                        lhs / rhs
                    }
                }
                "%" => {
                    if rhs == 0 {
                        0
                    } else {
                        lhs % rhs
                    }
                }
                _ => 0,
            });
        }
    }
    if let Some(rest) = expr.strip_prefix('+') {
        return eval_atom_value(rest, defs);
    }
    if let Some(rest) = expr.strip_prefix('-') {
        return -eval_atom_value(rest, defs);
    }
    if let Some(rest) = expr.strip_prefix('~') {
        return !eval_atom_value(rest, defs);
    }
    0
}

fn parse_preprocessor_integer(expr: &str) -> Option<i64> {
    let trimmed = expr.trim();
    if let Some(value) = parse_preprocessor_char_literal(trimmed) {
        return Some(value);
    }
    let number = trimmed
        .trim_end_matches(|ch: char| matches!(ch, 'u' | 'U' | 'l' | 'L'))
        .replace('\'', "");
    if let Some(hex) = number
        .strip_prefix("0x")
        .or_else(|| number.strip_prefix("0X"))
    {
        i64::from_str_radix(hex, 16).ok()
    } else if let Some(binary) = number
        .strip_prefix("0b")
        .or_else(|| number.strip_prefix("0B"))
    {
        i64::from_str_radix(binary, 2).ok()
    } else if number.len() > 1 && number.starts_with('0') {
        i64::from_str_radix(&number[1..], 8).ok()
    } else {
        number.parse::<i64>().ok()
    }
}

fn parse_preprocessor_char_literal(expr: &str) -> Option<i64> {
    let inner = expr.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut chars = inner.chars();
    let ch = match chars.next()? {
        '\\' => match chars.next()? {
            '0' => '\0',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '\\' => '\\',
            '\'' => '\'',
            '"' => '"',
            other => other,
        },
        ch => ch,
    };
    chars.next().is_none().then_some(ch as i64)
}

fn parse_defined(expr: &str) -> Option<&str> {
    let rest = expr.trim().strip_prefix("defined")?.trim_start();
    if let Some(inner) = rest.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        first_ident(inner.trim())
    } else {
        first_ident(rest)
    }
}

fn strip_outer_parens(mut expr: &str) -> &str {
    loop {
        let trimmed = expr.trim();
        if !(trimmed.starts_with('(') && trimmed.ends_with(')')) {
            return trimmed;
        }
        let inner = &trimmed[1..trimmed.len() - 1];
        if parens_balanced(inner) {
            expr = inner;
        } else {
            return trimmed;
        }
    }
}

fn split_top_level_operator<'a>(expr: &'a str, op: &str) -> Option<Vec<&'a str>> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let bytes = expr.as_bytes();
    let op_bytes = op.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        match bytes[idx] as char {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && bytes[idx..].starts_with(op_bytes) {
            parts.push(expr[start..idx].trim());
            idx += op_bytes.len();
            start = idx;
            continue;
        }
        idx += 1;
    }
    if parts.is_empty() {
        None
    } else {
        parts.push(expr[start..].trim());
        Some(parts)
    }
}

fn split_top_level_integer_operator<'a>(expr: &'a str, op: &str) -> Option<Vec<&'a str>> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let bytes = expr.as_bytes();
    let op_bytes = op.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        match bytes[idx] as char {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0
            && bytes[idx..].starts_with(op_bytes)
            && !integer_operator_is_logical_neighbor(bytes, idx, op)
            && !integer_operator_is_unary(expr, idx, op)
        {
            parts.push(expr[start..idx].trim());
            idx += op_bytes.len();
            start = idx;
            continue;
        }
        idx += 1;
    }
    if parts.is_empty() {
        None
    } else {
        parts.push(expr[start..].trim());
        Some(parts)
    }
}

fn integer_operator_is_logical_neighbor(bytes: &[u8], idx: usize, op: &str) -> bool {
    matches!(op, "|" | "&")
        && (bytes.get(idx + 1) == Some(&bytes[idx])
            || idx.checked_sub(1).and_then(|prev| bytes.get(prev)) == Some(&bytes[idx]))
}

fn integer_operator_is_unary(expr: &str, idx: usize, op: &str) -> bool {
    if !matches!(op, "+" | "-") {
        return false;
    }
    let before = expr[..idx].trim_end();
    before.is_empty()
        || before
            .chars()
            .last()
            .is_some_and(|ch| matches!(ch, '(' | '+' | '-' | '*' | '/' | '%' | '&' | '|' | '^'))
}

fn split_top_level_once<'a>(expr: &'a str, op: &str) -> Option<(&'a str, &'a str)> {
    split_top_level_operator(expr, op).and_then(|parts| {
        if parts.len() == 2 {
            Some((parts[0], parts[1]))
        } else {
            None
        }
    })
}

fn parens_balanced(expr: &str) -> bool {
    let mut depth = 0usize;
    for ch in expr.chars() {
        match ch {
            '(' => depth += 1,
            ')' => {
                let Some(next) = depth.checked_sub(1) else {
                    return false;
                };
                depth = next;
            }
            _ => {}
        }
    }
    depth == 0
}

fn parse_variables(
    code: &str,
    line: usize,
    file: &Path,
    scope: &[String],
    in_type_binding_part: bool,
) -> Option<Vec<Symbol>> {
    let (lhs, rhs) = code
        .split_once("::")
        .or_else(|| split_legacy_declaration(code))?;
    let decl = parse_declaration_lhs(lhs.trim())?;
    if !matches!(
        decl.type_keyword.as_str(),
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
            | "external"
    ) {
        return None;
    }
    let mut symbols = Vec::new();
    for item in parse_decl_items(rhs) {
        let col = find_ci(code, &item.name).unwrap_or(0);
        symbols.push(Symbol {
            name: item.name.clone(),
            kind: if decl.type_keyword == "procedure" && in_type_binding_part {
                SymbolKind::Method
            } else {
                SymbolKind::Variable
            },
            file: file.to_path_buf(),
            range: Range {
                start: Position::new(line, col),
                end: Position::new(line, col + item.name.len()),
            },
            selection_range: Range {
                start: Position::new(line, col),
                end: Position::new(line, col + item.name.len()),
            },
            scope: scope.to_vec(),
            signature: format!("{} :: {}", lhs.trim(), item.name),
            args: Vec::new(),
            documentation: None,
            visibility: decl.visibility.unwrap_or(Visibility::Default),
            type_spec: Some(decl.type_spec.clone()),
            attributes: declaration_attributes(&decl, &item),
            result: None,
            is_parameter: decl.has_attribute("parameter"),
            is_external: decl.has_attribute("external") || decl.type_keyword == "external",
            extends: None,
            is_abstract: false,
            binding_target: item.binding_target,
            pass_arg: decl.pass_arg(),
            is_deferred: decl.has_attribute("deferred"),
            is_module_procedure: false,
            ancestor: None,
        });
    }
    Some(symbols)
}

/// A bare symbol for legacy statement constructs (`entry`, `common`,
/// `namelist`): name + location + signature, everything else defaulted.
fn legacy_symbol(
    name: &str,
    kind: SymbolKind,
    code: &str,
    line: usize,
    file: &Path,
    scope: &[String],
    signature: String,
) -> Symbol {
    let col = find_ci(code, name).unwrap_or(0);
    let range = Range {
        start: Position::new(line, col),
        end: Position::new(line, col + name.len()),
    };
    Symbol {
        name: name.to_string(),
        kind,
        file: file.to_path_buf(),
        range: range.clone(),
        selection_range: range,
        scope: scope.to_vec(),
        signature,
        args: Vec::new(),
        documentation: None,
        visibility: Visibility::Default,
        type_spec: None,
        attributes: Vec::new(),
        result: None,
        is_parameter: false,
        is_external: false,
        extends: None,
        is_abstract: false,
        binding_target: None,
        pass_arg: None,
        is_deferred: false,
        is_module_procedure: false,
        ancestor: None,
    }
}

/// `common [/blk/] a, b(10) [[,] /blk2/ c]` — every listed object is a
/// variable of the enclosing scope. Returned as pending symbols: most legacy
/// code also declares the members with explicit types, which must win.
fn parse_common(code: &str, line: usize, file: &Path, scope: &[String]) -> Option<Vec<Symbol>> {
    let rest = after_keyword(code, "common")?;
    let mut symbols = Vec::new();
    for (block, items) in split_slash_delimited_groups(rest) {
        // The block name itself is queryable (hover/workspace-symbol on
        // `/setup/`). COMMON names live in their own namespace, so this is
        // pending too — an unrelated variable with the same name wins.
        if let Some(name) = block.as_deref().filter(|b| !b.is_empty()) {
            symbols.push(legacy_symbol(
                name,
                SymbolKind::Variable,
                code,
                line,
                file,
                scope,
                format!("common /{name}/"),
            ));
        }
        let block_label = block
            .as_deref()
            .filter(|b| !b.is_empty())
            .map(|b| format!(" /{b}/"))
            .unwrap_or_default();
        for item in parse_decl_items(&items) {
            symbols.push(legacy_symbol(
                &item.name,
                SymbolKind::Variable,
                code,
                line,
                file,
                scope,
                format!("common{block_label} :: {}", item.name),
            ));
        }
    }
    (!symbols.is_empty()).then_some(symbols)
}

/// `equivalence (a, b(1)) [, (c, d)]` — every listed object may be implicitly
/// typed. Keep these pending so explicit declarations on nearby lines win.
fn parse_equivalence(
    code: &str,
    line: usize,
    file: &Path,
    scope: &[String],
) -> Option<Vec<Symbol>> {
    let rest = after_keyword(code, "equivalence")?;
    let mut symbols = Vec::new();
    for group in parenthesized_groups(rest) {
        for item in parse_decl_items(group) {
            symbols.push(legacy_symbol(
                &item.name,
                SymbolKind::Variable,
                code,
                line,
                file,
                scope,
                format!("equivalence :: {}", item.name),
            ));
        }
    }
    (!symbols.is_empty()).then_some(symbols)
}

fn parenthesized_groups(s: &str) -> Vec<&str> {
    let mut groups = Vec::new();
    let mut depth = 0usize;
    let mut start = None;
    for (idx, ch) in s.char_indices() {
        match ch {
            '(' => {
                if depth == 0 {
                    start = Some(idx + ch.len_utf8());
                }
                depth += 1;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some(start_idx) = start.take() {
                        groups.push(&s[start_idx..idx]);
                    }
                }
            }
            _ => {}
        }
    }
    groups
}

/// `namelist /group/ a, b [[,] /group2/ c]` — each group name becomes a
/// symbol (the members are ordinary variables declared elsewhere). Pending:
/// repeated NAMELIST statements legally extend the same group.
fn parse_namelist(code: &str, line: usize, file: &Path, scope: &[String]) -> Option<Vec<Symbol>> {
    let rest = after_keyword(code, "namelist")?;
    let mut symbols = Vec::new();
    for (block, _) in split_slash_delimited_groups(rest) {
        if let Some(group) = block.as_deref().filter(|b| !b.is_empty()) {
            symbols.push(legacy_symbol(
                group,
                SymbolKind::Variable,
                code,
                line,
                file,
                scope,
                format!("namelist /{group}/"),
            ));
        }
    }
    (!symbols.is_empty()).then_some(symbols)
}

/// Split `[/name/] items [[,] /name2/ items]...` into `(block, items)` pairs.
/// The leading block name is `None` for blank COMMON.
fn split_slash_delimited_groups(rest: &str) -> Vec<(Option<String>, String)> {
    let mut groups = Vec::new();
    let mut current_block: Option<String> = None;
    let mut current_items = String::new();
    let mut chars = rest.char_indices().peekable();
    let mut depth = 0usize;
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '(' => {
                depth += 1;
                current_items.push(ch);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current_items.push(ch);
            }
            '/' if depth == 0 => {
                // Close the running group, then read the next block name.
                if !current_items.trim().is_empty() || current_block.is_some() {
                    groups.push((current_block.take(), std::mem::take(&mut current_items)));
                }
                let tail = &rest[idx + 1..];
                let Some(end) = tail.find('/') else {
                    return groups; // unbalanced — bail with what we have
                };
                current_block = Some(tail[..end].trim().to_string());
                current_items.clear();
                // Skip up to and including the closing '/'.
                for _ in 0..=end {
                    chars.next();
                }
            }
            _ => current_items.push(ch),
        }
    }
    if !current_items.trim().is_empty() || current_block.is_some() {
        groups.push((current_block, current_items));
    }
    groups
}

fn parse_enumerators(code: &str, line: usize, file: &Path, scope: &[String]) -> Vec<Symbol> {
    let Some(rest) = after_keyword(code, "enumerator") else {
        return Vec::new();
    };
    let rhs = rest
        .split_once("::")
        .map(|(_, rhs)| rhs)
        .unwrap_or(rest)
        .trim();
    parse_decl_items(rhs)
        .into_iter()
        .map(|item| {
            let col = find_ci(code, &item.name).unwrap_or(0);
            Symbol {
                name: item.name.clone(),
                kind: SymbolKind::Variable,
                file: file.to_path_buf(),
                range: Range {
                    start: Position::new(line, col),
                    end: Position::new(line, col + item.name.len()),
                },
                selection_range: Range {
                    start: Position::new(line, col),
                    end: Position::new(line, col + item.name.len()),
                },
                scope: scope.to_vec(),
                signature: format!("enumerator :: {}", item.name),
                args: Vec::new(),
                documentation: None,
                visibility: Visibility::Default,
                type_spec: Some("integer".to_string()),
                attributes: vec!["parameter".to_string()],
                result: None,
                is_parameter: true,
                is_external: false,
                extends: None,
                is_abstract: false,
                binding_target: None,
                pass_arg: None,
                is_deferred: false,
                is_module_procedure: false,
                ancestor: None,
            }
        })
        .collect()
}

fn split_legacy_declaration(code: &str) -> Option<(&str, &str)> {
    let trimmed = code.trim_start();
    let leading_ws = code.len() - trimmed.len();
    let keyword = first_ident(trimmed)?.to_ascii_lowercase();
    if !matches!(
        keyword.as_str(),
        "integer" | "real" | "double" | "complex" | "character" | "logical" | "type" | "class"
    ) {
        return None;
    }

    let mut end = keyword.len();
    if keyword == "double" {
        let rest = trimmed.get(end..)?.trim_start();
        if let Some(precision) = first_ident(rest) {
            if precision.eq_ignore_ascii_case("precision") {
                end = trimmed.len() - rest.len() + precision.len();
            }
        }
    }
    let rest = trimmed.get(end..)?;
    let rest = rest.trim_start();
    if matches!(keyword.as_str(), "type" | "class")
        && first_ident(rest).is_some_and(|ident| {
            ident.eq_ignore_ascii_case("is") || ident.eq_ignore_ascii_case("default")
        })
    {
        return None;
    }
    if rest.starts_with('(') {
        let close = matching_paren_end(rest)?;
        end = trimmed.len() - rest.len() + close;
    }
    let rhs = trimmed.get(end..)?.trim_start();
    if rhs.is_empty() || rhs.starts_with(',') {
        return None;
    }
    let lhs_end = leading_ws + end;
    Some((&code[..lhs_end], rhs))
}

fn matching_paren_end(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, ch) in text.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(idx + ch.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Debug, Clone)]
struct DeclarationLhs {
    type_keyword: String,
    type_spec: String,
    attributes: Vec<String>,
    visibility: Option<Visibility>,
}

impl DeclarationLhs {
    fn has_attribute(&self, name: &str) -> bool {
        self.attributes
            .iter()
            .any(|attr| attr.eq_ignore_ascii_case(name))
    }

    fn pass_arg(&self) -> Option<String> {
        self.attributes
            .iter()
            .find(|attr| attr.to_ascii_lowercase().starts_with("pass"))
            .and_then(|attr| paren_content(attr))
            .and_then(|value| first_ident(value).map(ToString::to_string))
    }
}

fn parse_declaration_lhs(lhs: &str) -> Option<DeclarationLhs> {
    let mut parts = split_top_level_commas(lhs);
    let first = parts.first()?.trim().to_string();
    let type_keyword = first_ident(&first)?.to_ascii_lowercase();
    parts.remove(0);
    let mut attributes = Vec::new();
    let mut visibility = None;
    for attr in parts {
        let attr = attr.trim();
        if attr.is_empty() {
            continue;
        }
        if attr.eq_ignore_ascii_case("public") {
            visibility = Some(Visibility::Public);
        } else if attr.eq_ignore_ascii_case("private") {
            visibility = Some(Visibility::Private);
        }
        attributes.push(attr.to_string());
    }
    Some(DeclarationLhs {
        type_keyword,
        type_spec: first,
        attributes,
        visibility,
    })
}

fn default_visibility_for_scope(kind: SymbolKind) -> Visibility {
    match kind {
        SymbolKind::Module => Visibility::Public,
        _ => Visibility::Default,
    }
}

fn parse_result_name(signature: &str) -> Option<String> {
    let lower = signature.to_ascii_lowercase();
    let idx = lower.find("result")?;
    let after = &signature[idx + "result".len()..];
    let start = after.find('(')? + 1;
    let end = after[start..].find(')')? + start;
    first_ident(&after[start..end]).map(ToString::to_string)
}

#[derive(Debug, Clone)]
struct DeclItem {
    name: String,
    binding_target: Option<String>,
    has_shape: bool,
}

fn parse_decl_items(rhs: &str) -> Vec<DeclItem> {
    split_top_level_commas(rhs)
        .into_iter()
        .filter_map(|item| {
            let item = item.trim().to_string();
            let (name_part, binding_target) = if let Some((lhs, rhs)) = item.split_once("=>") {
                (lhs.trim(), first_ident(rhs).map(ToString::to_string))
            } else {
                (item.as_str(), None)
            };
            let has_shape = name_part
                .trim_start()
                .get(first_ident(name_part)?.len()..)
                .is_some_and(|rest| rest.trim_start().starts_with('('));
            Some(DeclItem {
                name: first_ident(name_part)?.to_string(),
                binding_target,
                has_shape,
            })
        })
        .collect()
}

fn parse_statement_function_lhs(code: &str) -> Option<(String, Vec<String>)> {
    let (lhs, _) = split_top_level_once(code, "=")?;
    let lhs = lhs.trim();
    let name = first_ident(lhs)?;
    let name_start = lhs.find(name)?;
    if !lhs[..name_start]
        .trim()
        .chars()
        .all(|ch| ch.is_ascii_digit())
    {
        return None;
    }
    let rest = lhs[name_start + name.len()..].trim_start();
    if !rest.starts_with('(') {
        return None;
    }
    let close = matching_paren_index(rest, 0)?;
    if !rest[close + 1..].trim().is_empty() {
        return None;
    }
    let args = split_names(&rest[1..close]);
    if args
        .iter()
        .all(|arg| first_ident(arg).is_some_and(|ident| ident == arg))
    {
        Some((name.to_string(), args))
    } else {
        None
    }
}

fn declaration_attributes(decl: &DeclarationLhs, item: &DeclItem) -> Vec<String> {
    let mut attributes = decl.attributes.clone();
    if item.has_shape
        && !attributes
            .iter()
            .any(|attr| attr.eq_ignore_ascii_case("dimension"))
    {
        attributes.push("dimension".to_string());
    }
    attributes
}

fn paren_content(s: &str) -> Option<&str> {
    let start = s.find('(')? + 1;
    let end = s[start..].find(')')? + start;
    Some(&s[start..end])
}

fn quoted_path(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
    let quote = trimmed.chars().next()?;
    if quote != '\'' && quote != '"' && quote != '<' {
        return None;
    }
    let end_quote = if quote == '<' { '>' } else { quote };
    let start = quote.len_utf8();
    let end = trimmed[start..].find(end_quote)? + start;
    Some(trimmed[start..end].to_string())
}

fn parse_doc_comment(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    for marker in ["!>", "!!"] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return Some(rest.trim_start().to_string());
        }
    }
    None
}

fn split_names(s: &str) -> Vec<String> {
    split_top_level_commas(s)
        .into_iter()
        .filter_map(|item| use_or_visibility_name(item.trim()))
        .collect()
}

fn split_use_only_names(s: &str) -> Vec<String> {
    split_top_level_commas(s)
        .into_iter()
        .filter_map(|item| {
            let item = item.trim();
            let local = item.split_once("=>").map(|(lhs, _)| lhs).unwrap_or(item);
            use_or_visibility_name(local)
        })
        .collect()
}

fn split_use_renames(s: &str) -> Vec<UseRename> {
    split_top_level_commas(s)
        .into_iter()
        .filter_map(|item| {
            let (local, remote) = item.trim().split_once("=>")?;
            Some(UseRename {
                local: use_or_visibility_name(local)?,
                remote: use_or_visibility_name(remote)?,
            })
        })
        .collect()
}

fn use_or_visibility_name(item: &str) -> Option<String> {
    let item = item.trim().trim_start_matches('&').trim_start();
    for keyword in ["operator", "assignment"] {
        if item
            .get(..keyword.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(keyword))
        {
            let rest = item[keyword.len()..].trim_start();
            if rest.starts_with('(') {
                let close = matching_paren_index(rest, 0)?;
                return Some(format!(
                    "{}{}",
                    keyword,
                    rest.get(..=close)?.split_whitespace().collect::<String>()
                ));
            }
        }
    }
    first_ident(item).map(ToString::to_string)
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

fn after_keyword_words<'a>(code: &'a str, keyword: &str) -> Option<&'a str> {
    let words: Vec<_> = keyword.split_whitespace().collect();
    let mut rest = code.trim_start();
    for (idx, word) in words.iter().enumerate() {
        let prefix = rest.get(..word.len())?;
        if !prefix.eq_ignore_ascii_case(word) {
            return None;
        }
        rest = &rest[word.len()..];
        if rest
            .chars()
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        {
            return None;
        }
        if idx + 1 < words.len() {
            let trimmed = rest.trim_start();
            if trimmed.len() == rest.len() {
                return None;
            }
            rest = trimmed;
        }
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
        "block" => Some(SymbolKind::Block),
        "associate" => Some(SymbolKind::Associate),
        "select" => Some(SymbolKind::SelectType),
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

pub(crate) fn word_at_source(source: &str, pos: Position) -> Option<String> {
    let line = source.lines().nth(pos.line)?;
    word_at_line(line, pos.character)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberAccess {
    pub(crate) receiver: String,
    pub(crate) member: String,
}

pub(crate) fn member_access_at_source(source: &str, pos: Position) -> Option<MemberAccess> {
    let line = source.lines().nth(pos.line)?;
    let member = word_at_line(line, pos.character)?;
    let byte_character = byte_idx_for_utf16_col(line, pos.character);
    let mut search_start = 0;
    while let Some(relative_start) = line.get(search_start..)?.find(&member) {
        let member_start = search_start + relative_start;
        let member_end = member_start + member.len();
        let before_boundary = member_start == 0
            || !line
                .get(..member_start)?
                .chars()
                .next_back()
                .is_some_and(is_ident);
        let after_boundary = member_end == line.len()
            || !line.get(member_end..)?.chars().next().is_some_and(is_ident);
        if before_boundary
            && after_boundary
            && member_start <= byte_character
            && byte_character <= member_end
        {
            return member_access_before(line, member_start, &member);
        }
        search_start = member_end;
    }
    let member_start = line
        .get(..byte_character.min(line.len()))?
        .rfind(&member)
        .filter(|idx| line.get(*idx..).is_some_and(|s| s.starts_with(&member)))?;
    member_access_before(line, member_start, &member)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallContext {
    pub(crate) name: String,
    pub(crate) receiver: Option<String>,
    pub(crate) active_parameter: usize,
    pub(crate) active_argument_name: Option<String>,
    pub(crate) argument_count: usize,
}

pub(crate) fn call_context(source: &str, pos: Position) -> Option<CallContext> {
    let prefix = source_prefix(source, pos)?;
    let open = find_unclosed_call_paren(&prefix)?;
    let before = prefix[..open].trim_end();
    let name_end = before.len();
    let name_start = before
        .char_indices()
        .rev()
        .find(|(_, ch)| !is_ident(*ch))
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    let name = before[name_start..name_end].trim();
    if name.is_empty() || name.eq_ignore_ascii_case("call") {
        return None;
    }
    let receiver = member_access_before(before, name_start, name).map(|access| access.receiver);
    Some(CallContext {
        name: name.to_string(),
        receiver,
        active_parameter: count_top_level_commas(&prefix[open + 1..]),
        active_argument_name: active_keyword_argument(&prefix[open + 1..]),
        argument_count: count_call_arguments(&prefix[open + 1..]),
    })
}

fn member_access_before(line: &str, member_start: usize, member: &str) -> Option<MemberAccess> {
    let before = line.get(..member_start)?.trim_end();
    let sep = before.chars().last()?;
    if sep != '%' && sep != '.' {
        return None;
    }
    let receiver_end = before.len() - sep.len_utf8();
    let receiver_prefix = before.get(..receiver_end)?.trim_end();
    let receiver_start = receiver_prefix
        .char_indices()
        .rev()
        .find(|(_, ch)| !is_ident(*ch))
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    let receiver = receiver_prefix.get(receiver_start..)?.trim();
    if receiver.is_empty() {
        return None;
    }
    Some(MemberAccess {
        receiver: receiver.to_string(),
        member: member.to_string(),
    })
}

fn source_prefix(source: &str, pos: Position) -> Option<String> {
    let mut out = String::new();
    for (line_no, line) in source.lines().enumerate() {
        if line_no > pos.line {
            break;
        }
        if line_no == pos.line {
            let byte_character = byte_idx_for_utf16_col(line, pos.character);
            out.push_str(line.get(..byte_character.min(line.len()))?);
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

fn find_unclosed_call_paren(prefix: &str) -> Option<usize> {
    let mut stack = Vec::new();
    let mut single = false;
    let mut double = false;
    for (idx, ch) in prefix.char_indices() {
        match ch {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '(' if !single && !double => stack.push(idx),
            ')' if !single && !double => {
                stack.pop();
            }
            _ => {}
        }
    }
    stack.pop()
}

fn count_top_level_commas(s: &str) -> usize {
    let mut depth = 0usize;
    let mut count = 0usize;
    let mut single = false;
    let mut double = false;
    for ch in s.chars() {
        match ch {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '(' if !single && !double => depth += 1,
            ')' if !single && !double => depth = depth.saturating_sub(1),
            ',' if !single && !double && depth == 0 => count += 1,
            _ => {}
        }
    }
    count
}

fn count_call_arguments(s: &str) -> usize {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return 0;
    }
    count_top_level_commas(s) + 1
}

fn active_keyword_argument(s: &str) -> Option<String> {
    let active_arg = split_top_level_commas(s).pop()?;
    let eq = top_level_equals(&active_arg)?;
    let name = active_arg[..eq].trim();
    if is_fortran_name(name) {
        Some(name.to_ascii_lowercase())
    } else {
        None
    }
}

fn top_level_equals(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut single = false;
    let mut double = false;
    for (idx, ch) in s.char_indices() {
        match ch {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '(' if !single && !double => depth += 1,
            ')' if !single && !double => depth = depth.saturating_sub(1),
            '=' if !single && !double && depth == 0 => return Some(idx),
            _ => {}
        }
    }
    None
}

fn is_fortran_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic() && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut single = false;
    let mut double = false;
    for (idx, ch) in s.char_indices() {
        match ch {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '(' if !single && !double => paren_depth += 1,
            ')' if !single && !double => paren_depth = paren_depth.saturating_sub(1),
            '[' if !single && !double => bracket_depth += 1,
            ']' if !single && !double => bracket_depth = bracket_depth.saturating_sub(1),
            ',' if !single && !double && paren_depth == 0 && bracket_depth == 0 => {
                parts.push(s[start..idx].to_string());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(s[start..].to_string());
    parts
}

fn word_at_line(line: &str, character: usize) -> Option<String> {
    word_at_line_with_range(line, character).map(|(word, _)| word)
}

fn word_range_at_line(line: &str, character: usize) -> Option<Range> {
    word_at_line_with_range(line, character).map(|(_, range)| range)
}

fn word_at_line_with_range(line: &str, character: usize) -> Option<(String, Range)> {
    let bytes = line.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut idx = byte_idx_for_utf16_col(line, character).min(bytes.len().saturating_sub(1));
    while idx > 0 && !line.is_char_boundary(idx) {
        idx -= 1;
    }
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
    Some((
        line[start..end].to_string(),
        Range {
            start: Position::new(0, utf16_col(line, start)),
            end: Position::new(0, utf16_col(line, end)),
        },
    ))
}

pub(crate) fn identifier_occurrences(source: &str, name: &str) -> Vec<Range> {
    let wanted = name.to_ascii_lowercase();
    let mut out = Vec::new();
    for (line_no, line) in source.lines().enumerate() {
        let code = strip_inline_comment(line);
        let bytes = code.as_bytes();
        let mut idx = 0;
        while idx < bytes.len() {
            if !is_ident(bytes[idx] as char) {
                idx += 1;
                continue;
            }
            let start = idx;
            idx += 1;
            while idx < bytes.len() && is_ident(bytes[idx] as char) {
                idx += 1;
            }
            if code[start..idx].eq_ignore_ascii_case(&wanted) {
                out.push(Range {
                    start: Position::new(line_no, utf16_col(&code, start)),
                    end: Position::new(line_no, utf16_col(&code, idx)),
                });
            }
        }
    }
    out
}

pub(crate) fn word_range_at_source(source: &str, pos: Position) -> Option<Range> {
    let line = source.lines().nth(pos.line)?;
    let mut range = word_range_at_line(line, pos.character)?;
    range.start.line = pos.line;
    range.end.line = pos.line;
    Some(range)
}

fn line_end(source: &str, line: usize) -> Position {
    Position::new(
        line,
        source.lines().nth(line).map(utf16_len).unwrap_or_default(),
    )
}

fn source_end(source: &str) -> Position {
    let mut line_count: usize = 0;
    let mut last_len = 0;
    for line in source.lines() {
        line_count += 1;
        last_len = utf16_len(line);
    }
    Position::new(line_count.saturating_sub(1), last_len)
}

fn split_statement_line(line: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut start = 0usize;
    let mut single = false;
    let mut double = false;
    for (idx, ch) in line.char_indices() {
        match ch {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            ';' if !single && !double => {
                push_statement_segment(line, start, idx, &mut statements);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    push_statement_segment(line, start, line.len(), &mut statements);
    statements
}

fn push_statement_segment(line: &str, start: usize, end: usize, statements: &mut Vec<String>) {
    let Some(segment) = line.get(start..end) else {
        return;
    };
    if segment.trim().is_empty() {
        return;
    }
    let mut padded = " ".repeat(start);
    padded.push_str(segment.trim_end());
    statements.push(padded);
}

pub(crate) fn is_scope_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Module
            | SymbolKind::Program
            | SymbolKind::Submodule
            | SymbolKind::Subroutine
            | SymbolKind::Function
            | SymbolKind::Type
            | SymbolKind::Interface
            | SymbolKind::Block
            | SymbolKind::Associate
            | SymbolKind::SelectType
    )
}

fn is_construct_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Block | SymbolKind::Associate | SymbolKind::SelectType
    )
}

pub(crate) fn scope_match_len(current: &[String], candidate: &[String]) -> usize {
    current
        .iter()
        .zip(candidate)
        .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
        .count()
}

fn is_ident(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .to_ascii_lowercase()
        .find(&needle.to_ascii_lowercase())
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn utf16_col(line: &str, byte_idx: usize) -> usize {
    let mut idx = byte_idx.min(line.len());
    while idx > 0 && !line.is_char_boundary(idx) {
        idx -= 1;
    }
    line[..idx].encode_utf16().count()
}

fn byte_idx_for_utf16_col(line: &str, character: usize) -> usize {
    let mut utf16 = 0;
    for (idx, ch) in line.char_indices() {
        if utf16 >= character {
            return idx;
        }
        let next = utf16 + ch.len_utf16();
        if next > character {
            return idx;
        }
        utf16 = next;
    }
    line.len()
}
