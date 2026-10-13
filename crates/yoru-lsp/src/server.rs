//! LSP 服务端：维护打开的文档，路由 JSON-RPC 请求/通知。
//!
//! 诊断采用 **push 模型**（didOpen/Change/Save 时全量重算并 publishDiagnostics），
//! 经 `pending` 通知队列在每次消息处理后由 transport 冲刷。

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::analysis;
use crate::lsp::*;

pub struct Server {
    pub documents: HashMap<String, String>,
    pending: Vec<Value>,
    shutdown: bool,
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

impl Server {
    pub fn new() -> Self {
        Server {
            documents: HashMap::new(),
            pending: Vec::new(),
            shutdown: false,
        }
    }

    /// 取走待发的通知（transport 在发完响应后冲刷）。
    pub fn drain_pending(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.pending)
    }

    /// 路由一条 JSON-RPC 消息。返回该消息的响应（通知返回 None）。
    pub fn handle(&mut self, msg: &Value) -> Option<Value> {
        let method = msg.get("method").and_then(|m| m.as_str());
        let method = method?;
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => Some(self.initialize(id)),
            "initialized" => None,
            "shutdown" => {
                self.shutdown = true;
                Some(json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null }))
            }
            "exit" => {
                // LSP：shutdown 收到后 exit 退出码 0，否则 1。
                std::process::exit(if self.shutdown { 0 } else { 1 });
            }
            "textDocument/didOpen" => {
                if let (Some(uri), Some(text)) = (
                    str_field(&params, &["textDocument", "uri"]),
                    str_field(&params, &["textDocument", "text"]),
                ) {
                    self.check_and_publish(uri, text);
                }
                None
            }
            "textDocument/didChange" => {
                if let (Some(uri), Some(text)) = (
                    str_field(&params, &["textDocument", "uri"]),
                    str_field(&params, &["contentChanges", "0", "text"]),
                ) {
                    self.check_and_publish(uri, text);
                }
                None
            }
            "textDocument/didSave" => {
                if let Some(uri) = str_field(&params, &["textDocument", "uri"]) {
                    if let Some(text) = self.documents.get(uri).cloned() {
                        self.check_and_publish(uri, &text);
                    }
                }
                None
            }
            "textDocument/didClose" => {
                if let Some(uri) = str_field(&params, &["textDocument", "uri"]) {
                    self.documents.remove(uri);
                    self.push_diagnostics(uri, Vec::new());
                }
                None
            }
            "textDocument/hover" => self.hover(id, &params),
            "textDocument/completion" => self.completion(id, &params),
            "textDocument/definition" => self.definition(id, &params),
            "textDocument/formatting" => self.formatting(id, &params),
            _ => {
                // 通知无 id；请求回 method-not-found
                id.map(method_not_found)
            }
        }
    }

    fn initialize(&self, id: Option<Value>) -> Value {
        let result = InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: 1,
                hover_provider: true,
                definition_provider: true,
                document_formatting_provider: true,
                completion_provider: CompletionProvider {
                    trigger_characters: vec![".".to_string()],
                },
            },
            server_info: ServerInfo {
                name: "yoru-lsp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        };
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": serde_json::to_value(&result).unwrap(),
        })
    }

    fn check_and_publish(&mut self, uri: &str, text: &str) {
        self.documents.insert(uri.to_string(), text.to_string());
        let diags = analysis::diagnostics(text);
        self.push_diagnostics(uri, diags);
    }

    fn push_diagnostics(&mut self, uri: &str, diags: Vec<LspDiagnostic>) {
        let params = PublishDiagnosticsParams {
            uri: uri.to_string(),
            diagnostics: diags,
        };
        self.pending.push(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": serde_json::to_value(&params).unwrap(),
        }));
    }

    fn hover(&self, id: Option<Value>, params: &Value) -> Option<Value> {
        let (uri, line, character) = text_pos(params)?;
        let text = self.documents.get(uri)?;
        let offset = position_to_offset(text, line, character);
        let result = analysis::hover(text, offset)
            .map(|h| serde_json::to_value(&h).unwrap())
            .unwrap_or(Value::Null);
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    fn completion(&self, id: Option<Value>, params: &Value) -> Option<Value> {
        let (uri, line, character) = text_pos(params)?;
        let text = self.documents.get(uri)?;
        let offset = position_to_offset(text, line, character);
        let items = analysis::completions(text, offset);
        let result = serde_json::to_value(&items).unwrap();
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    fn definition(&self, id: Option<Value>, params: &Value) -> Option<Value> {
        let (uri, line, character) = text_pos(params)?;
        let text = self.documents.get(uri)?;
        let offset = position_to_offset(text, line, character);
        let result = analysis::definition(text, uri, offset)
            .map(|l| serde_json::to_value(&l).unwrap())
            .unwrap_or(Value::Null);
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    fn formatting(&self, id: Option<Value>, params: &Value) -> Option<Value> {
        let uri = str_field(params, &["textDocument", "uri"])?;
        let text = self.documents.get(uri)?;
        let result = analysis::format_edit(text)
            .map(|e| serde_json::to_value(&e).unwrap())
            .unwrap_or(Value::Null);
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }
}

// ---- 参数提取辅助 ----

fn str_field<'a>(params: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = params;
    for p in path {
        cur = if let Ok(idx) = p.parse::<usize>() {
            cur.get(idx)? // 数值段按数组下标（contentChanges[0]）
        } else {
            cur.get(p)?
        };
    }
    cur.as_str()
}

fn text_pos(params: &Value) -> Option<(&str, u32, u32)> {
    let uri = str_field(params, &["textDocument", "uri"])?;
    let pos = params.get("position")?;
    let line = pos.get("line")?.as_u64()? as u32;
    let character = pos.get("character")?.as_u64()? as u32;
    Some((uri, line, character))
}

fn method_not_found(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32601, "message": "Method not found" }
    })
}
