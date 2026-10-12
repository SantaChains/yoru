//! lexer 单独探针。
use yoru_syntax::lexer;

fn main() {
    let depth: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(90_000);
    let mut src = String::from("pub fn main() -> int {\n    return ");
    src.push_str(&"(".repeat(depth));
    src.push('1');
    src.push_str(&")".repeat(depth));
    src.push_str("\n}\n");
    let (tokens, diags) = lexer::lex(&src);
    println!("tokens={} diags={}", tokens.len(), diags.len());
}
