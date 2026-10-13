//! LSP 协议子集类型 + 源码偏移 ↔ LSP 位置双向换算。
//!
//! 设计（22 号决策 73 / 28 号 §2.4）：手写最小 JSON-RPC 子集，零异步依赖，
//! 与 check_source / yoru_fmt / BUILTINS 复用同一套前端，不倒灌语言服务。

use serde::Serialize;

// ---- 位置与范围 ----

/// LSP `Position`：行/列均为从 0 计；列以 **UTF-16 码元**计（VS Code / Neovim 约定）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Serialize)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

// ---- 诊断 ----

/// `DiagnosticSeverity`：1=Error, 2=Warning, 3=Info, 4=Hint。
pub const SEV_ERROR: u8 = 1;
pub const SEV_WARNING: u8 = 2;
pub const SEV_INFO: u8 = 3;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspDiagnostic {
    pub range: Range,
    pub severity: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub source: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishDiagnosticsParams {
    pub uri: String,
    pub diagnostics: Vec<LspDiagnostic>,
}

// ---- hover ----

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkupContent {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hover {
    pub contents: MarkupContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
}

// ---- completion ----

/// `CompletionItemKind`：3=Function, 13=Constant, 14=Keyword, 22=Struct/Class。
pub const KIND_FUNCTION: u8 = 3;
pub const KIND_MODULE: u8 = 9;
pub const KIND_CONSTANT: u8 = 13;
pub const KIND_KEYWORD: u8 = 14;
pub const KIND_STRUCT: u8 = 22;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionItem {
    pub label: String,
    pub kind: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<MarkupContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insert_text: Option<String>,
}

// ---- formatting ----

#[derive(Debug, Clone, Serialize)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

// ---- 能力声明（initialize 响应）----

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionProvider {
    pub trigger_characters: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    /// 1 = Full（客户端每次变更发送整篇文档）
    pub text_document_sync: u8,
    pub hover_provider: bool,
    pub definition_provider: bool,
    pub document_formatting_provider: bool,
    pub completion_provider: CompletionProvider,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub capabilities: ServerCapabilities,
    pub server_info: ServerInfo,
}

// ---- 偏移 ↔ 位置换算 ----

/// 字节偏移 → LSP 位置。`offset` 越界则钳到文末。UTF-16 列宽对多字节字符正确。
pub fn offset_to_position(src: &str, offset: usize) -> Position {
    let offset = offset.min(src.len());
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (i, b) in src.bytes().enumerate() {
        if i >= offset {
            break;
        }
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    let line_str = &src[line_start..offset];
    let utf16 = line_str.chars().map(|c| c.len_utf16()).sum::<usize>();
    Position {
        line,
        character: utf16 as u32,
    }
}

/// LSP 位置 → 字节偏移。列按 UTF-16 码元计；行列越界则钳到最近合法位置。
pub fn position_to_offset(src: &str, line: u32, character: u32) -> usize {
    let mut cur_line = 0u32;
    let mut line_start = 0usize;
    for (i, b) in src.bytes().enumerate() {
        if cur_line == line {
            break;
        }
        if b == b'\n' {
            cur_line += 1;
            line_start = i + 1;
        }
    }
    let line_end = src[line_start..]
        .find('\n')
        .map(|p| line_start + p)
        .unwrap_or(src.len());
    let line_str = &src[line_start..line_end];
    let mut off = line_start;
    let mut utf16 = 0u32;
    for ch in line_str.chars() {
        if utf16 >= character {
            break;
        }
        utf16 += ch.len_utf16() as u32;
        off += ch.len_utf8();
    }
    off
}
