//! yoru-fmt：Yoru 源码格式化核心（F1，29 号裁决架构）。
//!
//! 管线（29 号 §6）：`src → lex（含 Comment）→ token 流` 与 `src → parse → AST`
//! 双路输入 → token 空白重写器（AST 区间指导布局）→ `src'`。
//! token 文本按 span 逐字节切片——注释/字符串/字面量天然保真；只重排空白。
//!
//! 验收协议（29 号 D3 双断言 + 幂等，[`verify`] 实现）：
//! 1. token 序列保真：输出 re-lex 后非空白 token 的 `(kind, 原文切片)` 逐项相等；
//! 2. 结构投影保真：span-blind 投影打印器前后相等；
//! 3. 幂等：`fmt(fmt(x)) == fmt(x)` 字节级。
//!
//! CLI（`yoru fmt` / `--check`）属 F2。

mod plan;
mod projector;
mod rewrite;

use yoru_syntax::{Diagnostic, Token, TokenKind};

pub use plan::Plan;
pub use projector::project;

/// 格式化源码。解析失败返回诊断（fmt 只对合法文件负责）。
pub fn format(src: &str) -> Result<String, Vec<Diagnostic>> {
    let (tokens, diags) = yoru_syntax::tokenize(src);
    if diags.iter().any(|d| d.is_error()) {
        return Err(diags);
    }
    let prog = yoru_syntax::parser::parse(&tokens)?;
    let plan = plan::build_plan(&prog, &tokens);
    Ok(rewrite::rewrite(src, &tokens, &plan))
}

/// 双断言 + 幂等验证结果（29 号 D3 验收协议的机械化形态）。
#[derive(Debug, PartialEq, Eq)]
pub struct Fidelity {
    pub tokens_equal: bool,
    pub projection_equal: bool,
    pub idempotent: bool,
}

impl Fidelity {
    pub fn ok(&self) -> bool {
        self.tokens_equal && self.projection_equal && self.idempotent
    }
}

/// 非空白 token 的可比键：kind（span-blind 全值语义）+ 原文切片
/// （Comment 是 unit 变体、字面量可能同值异文，切片兜底逐字节保真）。
///
/// span-blind 例外修正：`StrPart::Interp(Vec<Token>)` 内嵌带 span 的 token
/// （29 号 D3「kind 天然 span-blind」的插值盲区）——此处递归把内嵌 span 归零。
fn token_key(tok: &Token, src: &str) -> (TokenKind, String) {
    (
        strip_spans(&tok.kind),
        src[tok.span.start..tok.span.end].to_owned(),
    )
}

fn strip_spans(k: &TokenKind) -> TokenKind {
    match k {
        TokenKind::Str(parts) => TokenKind::Str(
            parts
                .iter()
                .map(|p| match p {
                    yoru_syntax::StrPart::Lit(s) => yoru_syntax::StrPart::Lit(s.clone()),
                    yoru_syntax::StrPart::Interp(ts) => yoru_syntax::StrPart::Interp(
                        ts.iter()
                            .map(|t| Token {
                                kind: strip_spans(&t.kind),
                                span: yoru_syntax::Span::new(0, 0),
                            })
                            .collect(),
                    ),
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// 对「原文 → 格式化输出」执行 29 号 D3 双断言 + 幂等。
pub fn verify(src: &str, out: &str) -> Result<Fidelity, Vec<Diagnostic>> {
    let out2 = format(out)?;
    let (t1, d1) = yoru_syntax::tokenize(src);
    let (t2, d2) = yoru_syntax::tokenize(out);
    if d1.iter().any(|d| d.is_error()) || d2.iter().any(|d| d.is_error()) {
        return Err(if d1.iter().any(|d| d.is_error()) {
            d1
        } else {
            d2
        });
    }
    let seq = |toks: &[Token], s: &str| -> Vec<(TokenKind, String)> {
        toks.iter()
            .filter(|t| !matches!(t.kind, TokenKind::Newline | TokenKind::Eof))
            .map(|t| token_key(t, s))
            .collect()
    };
    let p1 = yoru_syntax::parser::parse(&t1).map(|p| project(&p));
    let p2 = yoru_syntax::parser::parse(&t2).map(|p| project(&p));
    let projection_equal = matches!((&p1, &p2), (Ok(a), Ok(b)) if a == b);
    Ok(Fidelity {
        tokens_equal: seq(&t1, src) == seq(&t2, out),
        projection_equal,
        idempotent: out == out2,
    })
}

#[cfg(test)]
mod tests;
