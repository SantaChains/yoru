//! token 空白重写器（29 号 D1 裁决的文本半边）。
//!
//! 输入 = 词法流（F0 后含 Comment 一等 token）+ 布局计划（plan.rs）+ 原文。
//! 只重排空白（换行/缩进/空格），token 文本按 span 从原文逐字节切片——注释、
//! 字符串（含插值）、字面量原文天然保真（L6）。
//!
//! 发射不变式（29 号 §2.1）：输出 re-lex 后非空白 token 序列与原流相等，且
//! AST 结构投影相等。布局铁律 L1-L7 由构造保证：
//! - 断点首 token 全为词/字面量（plan.rs）→ 永不行首运算符（L1）；
//! - 块式 `{` 后强制换行、`}` 前回退缩进（L2/L5）；
//! - 结构体声明字段、match 臂由 breaks 逐行发射（L2/L3）；
//! - 行注释/文档注释后强制行尾（自身吞到行尾；延续记号由 lexer 穿透判定承接，
//!   R2 语义保持，L7）。

use yoru_syntax::token::{Token, TokenKind};

use crate::plan::Plan;

const INDENT: &str = "    ";

pub fn rewrite(src: &str, tokens: &[Token], plan: &Plan) -> String {
    let mut w = Emitter {
        src,
        plan,
        out: String::new(),
        depth: 0,
        brace_stack: Vec::new(),
        pending_break: false,
        line_started: false,
        line_has_token: false,
        tight_next: false,
        force_space: false,
        prev_operand_end: false,
        prev_kind: None,
        newline_since_prev: false,
        last_end: 0,
    };
    for tok in tokens {
        w.emit(tok);
    }
    if !w.out.ends_with('\n') {
        w.out.push('\n');
    }
    w.out
}

struct Emitter<'a> {
    src: &'a str,
    plan: &'a Plan,
    out: String,
    depth: usize,
    brace_stack: Vec<bool>, // true = 块式
    pending_break: bool,
    line_started: bool,
    line_has_token: bool,
    tight_next: bool,
    force_space: bool,
    prev_operand_end: bool,
    prev_kind: Option<TokenKind>,
    newline_since_prev: bool,
    last_end: usize,
}

impl<'a> Emitter<'a> {
    fn indent_str(&self) -> String {
        INDENT.repeat(self.depth)
    }

    /// 原文中上一发射 token 与当前 token 之间隔了空行（≥2 个换行）。
    fn gap_blank(&self, start: usize) -> bool {
        self.src[self.last_end.min(start)..start].contains("\n\n")
    }

    fn emit(&mut self, tok: &Token) {
        match &tok.kind {
            TokenKind::Newline => {
                self.newline_since_prev = true;
                return;
            }
            TokenKind::Eof => return,
            TokenKind::Comment | TokenKind::DocComment(_) => {
                self.emit_comment(tok);
                return;
            }
            _ => {}
        }

        if self.plan.breaks.contains(&tok.span.start) {
            self.pending_break = true;
        }
        if self.pending_break {
            let blank = self.last_end > 0 && self.gap_blank(tok.span.start);
            if blank {
                self.out.push('\n'); // 空行：line_started 与否各补一行
            }
            if self.line_started {
                self.out.push('\n');
                self.line_started = false;
            }
            self.out.push_str(&self.indent_str());
            self.pending_break = false;
            self.line_started = true;
            self.line_has_token = false;
        } else if !self.line_started {
            // 行注释强制换行后的延续内容：按当前深度缩进续行（L7）
            self.out.push_str(&self.indent_str());
            self.line_started = true;
            self.line_has_token = false;
        }

        let need_sp = self.line_has_token
            && !self.tight_next
            && (self.force_space
                || need_space(self.prev_kind.as_ref(), &tok.kind, self.prev_operand_end));
        self.force_space = false;
        self.tight_next = false;
        self.last_end = tok.span.end;
        self.newline_since_prev = false;

        let operand_end = is_operand_end(&tok.kind);
        match &tok.kind {
            TokenKind::LBrace => {
                let block = self.plan.block_opens.contains(&tok.span.start);
                if need_sp {
                    self.out.push(' ');
                }
                self.out.push('{');
                self.line_started = true;
                self.line_has_token = true;
                self.brace_stack.push(block);
                if block {
                    self.depth += 1;
                    self.out.push('\n');
                    self.line_started = false;
                    self.line_has_token = false;
                }
            }
            TokenKind::RBrace => {
                let block = self.brace_stack.pop().unwrap_or(false);
                if block {
                    self.depth = self.depth.saturating_sub(1);
                    self.out.push('\n');
                    self.out.push_str(&self.indent_str());
                    self.out.push('}');
                    self.line_started = true;
                } else {
                    if need_sp {
                        self.out.push(' ');
                    }
                    self.out.push('}');
                    self.line_started = true;
                }
                self.line_has_token = true;
            }
            _ => {
                if need_sp {
                    self.out.push(' ');
                }
                self.out.push_str(&self.src[tok.span.start..tok.span.end]);
                self.line_started = true;
                self.line_has_token = true;
            }
        }

        match &tok.kind {
            TokenKind::Minus => {
                if !self.prev_operand_end {
                    self.tight_next = true; // 一元负号：右侧紧排
                }
            }
            TokenKind::Bang => {
                if !self.prev_operand_end {
                    self.tight_next = true; // 前缀 `!`：右侧紧排；后缀 `!` 右侧正常
                }
            }
            TokenKind::DotDot => {
                self.tight_next = true;
            }
            _ => {}
        }
        self.prev_operand_end = operand_end;
        self.prev_kind = Some(tok.kind.clone());
    }

    fn emit_comment(&mut self, tok: &Token) {
        let text = &self.src[tok.span.start..tok.span.end];
        let line_form = self.src[tok.span.start..].starts_with("//");
        // 独立行判定：原文中前面有换行（注释行归属作者意图，go 同款）
        if self.newline_since_prev || !self.line_started {
            let blank = self.last_end > 0 && self.gap_blank(tok.span.start);
            if blank {
                self.out.push('\n');
            }
            if self.line_started {
                self.out.push('\n');
                self.line_started = false;
            }
            self.out.push_str(&self.indent_str());
            self.out.push_str(text);
            self.out.push('\n');
            self.line_started = false;
            self.line_has_token = false;
        } else {
            // 行尾/行内注释
            self.out.push(' ');
            self.out.push_str(text);
            if line_form {
                self.out.push('\n');
                self.line_started = false;
                self.line_has_token = false;
            } else {
                self.force_space = true; // 行内块注释：右侧补空格
                self.line_has_token = true;
            }
        }
        self.last_end = tok.span.end;
        self.newline_since_prev = false;
        // 注释不参与运算符间距判定
        self.prev_kind = None;
        self.prev_operand_end = false;
    }
}

fn is_operand_end(k: &TokenKind) -> bool {
    matches!(
        k,
        TokenKind::Int(_)
            | TokenKind::Float(_)
            | TokenKind::Str(_)
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Nil
            | TokenKind::Ident(_)
            | TokenKind::RParen
            | TokenKind::RBracket
            | TokenKind::Question
            | TokenKind::Bang
    )
}

/// token 间距表（行内相邻两 token 之间是否空一格）。
/// 紧排例外显式列出，默认空格——`} else {` / `try f` 等词序列天然正确。
/// 一元 `-` / 前缀 `!` 的右侧紧排由发射器 tight_next 承担（间距表只管左侧）。
fn need_space(prev: Option<&TokenKind>, cur: &TokenKind, prev_operand_end: bool) -> bool {
    let Some(p) = prev else {
        return false;
    };
    use TokenKind::*;
    // 当前 token 右侧贴靠（后缀位）
    if matches!(cur, Comma | RParen | RBracket | Question | Colon | Dot) {
        return false;
    }
    if matches!(cur, Bang) {
        // 后缀 `o!` 紧靠（操作数后）；前缀 `!flag` 与前置记号留空格
        // （前缀右侧紧排由发射器 tight_next 承担）
        return !prev_operand_end;
    }
    if matches!(cur, DotDot) {
        return !prev_operand_end; // 区间 `a..b` 紧；展开 `{ ..p,` 空格
    }
    // 前一 token 左侧贴靠（前缀/开括号位）
    if matches!(p, LParen | LBracket | Dot | DotDot) {
        return false;
    }
    if matches!(p, Comma | Colon) {
        return true;
    }
    if matches!(cur, LBrace) {
        return true;
    }
    if matches!(cur, LParen | LBracket) {
        // 调用/索引紧靠操作数；`fn(` 紧靠关键字；其余（`= (`）空格
        return !(prev_operand_end || matches!(p, Fn));
    }
    true
}
