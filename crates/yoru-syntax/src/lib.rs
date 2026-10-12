//! yoru-syntax：Yoru 语言前端（词法与语法，唯一输入 = 设计 19 号语法规范 v2）。
//!
//! 模块划分：
//! - `diagnostic`：结构化诊断（人 + AI 双端，JSON 载荷）
//! - `token`：Token 定义（19.1 词法面）
//! - `lexer`：源码 → Token 流（换行终止 / 插值切分 / Bidi 拒绝）
//! - `ast`：语法树（19.2 EBNF v2 的 Rust 表示）
//! - `parser`：递归下降解析（条件位受限文法 + 语句级错误恢复）

pub mod ast;
pub mod diagnostic;
pub mod lexer;
pub mod parser;
pub mod token;

pub use ast::*;
pub use diagnostic::{Diagnostic, Severity, Span};
pub use token::{StrPart, Token, TokenKind};

/// 便捷入口：源码 → (Token 流, 诊断)。
pub fn tokenize(src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    lexer::lex(src)
}

/// 便捷入口：源码 → Program（解析失败返回诊断）。
/// 解析入口。递归下降在 16MB 大栈线程上执行——rustc 同款策略：
/// Windows 主线程栈仅 1MB，实测 ~56 层深嵌套即爆原生栈；
/// 大栈 + 双闸（MAX_NEST_DEPTH 递归深度闸 / MAX_CHAIN_LEN 迭代链长闸）
/// 三重保险，平台栈尺寸差异不再影响行为。
pub fn parse_source(src: &str) -> Result<Program, Vec<Diagnostic>> {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(s, || parse_source_inner(src))
            .expect("spawn parse thread")
            .join()
            .unwrap_or_else(|p| std::panic::resume_unwind(p))
    })
}

fn parse_source_inner(src: &str) -> Result<Program, Vec<Diagnostic>> {
    let (tokens, lex_diags) = lexer::lex(src);
    if let Some(err) = lex_diags.iter().find(|d| d.is_error()) {
        return Err(vec![err.clone()]);
    }
    parser::parse(&tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newline_terminates_statements() {
        let src = "fn f() {\nlet x = 1\nlet y = 2\n}\n";
        assert!(parse_source(src).is_ok(), "{:?}", parse_source(src));
    }

    #[test]
    fn line_continues_after_binary_op() {
        let src = "fn f() {\nlet x = 1 +\n    2\n}\n";
        assert!(parse_source(src).is_ok(), "{:?}", parse_source(src));
    }

    #[test]
    fn pipeline_and_lambda() {
        let src = "fn f(items: [Item]) {\nlet t = items |> map(fn(it) = it.size) |> fold(0, fn(a, b) = a + b)\n}\n";
        assert!(parse_source(src).is_ok(), "{:?}", parse_source(src));
    }

    #[test]
    fn interpolation_parses_inner_expr() {
        let src = "fn f() {\nlet s = \"a ${x + 1} b\"\n}\n";
        assert!(parse_source(src).is_ok(), "{:?}", parse_source(src));
    }

    #[test]
    fn interp_trailing_bang_no_spin() {
        // D-02（26 号登记）：插值子解析器切片无 Eof 哨兵，`${o!}` 以 `!` 结尾
        // 曾使 postfix 循环死自旋直至 Y-S9502。
        let src = "fn f() {\nlet o: int? = 1\nlet s = \"${o!}\"\n}\n";
        assert!(parse_source(src).is_ok(), "{:?}", parse_source(src));
    }

    #[test]
    fn struct_update_and_match() {
        let src = concat!(
            "tag E = A | B(string)\n",
            "fn f(e: E) -> int {\n",
            "    match e {\n",
            "        A -> 0\n",
            "        B(s) -> len(s)\n",
            "    }\n",
            "}\n",
        );
        assert!(parse_source(src).is_ok(), "{:?}", parse_source(src));
    }

    // ---- T2 硬化：深嵌套防原生栈溢出 ----

    #[test]
    fn deep_nesting_rejected_not_crash() {
        // 括号 10k 层：500 深度闸内拒绝，进程不崩（无闸时实测栈溢出）
        let mut src = String::from("pub fn main() -> int {\n    return ");
        src.push_str(&"(".repeat(10_000));
        src.push('1');
        src.push_str(&")".repeat(10_000));
        src.push_str("\n}\n");
        let ds = parse_source(&src).unwrap_err();
        assert!(ds.iter().any(|d| d.code.as_deref() == Some("Y-S9501")));

        // 深块嵌套同样受闸
        let mut src2 = String::from("pub fn main() {\n");
        for _ in 0..2000 {
            src2.push_str("    if true {\n");
        }
        for _ in 0..2000 {
            src2.push_str("    }\n");
        }
        let ds2 = parse_source(&src2).unwrap_err();
        assert!(ds2.iter().any(|d| d.code.as_deref() == Some("Y-S9501")));
    }

    #[test]
    fn long_operator_chain_rejected_not_crash() {
        // 50k 项加法链是**合法语法**：循环迭代包装 Binary，递归深度闸不可见。
        // 无链长闸时 parse 成功、无诊断，随后 drop/typeck/lower 递归爆栈
        // （T2 硬化实测的 drop 崩溃根因，与深嵌套路径互补）。
        let mut src = String::from("pub fn main() -> int {\n    return 0");
        for _ in 0..50_000 {
            src.push_str(" + 1");
        }
        src.push_str("\n}\n");
        let ds = parse_source(&src).unwrap_err();
        assert!(ds.iter().any(|d| d.code.as_deref() == Some("Y-S9502")));

        // 正常长度的链（100 项）不受影响
        let mut ok = String::from("pub fn main() -> int {\n    return 0");
        for _ in 0..100 {
            ok.push_str(" + 1");
        }
        ok.push_str("\n}\n");
        assert!(parse_source(&ok).is_ok(), "{:?}", parse_source(&ok).err());
    }

    #[test]
    fn banned_bidi_char_is_error() {
        let src = "let x = 1 \u{202E}\n";
        assert!(parse_source(src).is_err());
    }

    #[test]
    fn no_semicolon_allowed() {
        // 分号不是 Yoru 的记号：作为非法字符报错（19.1 记号集封闭）
        let src = "let x = 1;\n";
        assert!(parse_source(src).is_err());
    }

    #[test]
    fn full_authoritative_sample() {
        let src = include_str!("../examples/authoritative.yoru");
        match parse_source(src) {
            Ok(_) => {}
            Err(diags) => panic!("authoritative sample failed: {diags:#?}"),
        }
    }
}
