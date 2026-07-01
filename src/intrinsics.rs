use std::sync::OnceLock;

use crate::model::{SymbolKind, Visibility};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntrinsicKind {
    Function,
    Subroutine,
    Module,
    Constant,
    Type,
    Keyword,
}

impl IntrinsicKind {
    pub fn symbol_kind(self) -> SymbolKind {
        match self {
            IntrinsicKind::Function => SymbolKind::Function,
            IntrinsicKind::Subroutine => SymbolKind::Subroutine,
            IntrinsicKind::Module => SymbolKind::Module,
            IntrinsicKind::Constant => SymbolKind::Variable,
            IntrinsicKind::Type => SymbolKind::Type,
            IntrinsicKind::Keyword => SymbolKind::Variable,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            IntrinsicKind::Function => "intrinsic function",
            IntrinsicKind::Subroutine => "intrinsic subroutine",
            IntrinsicKind::Module => "intrinsic module",
            IntrinsicKind::Constant => "intrinsic constant",
            IntrinsicKind::Type => "intrinsic type",
            IntrinsicKind::Keyword => "keyword",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntrinsicSymbol {
    pub name: String,
    pub kind: IntrinsicKind,
    pub args: Vec<String>,
    pub documentation: String,
    pub module: Option<String>,
}

impl IntrinsicSymbol {
    pub fn signature(&self) -> String {
        match self.kind {
            IntrinsicKind::Function | IntrinsicKind::Subroutine => {
                format!("{}({})", self.name, self.args.join(", "))
            }
            _ => self.name.clone(),
        }
    }

    pub fn hover_markdown(&self) -> String {
        let mut out = format!("```fortran\n{}\n```", self.signature());
        if !self.documentation.is_empty() {
            out.push_str("\n\n");
            out.push_str(&self.documentation);
        }
        if let Some(module) = &self.module {
            out.push_str("\n\nmodule: `");
            out.push_str(module);
            out.push('`');
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntrinsicCompletion {
    pub label: String,
    pub detail: String,
    pub kind: SymbolKind,
    pub documentation: Option<String>,
    pub visibility: Visibility,
}

impl IntrinsicCompletion {
    pub fn from_symbol(sym: &IntrinsicSymbol) -> Self {
        Self {
            label: sym.name.clone(),
            detail: sym.signature(),
            kind: sym.kind.symbol_kind(),
            documentation: (!sym.documentation.is_empty()).then(|| sym.documentation.clone()),
            visibility: Visibility::Public,
        }
    }
}

pub fn find_intrinsic(name: &str) -> Option<&'static IntrinsicSymbol> {
    intrinsics()
        .iter()
        .find(|sym| sym.name.eq_ignore_ascii_case(name))
}

pub fn find_global_intrinsic(name: &str) -> Option<&'static IntrinsicSymbol> {
    intrinsics()
        .iter()
        .filter(|sym| sym.module.is_none())
        .find(|sym| sym.name.eq_ignore_ascii_case(name))
}

pub fn find_intrinsic_module(name: &str) -> Option<&'static IntrinsicSymbol> {
    intrinsics()
        .iter()
        .find(|sym| sym.kind == IntrinsicKind::Module && sym.name.eq_ignore_ascii_case(name))
}

pub fn module_exports(module: &str, name: &str) -> bool {
    intrinsics().iter().any(|sym| {
        sym.module
            .as_deref()
            .is_some_and(|scope| scope.eq_ignore_ascii_case(module))
            && sym.name.eq_ignore_ascii_case(name)
    })
}

pub fn module_symbols(module: &str) -> impl Iterator<Item = &'static IntrinsicSymbol> + '_ {
    intrinsics().iter().filter(move |sym| {
        sym.module
            .as_deref()
            .is_some_and(|scope| scope.eq_ignore_ascii_case(module))
    })
}

pub fn completions(prefix: &str) -> impl Iterator<Item = IntrinsicCompletion> + '_ {
    let prefix = prefix.to_ascii_lowercase();
    intrinsics()
        .iter()
        .filter(move |sym| sym.name.to_ascii_lowercase().starts_with(&prefix))
        .map(IntrinsicCompletion::from_symbol)
}

pub fn intrinsics() -> &'static [IntrinsicSymbol] {
    static INTRINSICS: OnceLock<Vec<IntrinsicSymbol>> = OnceLock::new();
    INTRINSICS.get_or_init(load_intrinsics)
}

fn load_intrinsics() -> Vec<IntrinsicSymbol> {
    let mut symbols = Vec::new();
    load_procedures(&mut symbols);
    load_modules(&mut symbols);
    symbols.sort_by(|a, b| {
        a.module
            .cmp(&b.module)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| kind_rank(a.kind).cmp(&kind_rank(b.kind)))
            .then_with(|| a.args.cmp(&b.args))
    });
    symbols.dedup_by(|a, b| {
        a.name.eq_ignore_ascii_case(&b.name) && a.module == b.module && a.kind == b.kind
    });
    symbols
}

fn load_procedures(out: &mut Vec<IntrinsicSymbol>) {
    let data: serde_json::Value =
        serde_json::from_str(include_str!("data/intrinsic.procedures.json"))
            .expect("vendored fortls intrinsic procedure JSON is valid");
    let Some(map) = data.as_object() else {
        return;
    };
    for (name, item) in map {
        let kind = match item.get("type").and_then(serde_json::Value::as_i64) {
            Some(2) => IntrinsicKind::Subroutine,
            Some(3) => IntrinsicKind::Function,
            _ => continue,
        };
        out.push(IntrinsicSymbol {
            name: canonical_name(name),
            kind,
            args: parse_args(item.get("args").and_then(serde_json::Value::as_str)),
            documentation: item
                .get("doc")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            module: None,
        });
    }
}

fn load_modules(out: &mut Vec<IntrinsicSymbol>) {
    let data: serde_json::Value = serde_json::from_str(include_str!("data/intrinsic.modules.json"))
        .expect("vendored fortls intrinsic module JSON is valid");
    let Some(map) = data.as_object() else {
        return;
    };
    for (fallback_name, module) in map {
        let module_name = module
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(fallback_name);
        let module_name = canonical_name(module_name);
        out.push(IntrinsicSymbol {
            name: module_name.clone(),
            kind: IntrinsicKind::Module,
            args: Vec::new(),
            documentation: format!("Intrinsic module `{module_name}`."),
            module: None,
        });
        let Some(children) = module.get("children").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for child in children {
            let Some(name) = child.get("name").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let kind = match child.get("type").and_then(serde_json::Value::as_i64) {
                Some(1) => IntrinsicKind::Subroutine,
                Some(2) => IntrinsicKind::Function,
                Some(3) => IntrinsicKind::Constant,
                Some(4) => IntrinsicKind::Type,
                _ => continue,
            };
            out.push(IntrinsicSymbol {
                name: canonical_name(name),
                kind,
                args: parse_args(child.get("args").and_then(serde_json::Value::as_str)),
                documentation: module_child_doc(child),
                module: Some(module_name.clone()),
            });
        }
    }
}

fn module_child_doc(child: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    if let Some(return_type) = child.get("return").and_then(serde_json::Value::as_str) {
        parts.push(format!("Returns `{return_type}`."));
    }
    if let Some(desc) = child.get("desc").and_then(serde_json::Value::as_str) {
        parts.push(desc.to_string());
    }
    parts.join("\n\n")
}

fn parse_args(args: Option<&str>) -> Vec<String> {
    args.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|arg| !arg.is_empty())
        .map(canonical_name)
        .collect()
}

fn canonical_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn kind_rank(kind: IntrinsicKind) -> u8 {
    match kind {
        IntrinsicKind::Module => 0,
        IntrinsicKind::Function => 1,
        IntrinsicKind::Subroutine => 2,
        IntrinsicKind::Type => 3,
        IntrinsicKind::Constant => 4,
        IntrinsicKind::Keyword => 5,
    }
}
