//! 分析层：把既有前端/typeck/fmt API 桥接为 LSP 能力。
//!
//! 复用（不重复实现）：`yoru_typeck::check_source`（诊断）、`yoru_fmt::format`
//! （格式化）、`yoru_syntax::tokenize`/`parse_source`（hover/补全/定义定位）、
//! `yoru_stdlib::BUILTINS`（补全/hover 免费内容源，28 号 §2.4）。

use yoru_syntax::ast::{Decl, DeclKind, FnDecl, Type, TypeBody};
use yoru_syntax::{Span, Token, TokenKind, parse_source, tokenize};
use yoru_typeck::{Env, check_source};

use crate::lsp::*;

// ---- 诊断 ----

/// 对整篇源码跑 typeck，映射为 LSP 诊断（push 模型：didOpen/Change/Save 时全量重算）。
pub fn diagnostics(src: &str) -> Vec<LspDiagnostic> {
    let env = Env::core();
    let (diags, _ok) = check_source(src, &env);
    diags
        .into_iter()
        .map(|d| {
            let sp = d.span.unwrap_or(Span::new(0, 0));
            let severity = match d.severity {
                yoru_syntax::Severity::Error => SEV_ERROR,
                yoru_syntax::Severity::Warning => SEV_WARNING,
                yoru_syntax::Severity::Info => SEV_INFO,
            };
            LspDiagnostic {
                range: Range {
                    start: offset_to_position(src, sp.start),
                    end: offset_to_position(src, sp.end),
                },
                severity,
                code: d.code.clone(),
                source: "yoru".to_string(),
                message: d.message.clone(),
            }
        })
        .collect()
}

// ---- hover ----

/// 在 `offset` 处取 hover 信息：内建（文档+签名）优先，其次用户声明（签名）。
pub fn hover(src: &str, offset: usize) -> Option<Hover> {
    let (tokens, _) = tokenize(src);
    let (idx, tok) = tokens.iter().enumerate().find(|(_, t)| {
        matches!(t.kind, TokenKind::Ident(_)) && t.span.start <= offset && offset <= t.span.end
    })?;
    let name = match &tok.kind {
        TokenKind::Ident(n) => n.clone(),
        _ => return None,
    };

    // 模块上下文：`module.ident` → 取模块限定内建
    let module_ctx: Option<String> = if idx >= 2 {
        let prev = &tokens[idx - 1];
        let prev2 = &tokens[idx - 2];
        if matches!(prev.kind, TokenKind::Dot) {
            if let TokenKind::Ident(m) = &prev2.kind {
                Some(m.clone())
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    if let Some(m) = &module_ctx {
        if let Some(b) = yoru_stdlib::module_fn(m, &name) {
            return Some(builtin_hover(&format!("{m}.{}", b.name), b));
        }
    } else {
        let named = yoru_stdlib::builtins_named(&name);
        if let Some(b) = named.first() {
            return Some(builtin_hover(b.name, b));
        }
    }

    user_symbol_hover(src, &name).map(|value| Hover {
        contents: MarkupContent {
            kind: "markdown".to_string(),
            value,
        },
        range: None,
    })
}

fn builtin_hover(full: &str, b: &yoru_stdlib::Builtin) -> Hover {
    let value = format!(
        "**`{full}`** — 内建函数\n\n```\n{}\n```\n\n{}",
        builtin_signature(full, b),
        b.doc
    );
    Hover {
        contents: MarkupContent {
            kind: "markdown".to_string(),
            value,
        },
        range: None,
    }
}

/// 注册表参数 DSL → 可读类型串。
fn param_str(p: &yoru_stdlib::Param) -> String {
    use yoru_stdlib::Param::*;
    match p {
        Int => "int".into(),
        Float => "float".into(),
        Number => "number".into(),
        Str => "string".into(),
        Bool => "bool".into(),
        Unit => "unit".into(),
        Json => "json".into(),
        Rune => "rune".into(),
        Any => "any".into(),
        List(e) => format!("[{}]", param_str(&e[0])),
        MapKV(k, v) => format!("[{}: {}]", k, v),
        Range => "range".into(),
        Collection => "collection".into(),
        Fn(ps, r) => format!(
            "fn({}) -> {}",
            ps.iter().map(param_str).collect::<Vec<_>>().join(", "),
            param_str(&r[0])
        ),
        Var(n) => n.to_string(),
        Opt(inner) => format!("{}?", param_str(&inner[0])),
    }
}

fn builtin_signature(full: &str, b: &yoru_stdlib::Builtin) -> String {
    let params = b
        .params
        .iter()
        .map(param_str)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{full}({params}) -> {}", param_str(&b.ret))
}

fn type_str(t: &Type) -> String {
    match t {
        Type::Int => "int".into(),
        Type::Float => "float".into(),
        Type::Bool => "bool".into(),
        Type::Str => "string".into(),
        Type::Rune => "rune".into(),
        Type::Unit => "unit".into(),
        Type::Json => "json".into(),
        Type::Named(n, args) => {
            if args.is_empty() {
                n.clone()
            } else {
                format!(
                    "{n}<{}>",
                    args.iter().map(type_str).collect::<Vec<_>>().join(", ")
                )
            }
        }
        Type::Option(inner) => format!("{}?", type_str(inner)),
        Type::Result(o, e) => format!("Result[{}, {}]", type_str(o), type_str(e)),
        Type::List(e) => format!("[{}]", type_str(e)),
        Type::Map(k, v) => format!("[{}: {}]", type_str(k), type_str(v)),
        Type::Fn(ps, r) => format!(
            "fn({}) -> {}",
            ps.iter().map(type_str).collect::<Vec<_>>().join(", "),
            type_str(r)
        ),
        Type::Distinct(inner) => format!("distinct {}", type_str(inner)),
    }
}

fn ret_str(ret: &Option<Type>) -> String {
    match ret {
        Some(t) => format!(" -> {}", type_str(t)),
        None => String::new(),
    }
}

fn user_symbol_hover(src: &str, name: &str) -> Option<String> {
    let prog = parse_source(src).ok()?;
    for d in &prog.decls {
        let detail = match &d.kind {
            DeclKind::Fn(f) if f.name == name => fn_detail(f),
            DeclKind::Struct {
                name: n, fields, ..
            } if n == name => format!(
                "struct {n} {{ {} }}",
                fields
                    .iter()
                    .map(|f| format!("{}: {}", f.name, type_str(&f.ty)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            DeclKind::Tag {
                name: n, variants, ..
            } if n == name => format!(
                "tag {n} = {}",
                variants
                    .iter()
                    .map(|v| v.name.clone())
                    .collect::<Vec<_>>()
                    .join(" | ")
            ),
            DeclKind::Type { name: n, body, .. } if n == name => {
                format!("type {n} = {}", type_body_str(body))
            }
            DeclKind::Const { name: n, ty, .. } if n == name => {
                format!("const {n}: {}", type_str(ty))
            }
            _ => continue,
        };
        return Some(format!("**`{name}`**\n\n```\n{detail}\n```"));
    }
    None
}

fn fn_detail(f: &FnDecl) -> String {
    let params = f
        .params
        .iter()
        .map(|p| format!("{}: {}", p.name, type_str(&p.ty)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("fn {}({}){}", f.name, params, ret_str(&f.ret))
}

fn type_body_str(b: &TypeBody) -> String {
    match b {
        TypeBody::Alias(t) => type_str(t),
        TypeBody::Distinct(t) => format!("distinct {}", type_str(t)),
        TypeBody::Union(vs) => vs
            .iter()
            .map(|v| v.name.clone())
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

// ---- completion ----

const KEYWORDS: &[&str] = &[
    "fn", "let", "mut", "return", "if", "else", "for", "in", "while", "match", "struct", "tag",
    "type", "protocol", "impl", "const", "import", "as", "try", "catch", "throw", "await",
    "distinct", "pub", "break", "continue", "true", "false", "nil",
];

/// 在 `offset` 处生成补全项：注册表 + 用户声明 + 关键字；识别 `module.` 上下文。
pub fn completions(src: &str, offset: usize) -> Vec<CompletionItem> {
    let (prefix, module_ctx) = completion_context(src, offset);
    let mut items = Vec::new();

    if let Some(m) = &module_ctx {
        // `module.` 后：仅列该模块的裸方法名（编辑器在 `.` 后插入）
        for b in yoru_stdlib::BUILTINS.iter() {
            if b.module == Some(m.as_str()) && b.name.starts_with(&prefix) {
                items.push(builtin_item(b.name, b, None));
            }
        }
        return items;
    }

    // 自由内建（按名去重）
    let mut seen = std::collections::HashSet::new();
    for b in yoru_stdlib::BUILTINS.iter() {
        if seen.insert(b.name) && b.name.starts_with(&prefix) {
            items.push(builtin_item(b.name, b, b.module));
        }
    }
    // 模块限定形式 `module.name`
    let mut mseen = std::collections::HashSet::new();
    for b in yoru_stdlib::BUILTINS.iter() {
        if let Some(m) = b.module {
            let full = format!("{m}.{}", b.name);
            if mseen.insert(full.clone()) && full.starts_with(&prefix) {
                let sig = builtin_signature(&full, b);
                items.push(CompletionItem {
                    label: full,
                    kind: KIND_FUNCTION,
                    detail: Some(sig),
                    documentation: Some(MarkupContent {
                        kind: "markdown".into(),
                        value: b.doc.into(),
                    }),
                    insert_text: None,
                });
            }
        }
    }
    // 模块名本身也可补全（输入 `mat` 应能补出 `math`）
    for b in yoru_stdlib::BUILTINS.iter() {
        if let Some(m) = b.module {
            if mseen.insert(m.to_string()) && m.starts_with(&prefix) {
                items.push(CompletionItem {
                    label: m.to_string(),
                    kind: KIND_MODULE,
                    detail: Some("module".into()),
                    documentation: None,
                    insert_text: None,
                });
            }
        }
    }
    // 用户声明
    if let Ok(prog) = parse_source(src) {
        for d in &prog.decls {
            if let Some(n) = decl_name(d) {
                if n.starts_with(&prefix) {
                    items.push(CompletionItem {
                        label: n.clone(),
                        kind: decl_kind_code(d),
                        detail: None,
                        documentation: None,
                        insert_text: None,
                    });
                }
            }
        }
    }
    // 关键字
    for kw in KEYWORDS {
        if kw.starts_with(&prefix) {
            items.push(CompletionItem {
                label: kw.to_string(),
                kind: KIND_KEYWORD,
                detail: Some("keyword".into()),
                documentation: None,
                insert_text: None,
            });
        }
    }
    items
}

fn builtin_item(label: &str, b: &yoru_stdlib::Builtin, module: Option<&str>) -> CompletionItem {
    let full = match module {
        Some(m) => format!("{m}.{label}"),
        None => label.to_string(),
    };
    CompletionItem {
        label: label.to_string(),
        kind: KIND_FUNCTION,
        detail: Some(builtin_signature(&full, b)),
        documentation: Some(MarkupContent {
            kind: "markdown".into(),
            value: b.doc.into(),
        }),
        insert_text: None,
    }
}

/// 取光标前（当前行内）的 `module.` 上下文与已输入前缀。
fn completion_context(src: &str, offset: usize) -> (String, Option<String>) {
    let line_prefix = {
        let before = &src[..offset.min(src.len())];
        let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        &src[line_start..offset.min(src.len())]
    };
    // 末尾标识符
    let bytes = line_prefix.as_bytes();
    let end = bytes.len();
    let mut i = bytes.len();
    while i > 0 {
        let c = bytes[i - 1];
        if c.is_ascii_alphanumeric() || c == b'_' {
            i -= 1;
        } else {
            break;
        }
    }
    let ident = &line_prefix[i..end];
    // 前导 `module.`
    if i > 0 && bytes[i - 1] == b'.' {
        let mut k = i - 1;
        while k > 0 {
            let c = bytes[k - 1];
            if c.is_ascii_alphanumeric() || c == b'_' {
                k -= 1;
            } else {
                break;
            }
        }
        let module = &line_prefix[k..(i - 1)];
        if !module.is_empty() {
            return (ident.to_string(), Some(module.to_string()));
        }
    }
    (ident.to_string(), None)
}

fn decl_name(d: &Decl) -> Option<String> {
    match &d.kind {
        DeclKind::Fn(f) => Some(f.name.clone()),
        DeclKind::Struct { name, .. } => Some(name.clone()),
        DeclKind::Tag { name, .. } => Some(name.clone()),
        DeclKind::Type { name, .. } => Some(name.clone()),
        DeclKind::Const { name, .. } => Some(name.clone()),
        DeclKind::Protocol { name, .. } => Some(name.clone()),
        _ => None,
    }
}

fn decl_kind_code(d: &Decl) -> u8 {
    match &d.kind {
        DeclKind::Fn(_) => KIND_FUNCTION,
        DeclKind::Const { .. } => KIND_CONSTANT,
        _ => KIND_STRUCT,
    }
}

// ---- 定义跳转 ----

/// 跳转到光标处标识符的用户声明位置（内建无源码位置，返回 None）。
pub fn definition(src: &str, uri: &str, offset: usize) -> Option<Location> {
    let (tokens, _) = tokenize(src);
    let tok = tokens.iter().find(|t| {
        matches!(t.kind, TokenKind::Ident(_)) && t.span.start <= offset && offset <= t.span.end
    })?;
    let name = match &tok.kind {
        TokenKind::Ident(n) => n.clone(),
        _ => return None,
    };
    let prog = parse_source(src).ok()?;
    for d in &prog.decls {
        if let Some(n) = decl_name(d) {
            if n == name {
                return Some(Location {
                    uri: uri.to_string(),
                    range: range_of_span(src, d.span),
                });
            }
        }
    }
    None
}

fn range_of_span(src: &str, sp: Span) -> Range {
    Range {
        start: offset_to_position(src, sp.start),
        end: offset_to_position(src, sp.end),
    }
}

// ---- 格式化 ----

/// 复用 yoru_fmt：解析失败返回 None（编辑器不应用），否则返回替换整篇的 TextEdit。
pub fn format_edit(src: &str) -> Option<Vec<TextEdit>> {
    match yoru_fmt::format(src) {
        Ok(out) => {
            let end = offset_to_position(src, src.len());
            Some(vec![TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end,
                },
                new_text: out,
            }])
        }
        Err(_) => None,
    }
}

// 供 server 与测试共用的 token 判定辅助
#[allow(dead_code)]
pub(crate) fn is_ident_token(t: &Token) -> bool {
    matches!(t.kind, TokenKind::Ident(_))
}
