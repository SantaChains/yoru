//! yoru-lsp 独立二进制：stdio JSON-RPC 语言服务器入口。

fn main() {
    // 协议消息独占 stdout；其余诊断走 stderr（transport 约定）。
    if let Err(e) = yoru_lsp::run_stdio() {
        eprintln!("yoru-lsp: transport error: {e}");
        std::process::exit(1);
    }
}
