//! yoru-lsp：Yoru 语言服务器（序 9，28 号 §2.4 / 22 号决策 73）。
//!
//! 独立 headless 进程，stdio JSON-RPC；复用 yoru-syntax / yoru-typeck /
//! yoru-fmt / yoru-stdlib，不倒灌语言服务。当前能力：
//! - 诊断（push 模型，typeck 全量重算）
//! - hover（内建文档+签名 / 用户声明签名）
//! - completion（注册表 + 用户声明 + 关键字；支持 `module.` 上下文）
//! - definition（用户声明跳转）
//! - formatting（复用 F 线 yoru_fmt）

pub mod analysis;
pub mod lsp;
pub mod server;
pub mod transport;

pub use server::Server;
pub use transport::run_stdio;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis;
    use crate::lsp::{Position, offset_to_position, position_to_offset};
    use serde_json::Value;

    #[test]
    fn offset_position_roundtrip_ascii() {
        let src = "abc\ndef";
        assert_eq!(
            offset_to_position(src, 0),
            Position {
                line: 0,
                character: 0
            }
        );
        assert_eq!(
            offset_to_position(src, 4),
            Position {
                line: 1,
                character: 0
            }
        );
        assert_eq!(
            offset_to_position(src, 5),
            Position {
                line: 1,
                character: 1
            }
        );
        assert_eq!(position_to_offset(src, 1, 0), 4);
        assert_eq!(position_to_offset(src, 0, 3), 3);
    }

    #[test]
    fn offset_position_utf16() {
        // 'é' 占 2 字节、1 个 UTF-16 码元；'中' 占 3 字节、1 码元。
        let src = "aé中";
        assert_eq!(
            offset_to_position(src, 1),
            Position {
                line: 0,
                character: 1
            }
        );
        assert_eq!(
            offset_to_position(src, 3),
            Position {
                line: 0,
                character: 2
            }
        ); // é 后：列=2（a+é）
        assert_eq!(
            offset_to_position(src, 6),
            Position {
                line: 0,
                character: 3
            }
        ); // 中 后：列=3
        assert_eq!(position_to_offset(src, 0, 3), 6);
    }

    #[test]
    fn diagnostics_maps_type_error() {
        // int * float 须触发 Y-T0003
        let src = "fn f(a: int) -> float {\n    return a * 0.5\n}\n";
        let diags = analysis::diagnostics(src);
        assert!(!diags.is_empty(), "应产生诊断");
        assert!(
            diags.iter().any(|d| d.code.as_deref() == Some("Y-T0003")),
            "须含 Y-T0003: {diags:#?}"
        );
        // 位置落在该行
        let d = diags
            .iter()
            .find(|d| d.code.as_deref() == Some("Y-T0003"))
            .unwrap();
        assert_eq!(d.range.start.line, 1);
    }

    #[test]
    fn hover_builtin_doc() {
        let src = "fn main() {\n    let n = len(\"abc\")\n}\n";
        // 定位 `len` 标识符起始偏移
        let off = src.find("len").unwrap();
        let h = analysis::hover(src, off).expect("hover 应命中内建");
        assert!(h.contents.value.contains("len"), "{}", h.contents.value);
        assert!(h.contents.value.contains("内建"));
    }

    #[test]
    fn hover_module_qualified() {
        let src = "import math from \"std\"\nfn f() -> float {\n    return math.sqrt(2.0)\n}\n";
        let off = src.find("sqrt").unwrap();
        let h = analysis::hover(src, off).expect("hover 应命中模块内建");
        assert!(h.contents.value.contains("sqrt"));
        assert!(h.contents.value.contains("math.sqrt"));
    }

    #[test]
    fn completions_include_builtins_and_keywords() {
        let src = "fn main() {\n    let x = ma\n}\n";
        let off = src.find("ma").unwrap() + "ma".len();
        let items = analysis::completions(src, off);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"map"), "应含 map: {labels:?}");
        assert!(
            labels.contains(&"math"),
            "应含 math（模块限定）: {labels:?}"
        );
        assert!(labels.contains(&"match"), "应含 match 关键字: {labels:?}");
    }

    #[test]
    fn completions_module_context_lists_methods() {
        // `math.` 后仅列 math 模块方法，且为裸名
        let src = "import math from \"std\"\nfn f() {\n    let x = math.\n}\n";
        let off = src.find("math.").unwrap() + "math.".len();
        let items = analysis::completions(src, off);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"sqrt"), "应含 sqrt: {labels:?}");
        assert!(
            !labels.iter().any(|l| l.contains('.')),
            "模块上下文应为裸名: {labels:?}"
        );
    }

    #[test]
    fn definition_jumps_to_user_decl() {
        let src = "fn helper() -> int {\n    return 1\n}\nfn main() {\n    let x = helper()\n}\n";
        // main 中 helper() 的调用位置
        let call_off = src.rfind("helper").unwrap();
        let loc = analysis::definition(src, "file:///t.yoru", call_off).expect("应跳转");
        assert_eq!(loc.range.start.line, 0, "声明在首行");
        assert_eq!(loc.uri, "file:///t.yoru");
    }

    #[test]
    fn format_edit_produced() {
        let src = "fn f(){\n    let x=1\n}\n";
        let edits = analysis::format_edit(src).expect("应产出格式化编辑");
        assert_eq!(edits.len(), 1);
        assert!(edits[0].new_text.contains("fn f() {")); // 规范化空格
        assert!(edits[0].new_text.contains("let x = 1"));
    }

    #[test]
    fn format_edit_none_on_parse_error() {
        let src = "fn f( {\n"; // 语法错误
        assert!(analysis::format_edit(src).is_none());
    }

    #[test]
    fn initialize_advertises_capabilities() {
        let mut s = Server::new();
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });
        let resp = s.handle(&req).expect("initialize 须有响应");
        assert_eq!(resp["result"]["capabilities"]["hoverProvider"], true);
        assert_eq!(resp["result"]["capabilities"]["definitionProvider"], true);
        assert_eq!(
            resp["result"]["capabilities"]["documentFormattingProvider"],
            true
        );
        assert_eq!(
            resp["result"]["capabilities"]["completionProvider"]["triggerCharacters"][0],
            "."
        );
    }

    #[test]
    fn did_open_publishes_diagnostics() {
        let mut s = Server::new();
        let note = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///x.yoru","languageId":"yoru","version":1,"text":"fn f(a: int) -> float {\n    return a * 0.5\n}\n"}}}"#;
        let msg: Value = serde_json::from_str(note).unwrap();
        let resp = s.handle(&msg);
        assert!(resp.is_none(), "通知无响应");
        let pending = s.drain_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["method"], "textDocument/publishDiagnostics");
        let diags = &pending[0]["params"]["diagnostics"];
        assert!(
            diags
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["code"] == "Y-T0003")
        );
        assert!(s.documents.contains_key("file:///x.yoru"));
    }

    #[test]
    fn unknown_request_is_method_not_found() {
        let mut s = Server::new();
        let req = serde_json::json!({"jsonrpc":"2.0","id":7,"method":"foobar","params":{}});
        let resp = s.handle(&req).expect("请求须有响应");
        assert_eq!(resp["error"]["code"], -32601);
    }
}
