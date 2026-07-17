use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use fortran_lsp::{
    CodeAction, CompletionItem, DiagnosticSeverity, DocumentSymbol, InlayHint, Location, Position,
    Range, RenameError, SelectionRange, SignatureHelp, Symbol, SymbolKind, TextEdit, Workspace,
};
use serde_json::{json, Value};

fn main() -> io::Result<()> {
    if std::env::args().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(());
    }
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("fortran-lsp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    Server::new().run()
}

fn print_help() {
    println!(
        "fortran-lsp {}\n\nNative Fortran language server over stdio.\n\nUsage:\n  fortran-lsp\n\nEditor clients should launch this process and speak LSP over stdio.",
        env!("CARGO_PKG_VERSION")
    );
}

struct Server {
    workspace: Workspace,
    open_sources: HashMap<PathBuf, String>,
    shutdown_requested: bool,
}

impl Server {
    fn new() -> Self {
        Self {
            workspace: Workspace::new(),
            open_sources: HashMap::new(),
            shutdown_requested: false,
        }
    }

    fn run(&mut self) -> io::Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut reader = io::BufReader::new(stdin.lock());
        let mut writer = stdout.lock();

        while let Some(msg) = read_lsp_message(&mut reader)? {
            if self.handle_message(&mut writer, msg)? {
                break;
            }
        }
        Ok(())
    }

    fn handle_message<W: Write>(&mut self, writer: &mut W, msg: Value) -> io::Result<bool> {
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            return Ok(false);
        };
        let id = msg.get("id").cloned();
        match method {
            "initialize" => {
                self.configure_from_initialize(&msg);
                if let Some(id) = id {
                    write_response(writer, id, initialize_result())?;
                }
            }
            "initialized" => {}
            "shutdown" => {
                self.shutdown_requested = true;
                if let Some(id) = id {
                    write_response(writer, id, Value::Null)?;
                }
            }
            "exit" => return Ok(true),
            "textDocument/didOpen" => {
                self.did_open(&msg);
                self.publish_diagnostics_for_message(writer, &msg)?;
            }
            "textDocument/didChange" => {
                self.did_change(&msg);
                self.publish_diagnostics_for_message(writer, &msg)?;
            }
            "textDocument/didSave" => {
                self.did_save(&msg);
                self.publish_diagnostics_for_message(writer, &msg)?;
            }
            "textDocument/didClose" => {
                self.did_close(&msg);
                self.publish_diagnostics_for_message(writer, &msg)?;
            }
            _ if id.is_some() => {
                let id = id.expect("checked above");
                let result = self.request_result(method, &msg);
                write_response(writer, id, result)?;
            }
            _ => {}
        }
        Ok(self.shutdown_requested && method == "exit")
    }

    fn configure_from_initialize(&mut self, msg: &Value) {
        let mut roots = Vec::new();
        if let Some(root_uri) = msg
            .get("params")
            .and_then(|p| p.get("rootUri"))
            .and_then(Value::as_str)
        {
            if let Some(root) = path_from_uri(root_uri) {
                push_existing_dir(&mut roots, root.clone());
                push_existing_dir(&mut roots, root.join("include"));
                push_existing_dir(&mut roots, root.join("inc"));
                push_existing_dir(&mut roots, root.join("src"));
            }
        } else if let Some(root_path) = msg
            .get("params")
            .and_then(|p| p.get("rootPath"))
            .and_then(Value::as_str)
        {
            let root = PathBuf::from(root_path);
            push_existing_dir(&mut roots, root.clone());
            push_existing_dir(&mut roots, root.join("include"));
            push_existing_dir(&mut roots, root.join("inc"));
            push_existing_dir(&mut roots, root.join("src"));
        }

        if let Some(options) = msg
            .get("params")
            .and_then(|p| p.get("initializationOptions"))
        {
            if let Some(include_roots) = options.get("includeRoots").and_then(Value::as_array) {
                for root in include_roots.iter().filter_map(Value::as_str) {
                    push_existing_dir(&mut roots, PathBuf::from(root));
                }
            }
            self.workspace.set_line_length_limits(
                options
                    .get("maxLineLength")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize),
                options
                    .get("maxCommentLineLength")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize),
            );
            self.workspace
                .set_predefined_macros(parse_predefined_macros(options));
        }

        self.workspace.set_include_roots(roots.clone());
        self.index_workspace_sources(&roots);
    }

    fn index_workspace_sources(&mut self, roots: &[PathBuf]) {
        let mut files = Vec::new();
        let mut visited = std::collections::HashSet::new();
        for root in roots {
            collect_fortran_files(root, &mut visited, &mut files);
        }
        for path in files {
            if self.open_sources.contains_key(&path) {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            if self
                .workspace
                .file(&path)
                .is_some_and(|file| file.source == source)
            {
                continue;
            }
            self.workspace.upsert_file(path, &source);
        }
    }

    fn did_open(&mut self, msg: &Value) {
        let Some(uri) = text_document_uri(msg) else {
            return;
        };
        let Some(path) = path_from_uri(&uri) else {
            return;
        };
        if !is_fortran_indexable(&path) {
            return;
        }
        let Some(text) = msg
            .get("params")
            .and_then(|p| p.get("textDocument"))
            .and_then(|d| d.get("text"))
            .and_then(Value::as_str)
        else {
            return;
        };
        self.workspace.upsert_file(path.clone(), text);
        self.open_sources.insert(path, text.to_string());
    }

    fn did_change(&mut self, msg: &Value) {
        let Some(uri) = text_document_uri(msg) else {
            return;
        };
        let Some(path) = path_from_uri(&uri) else {
            return;
        };
        if !is_fortran_indexable(&path) {
            return;
        }
        let Some(text) = msg
            .get("params")
            .and_then(|p| p.get("contentChanges"))
            .and_then(Value::as_array)
            .and_then(|changes| changes.last())
            .and_then(|change| change.get("text"))
            .and_then(Value::as_str)
        else {
            return;
        };
        self.workspace.upsert_file(path.clone(), text);
        self.open_sources.insert(path, text.to_string());
    }

    fn did_save(&mut self, msg: &Value) {
        let Some(uri) = text_document_uri(msg) else {
            return;
        };
        let Some(path) = path_from_uri(&uri) else {
            return;
        };
        if !is_fortran_indexable(&path) {
            return;
        }
        let text = msg
            .get("params")
            .and_then(|p| p.get("text"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| std::fs::read_to_string(&path).ok());
        if let Some(text) = text {
            self.workspace.upsert_file(path.clone(), &text);
            if self.open_sources.contains_key(&path) {
                self.open_sources.insert(path, text);
            }
        }
    }

    fn did_close(&mut self, msg: &Value) {
        let Some(uri) = text_document_uri(msg) else {
            return;
        };
        let Some(path) = path_from_uri(&uri) else {
            return;
        };
        self.open_sources.remove(&path);
        match std::fs::read_to_string(&path) {
            Ok(source) if is_fortran_indexable(&path) => {
                self.workspace.upsert_file(path, &source);
            }
            _ => self.workspace.remove_file(&path),
        }
    }

    fn request_result(&mut self, method: &str, msg: &Value) -> Value {
        match method {
            "textDocument/hover" => self.hover(msg).unwrap_or(Value::Null),
            "textDocument/signatureHelp" => self.signature_help(msg).unwrap_or(Value::Null),
            "textDocument/definition" => self.definition(msg).unwrap_or(Value::Null),
            "textDocument/implementation" => self.implementation(msg).unwrap_or(Value::Null),
            "textDocument/completion" => self.completion(msg).unwrap_or_else(|| {
                json!({
                    "isIncomplete": false,
                    "items": []
                })
            }),
            "textDocument/documentSymbol" => self
                .document_symbols(msg)
                .map(Value::Array)
                .unwrap_or_else(|| json!([])),
            "workspace/symbol" => self
                .workspace_symbols(msg)
                .map(Value::Array)
                .unwrap_or_else(|| json!([])),
            "textDocument/foldingRange" => self
                .folding_ranges(msg)
                .map(Value::Array)
                .unwrap_or_else(|| json!([])),
            "textDocument/codeAction" => self
                .code_actions(msg)
                .map(Value::Array)
                .unwrap_or_else(|| json!([])),
            "textDocument/references" => self
                .references(msg)
                .map(Value::Array)
                .unwrap_or_else(|| json!([])),
            "textDocument/documentHighlight" => self
                .document_highlight(msg)
                .map(Value::Array)
                .unwrap_or_else(|| json!([])),
            "textDocument/selectionRange" => self
                .selection_ranges(msg)
                .map(Value::Array)
                .unwrap_or_else(|| json!([])),
            "textDocument/inlayHint" => self
                .inlay_hints(msg)
                .map(Value::Array)
                .unwrap_or_else(|| json!([])),
            "textDocument/semanticTokens/full" => self
                .semantic_tokens(msg)
                .map(|data| json!({ "data": data }))
                .unwrap_or_else(|| json!({ "data": [] })),
            "textDocument/rename" => self.rename(msg).unwrap_or(Value::Null),
            _ => Value::Null,
        }
    }

    fn source_for_uri(&mut self, uri: &str) -> Option<(PathBuf, String)> {
        let path = path_from_uri(uri)?;
        if !is_fortran(&path) {
            return None;
        }
        if let Some(source) = self.open_sources.get(&path) {
            return Some((path, source.clone()));
        }
        let source = std::fs::read_to_string(&path).ok()?;
        self.workspace.upsert_file(path.clone(), &source);
        Some((path, source))
    }

    fn publish_diagnostics_for_message<W: Write>(
        &mut self,
        writer: &mut W,
        msg: &Value,
    ) -> io::Result<()> {
        let Some(uri) = text_document_uri(msg) else {
            return Ok(());
        };
        let Some(path) = path_from_uri(&uri) else {
            return Ok(());
        };
        if !is_fortran_indexable(&path) {
            return Ok(());
        }
        let diagnostics: Vec<Value> = self
            .workspace
            .diagnostics(&path)
            .into_iter()
            .map(diagnostic_to_lsp)
            .collect();
        write_lsp_message(
            writer,
            &json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": uri,
                    "diagnostics": diagnostics
                }
            }),
        )
    }

    fn hover(&mut self, msg: &Value) -> Option<Value> {
        let uri = text_document_uri(msg)?;
        let (line, character) = position(msg)?;
        let (path, source) = self.source_for_uri(&uri)?;
        let md = self
            .workspace
            .hover(&path, Position::new(line, character), &source)?;
        Some(json!({ "contents": { "kind": "markdown", "value": md } }))
    }

    fn signature_help(&mut self, msg: &Value) -> Option<Value> {
        let uri = text_document_uri(msg)?;
        let (line, character) = position(msg)?;
        let (path, source) = self.source_for_uri(&uri)?;
        self.workspace
            .signature_help(&path, Position::new(line, character), &source)
            .map(signature_help_to_lsp)
    }

    fn definition(&mut self, msg: &Value) -> Option<Value> {
        let uri = text_document_uri(msg)?;
        let (line, character) = position(msg)?;
        let (path, source) = self.source_for_uri(&uri)?;
        let loc =
            self.workspace
                .definition_location(&path, Position::new(line, character), &source)?;
        Some(location_to_lsp(&loc))
    }

    fn implementation(&mut self, msg: &Value) -> Option<Value> {
        let uri = text_document_uri(msg)?;
        let (line, character) = position(msg)?;
        let (path, source) = self.source_for_uri(&uri)?;
        let loc = self.workspace.implementation_location(
            &path,
            Position::new(line, character),
            &source,
        )?;
        Some(location_to_lsp(&loc))
    }

    fn completion(&mut self, msg: &Value) -> Option<Value> {
        let uri = text_document_uri(msg)?;
        let (line, character) = position(msg)?;
        let (path, source) = self.source_for_uri(&uri)?;
        let prefix = identifier_prefix(source.lines().nth(line).unwrap_or(""), character);
        let items: Vec<Value> = self
            .workspace
            .completions_at(&path, Position::new(line, character), &prefix)
            .into_iter()
            .map(completion_to_lsp)
            .collect();
        Some(json!({ "isIncomplete": false, "items": items }))
    }

    fn document_symbols(&mut self, msg: &Value) -> Option<Vec<Value>> {
        let uri = text_document_uri(msg)?;
        let (path, _) = self.source_for_uri(&uri)?;
        Some(
            self.workspace
                .document_symbols(&path)
                .iter()
                .map(document_symbol_to_lsp)
                .collect(),
        )
    }

    fn workspace_symbols(&mut self, msg: &Value) -> Option<Vec<Value>> {
        let query = msg
            .get("params")
            .and_then(|p| p.get("query"))
            .and_then(Value::as_str)
            .unwrap_or("");
        Some(
            self.workspace
                .workspace_symbols(query)
                .iter()
                .map(workspace_symbol_to_lsp)
                .collect(),
        )
    }

    fn folding_ranges(&mut self, msg: &Value) -> Option<Vec<Value>> {
        let uri = text_document_uri(msg)?;
        let (path, _) = self.source_for_uri(&uri)?;
        let mut ranges = Vec::new();
        for sym in self.workspace.document_symbols(&path) {
            collect_symbol_folds(&sym, &mut ranges);
        }
        Some(ranges)
    }

    fn code_actions(&mut self, msg: &Value) -> Option<Vec<Value>> {
        let uri = text_document_uri(msg)?;
        let (path, source) = self.source_for_uri(&uri)?;
        let start = msg
            .get("params")
            .and_then(|p| p.get("range"))
            .and_then(|r| r.get("start"));
        let actions = match start {
            Some(start) => {
                let line = start.get("line").and_then(Value::as_u64)? as usize;
                let character = start.get("character").and_then(Value::as_u64)? as usize;
                self.workspace
                    .code_actions_at(&path, Position::new(line, character), &source)
            }
            None => self.workspace.code_actions(&path),
        };
        Some(actions.into_iter().map(code_action_to_lsp).collect())
    }

    fn references(&mut self, msg: &Value) -> Option<Vec<Value>> {
        let uri = text_document_uri(msg)?;
        let (line, character) = position(msg)?;
        let (path, source) = self.source_for_uri(&uri)?;
        Some(
            self.workspace
                .references(&path, Position::new(line, character), &source)
                .into_iter()
                .map(|loc| location_to_lsp(&loc))
                .collect(),
        )
    }

    fn document_highlight(&mut self, msg: &Value) -> Option<Vec<Value>> {
        let uri = text_document_uri(msg)?;
        let (line, character) = position(msg)?;
        let (path, source) = self.source_for_uri(&uri)?;
        Some(
            self.workspace
                .references(&path, Position::new(line, character), &source)
                .into_iter()
                .filter(|loc| loc.file == path)
                .map(|loc| json!({ "range": range_to_lsp(&loc.range), "kind": 1 }))
                .collect(),
        )
    }

    fn selection_ranges(&mut self, msg: &Value) -> Option<Vec<Value>> {
        let uri = text_document_uri(msg)?;
        let positions = selection_range_positions(msg)?;
        let (path, source) = self.source_for_uri(&uri)?;
        Some(
            positions
                .into_iter()
                .filter_map(|pos| self.workspace.selection_range(&path, pos, &source))
                .map(selection_range_to_lsp)
                .collect(),
        )
    }

    fn inlay_hints(&mut self, msg: &Value) -> Option<Vec<Value>> {
        let uri = text_document_uri(msg)?;
        let (path, _) = self.source_for_uri(&uri)?;
        let range = msg.get("params")?.get("range")?;
        let start_line = range.get("start")?.get("line")?.as_u64()? as usize;
        let end_line = range.get("end")?.get("line")?.as_u64()? as usize;
        Some(
            self.workspace
                .inlay_hints(&path, start_line, end_line)
                .into_iter()
                .map(inlay_hint_to_lsp)
                .collect(),
        )
    }

    fn semantic_tokens(&mut self, msg: &Value) -> Option<Vec<u32>> {
        let uri = text_document_uri(msg)?;
        let (path, _) = self.source_for_uri(&uri)?;
        Some(self.workspace.semantic_token_data(&path))
    }

    fn rename(&mut self, msg: &Value) -> Option<Value> {
        let uri = text_document_uri(msg)?;
        let (line, character) = position(msg)?;
        let new_name = msg.get("params")?.get("newName")?.as_str()?;
        let (path, source) = self.source_for_uri(&uri)?;
        match self
            .workspace
            .rename(&path, Position::new(line, character), &source, new_name)
        {
            Ok(edits) => Some(workspace_edit_to_lsp(edits)),
            Err(err) => Some(rename_error_to_lsp(err)),
        }
    }
}

fn initialize_result() -> Value {
    json!({
        "capabilities": {
            "textDocumentSync": {
                "openClose": true,
                "change": 1,
                "save": { "includeText": true }
            },
            "hoverProvider": true,
            "definitionProvider": true,
            "implementationProvider": true,
            "referencesProvider": true,
            "documentHighlightProvider": true,
            "documentSymbolProvider": true,
            "workspaceSymbolProvider": true,
            "completionProvider": {
                "resolveProvider": false,
                "triggerCharacters": [":", "%", "(", ","]
            },
            "signatureHelpProvider": {
                "triggerCharacters": ["(", ","]
            },
            "foldingRangeProvider": true,
            "codeActionProvider": true,
            "selectionRangeProvider": true,
            "inlayHintProvider": true,
            "renameProvider": true,
            "semanticTokensProvider": {
                "legend": {
                    "tokenTypes": [
                        "namespace",
                        "type",
                        "function",
                        "method",
                        "property",
                        "variable",
                        "parameter",
                        "enumMember",
                        "macro"
                    ],
                    "tokenModifiers": []
                },
                "full": true,
                "range": false
            }
        },
        "serverInfo": {
            "name": "fortran-lsp",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn parse_predefined_macros(options: &Value) -> Vec<(String, String)> {
    let Some(raw) = options.get("predefinedMacros") else {
        return Vec::new();
    };
    let mut macros = Vec::new();
    if let Some(obj) = raw.as_object() {
        for (name, value) in obj {
            macros.push((
                name.clone(),
                value
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string()),
            ));
        }
    } else if let Some(values) = raw.as_array() {
        for value in values.iter().filter_map(Value::as_str) {
            let define = value.trim().trim_start_matches("-D");
            match define.split_once('=') {
                Some((name, val)) if !name.is_empty() => {
                    macros.push((name.to_string(), val.to_string()));
                }
                None if !define.is_empty() => macros.push((define.to_string(), String::new())),
                _ => {}
            }
        }
    }
    macros.sort();
    macros.dedup();
    macros
}

fn read_lsp_message<R: BufRead>(reader: &mut R) -> io::Result<Option<Value>> {
    let mut content_len = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_len = rest.trim().parse::<usize>().ok();
        }
    }
    let Some(len) = content_len else {
        return Ok(None);
    };
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body).ok())
}

fn write_response<W: Write>(writer: &mut W, id: Value, result: Value) -> io::Result<()> {
    write_lsp_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        }),
    )
}

fn write_lsp_message<W: Write>(writer: &mut W, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

fn diagnostic_to_lsp(diagnostic: fortran_lsp::Diagnostic) -> Value {
    let severity = match diagnostic.severity {
        DiagnosticSeverity::Error => 1,
        DiagnosticSeverity::Warning => 2,
        DiagnosticSeverity::Information => 3,
    };
    json!({
        "range": range_to_lsp(&diagnostic.range),
        "severity": severity,
        "source": "fortran-lsp",
        "message": diagnostic.message
    })
}

fn signature_help_to_lsp(help: SignatureHelp) -> Value {
    let parameters: Vec<Value> = help
        .parameters
        .iter()
        .map(|param| json!({ "label": param }))
        .collect();
    let mut signature = json!({
        "label": help.label,
        "parameters": parameters
    });
    if let Some(docs) = help.documentation {
        signature["documentation"] = json!({ "kind": "markdown", "value": docs });
    }
    json!({
        "signatures": [signature],
        "activeSignature": 0,
        "activeParameter": help.active_parameter
    })
}

fn completion_to_lsp(item: CompletionItem) -> Value {
    let mut out = json!({
        "label": item.label,
        "kind": completion_kind(item.kind),
        "detail": item.detail
    });
    if let Some(docs) = item.documentation {
        out["documentation"] = json!({ "kind": "markdown", "value": docs });
    }
    out
}

fn inlay_hint_to_lsp(hint: InlayHint) -> Value {
    json!({
        "position": position_to_lsp(&hint.position),
        "label": hint.label,
        "kind": 2,
        "paddingRight": true
    })
}

fn document_symbol_to_lsp(sym: &DocumentSymbol) -> Value {
    json!({
        "name": sym.name,
        "detail": sym.detail,
        "kind": symbol_kind(sym.kind),
        "range": range_to_lsp(&sym.range),
        "selectionRange": range_to_lsp(&sym.selection_range),
        "children": sym.children.iter().map(document_symbol_to_lsp).collect::<Vec<_>>()
    })
}

fn workspace_symbol_to_lsp(sym: &Symbol) -> Value {
    let mut item = json!({
        "name": sym.qualified_name(),
        "kind": symbol_kind(sym.kind),
        "location": location_to_lsp(&Location {
            file: sym.file.clone(),
            range: sym.selection_range.clone(),
        })
    });
    if !sym.scope.is_empty() {
        item.as_object_mut().unwrap().insert(
            "containerName".to_string(),
            Value::String(sym.scope.join("::")),
        );
    }
    item
}

fn selection_range_to_lsp(selection: SelectionRange) -> Value {
    let mut item = json!({ "range": range_to_lsp(&selection.range) });
    if let Some(parent) = selection.parent {
        item["parent"] = selection_range_to_lsp(*parent);
    }
    item
}

fn collect_symbol_folds(sym: &DocumentSymbol, out: &mut Vec<Value>) {
    if sym.range.end.line > sym.range.start.line {
        out.push(json!({
            "startLine": sym.range.start.line,
            "startCharacter": sym.range.start.character,
            "endLine": sym.range.end.line,
            "endCharacter": sym.range.end.character,
            "kind": "region"
        }));
    }
    for child in &sym.children {
        collect_symbol_folds(child, out);
    }
}

fn location_to_lsp(loc: &Location) -> Value {
    json!({
        "uri": uri_from_path(&loc.file),
        "range": range_to_lsp(&loc.range)
    })
}

fn code_action_to_lsp(action: CodeAction) -> Value {
    json!({
        "title": action.title,
        "kind": action.kind,
        "edit": workspace_edit_to_lsp(action.edits)
    })
}

fn workspace_edit_to_lsp(edits: Vec<TextEdit>) -> Value {
    let mut changes = serde_json::Map::new();
    for edit in edits {
        changes
            .entry(uri_from_path(&edit.file))
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("workspace edit entry is always an array")
            .push(json!({
                "range": range_to_lsp(&edit.range),
                "newText": edit.new_text
            }));
    }
    json!({ "changes": changes })
}

fn selection_range_positions(msg: &Value) -> Option<Vec<Position>> {
    let positions = msg.get("params")?.get("positions")?.as_array()?;
    Some(
        positions
            .iter()
            .filter_map(|pos| {
                Some(Position::new(
                    pos.get("line")?.as_u64()? as usize,
                    pos.get("character")?.as_u64()? as usize,
                ))
            })
            .collect(),
    )
}

fn rename_error_to_lsp(err: RenameError) -> Value {
    let message = match err {
        RenameError::UnresolvedSymbol => "No Fortran symbol at cursor".to_string(),
        RenameError::InvalidIdentifier => "New name is not a valid Fortran identifier".to_string(),
        RenameError::ConflictingSymbol { file, range } => format!(
            "Rename would conflict with symbol at {}:{}:{}",
            file.display(),
            range.start.line + 1,
            range.start.character + 1
        ),
    };
    json!({
        "documentChanges": [],
        "failureReason": message
    })
}

fn range_to_lsp(range: &Range) -> Value {
    json!({
        "start": { "line": range.start.line, "character": range.start.character },
        "end": { "line": range.end.line, "character": range.end.character }
    })
}

fn position_to_lsp(position: &Position) -> Value {
    json!({ "line": position.line, "character": position.character })
}

fn symbol_kind(kind: SymbolKind) -> u32 {
    match kind {
        SymbolKind::Module | SymbolKind::Program | SymbolKind::Submodule => 2,
        SymbolKind::Interface => 11,
        SymbolKind::Type => 23,
        SymbolKind::Subroutine | SymbolKind::Function => 12,
        SymbolKind::Method => 6,
        SymbolKind::Variable => 13,
        SymbolKind::Block | SymbolKind::Associate | SymbolKind::SelectType => 3,
        SymbolKind::Use => 2,
    }
}

fn completion_kind(kind: SymbolKind) -> u32 {
    match kind {
        SymbolKind::Module | SymbolKind::Program | SymbolKind::Submodule => 9,
        SymbolKind::Interface | SymbolKind::Type => 7,
        SymbolKind::Subroutine | SymbolKind::Function | SymbolKind::Method => 3,
        SymbolKind::Variable => 6,
        SymbolKind::Block | SymbolKind::Associate | SymbolKind::SelectType => 14,
        SymbolKind::Use => 9,
    }
}

fn identifier_prefix(line: &str, character: usize) -> String {
    let byte_idx = byte_idx_for_utf16_col(line, character);
    let prefix = &line[..byte_idx.min(line.len())];
    let start = prefix
        .char_indices()
        .rev()
        .find(|(_, ch)| !(*ch == '_' || ch.is_ascii_alphanumeric()))
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    prefix[start..].to_string()
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

fn position(msg: &Value) -> Option<(usize, usize)> {
    let pos = msg.get("params")?.get("position")?;
    Some((
        pos.get("line")?.as_u64()? as usize,
        pos.get("character")?.as_u64()? as usize,
    ))
}

fn text_document_uri(msg: &Value) -> Option<String> {
    msg.get("params")?
        .get("textDocument")?
        .get("uri")?
        .as_str()
        .map(ToString::to_string)
}

fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let raw = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(raw.len());
    let raw_bytes = raw.as_bytes();
    let mut idx = 0;
    while idx < raw_bytes.len() {
        if raw_bytes[idx] == b'%' && idx + 2 < raw_bytes.len() {
            let hex = std::str::from_utf8(&raw_bytes[idx + 1..idx + 3]).ok()?;
            let byte = u8::from_str_radix(hex, 16).ok()?;
            bytes.push(byte);
            idx += 3;
        } else {
            bytes.push(raw_bytes[idx]);
            idx += 1;
        }
    }
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

fn uri_from_path(path: &Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    format!("file://{}", abs.to_string_lossy())
}

fn is_fortran(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
            .unwrap_or(""),
        "f" | "for" | "ftn" | "f90" | "f95" | "f03" | "f08" | "f18" | "f77" | "f66"
    )
}

fn is_fortran_indexable(path: &Path) -> bool {
    is_fortran(path)
        || matches!(
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref()
                .unwrap_or(""),
            "inc"
        )
}

fn collect_fortran_files(
    dir: &Path,
    visited: &mut std::collections::HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) {
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    if !visited.insert(canonical) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "target" || name == "build" {
                continue;
            }
            collect_fortran_files(&path, visited, out);
        } else if file_type.is_file() && is_fortran_indexable(&path) {
            out.push(path);
        }
    }
}

fn push_existing_dir(roots: &mut Vec<PathBuf>, path: PathBuf) {
    if !path.is_dir() {
        return;
    }
    let key = path.canonicalize().unwrap_or_else(|_| path.clone());
    if !roots
        .iter()
        .any(|root| root.canonicalize().unwrap_or_else(|_| root.clone()) == key)
    {
        roots.push(path);
    }
}
