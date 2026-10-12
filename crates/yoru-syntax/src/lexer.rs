//! Lexer：源码 → Token 流（19 号语法规范 v2，19.1 词法规则）。
//!
//! 要点：
//! - **换行即语句终止**：产出 `Newline` token；括号 `(`/`[` 内、或行尾为延续记号
//!   （二元运算符/逗号/`..`/`->`/`else` 等）时抑制——确定性规则，无 ASI 式猜测（19.1）。
//!   `{` 不计入抑制深度：block 与 struct 体的字段/语句分隔依赖换行（19.2 struct_decl）。
//! - **插值**：字符串内 `${expr}` 在词法期递归切分为 `StrPart::Interp(token 序列)`；
//!   插值内禁止换行（19.1）。
//! - **源码卫生（决策 51）**：拒绝 Bidi 方向控制字符与零宽字符（含字符串/注释内部），
//!   报错不警告；标识符 ASCII-only。
//! - 错误恢复：非法字符跳过并记录诊断，不中断整个文件。

use crate::diagnostic::{Diagnostic, Span};
use crate::token::{StrPart, Token, TokenKind};

/// 双字符运算符辅助：当前字符已消费，尝试匹配 `second`。
macro_rules! two {
    ($self:ident, $second:expr_2021, $matched:expr_2021, $single:expr_2021) => {{
        if $self.peek_char() == Some($second) {
            $self.pos += 1;
            $matched
        } else {
            $single
        }
    }};
}

pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    diagnostics: Vec<Diagnostic>,
    /// 产出 token 流（用于行尾延续判定）。
    out: Vec<Token>,
    /// `(`/`[` 深度：>0 时换行抑制（19.1 未闭合括号延续；`{` 不计，block/struct 靠换行分隔）。
    depth: usize,
}

pub fn lex(src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let mut lx = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        diagnostics: Vec::new(),
        out: Vec::new(),
        depth: 0,
    };
    lx.run();
    (lx.out, lx.diagnostics)
}/// Bidi 方向控制字符（CVE-2021-42574）与零宽字符（CVE-2021-42694 同族）。
fn is_banned_char(c: char) -> bool {
    matches!(c,
        '\u{202A}'..='\u{202E}'   // LRE/LRO/RLE/RLO/PDF
        | '\u{2066}'..='\u{2069}' // LRI/RLI/FSI/PDI
        | '\u{061C}'              // ALM
        | '\u{200E}' | '\u{200F}' // LRM/RLM
        | '\u{200B}'..='\u{200D}' // ZWSP/ZWNJ/ZWJ
        | '\u{2060}' | '\u{FEFF}' // WJ/BOM
    )
}

impl Lexer<'_> {
    fn run(&mut self) {
        while self.pos < self.bytes.len() {
            let c = match self.next_char() {
                Some(c) => c,
                None => break,
            };
            if is_banned_char(c) {
                self.error("banned Unicode control character (Trojan Source guard, decision 51)");
                continue;
            }
            match c {
                ' ' | '\t' | '\r' => {}
                '\n' => self.emit_newline(),
                '/' => self.slash(),
                '"' => self.string(),
                '0'..='9' => self.number(),
                'A'..='Z' | 'a'..='z' | '_' => self.ident(c),
                _ => self.punct(c),
            }
        }
        // EOF 哨兵：parser 的 peek/bump 在末位不前进，各循环以 Eof 收敛（防死循环）
        let end = self.src.len();
        self.out.push(Token {
            kind: TokenKind::Eof,
            span: Span::new(end, end),
        });
    }

    // ---- 基础设施 ----

    fn next_char(&mut self) -> Option<char> {
        let s = &self.src[self.pos..];
        let c = s.chars().next()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn peek_char(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn error(&mut self, msg: impl Into<String>) {
        let sp = Span::new(self.pos.saturating_sub(1), self.pos);
        self.diagnostics.push(Diagnostic::error(msg, sp));
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.out.push(Token {
            kind,
            span: Span::new(start, self.pos),
        });
    }

    /// 换行产出判定：括号内抑制；行尾为延续记号时抑制。
    fn emit_newline(&mut self) {
        if self.depth > 0 {
            return; // 未闭合括号内语句延续（19.1）
        }
        if self.out.last().is_none() {
            return; // 文件头空行
        }
        if let Some(last) = self.out.last() {
            if matches!(last.kind, TokenKind::Newline | TokenKind::LBrace) {
                return; // 连续空行 / block 起始行
            }
            if last.kind.continues_line() {
                return; // 行尾延续（19.1 确定性规则）
            }
        }
        let sp = Span::new(self.pos - 1, self.pos);
        self.out.push(Token {
            kind: TokenKind::Newline,
            span: sp,
        });
    }

    // ---- 分类处理 ----

    fn slash(&mut self) {
        let start = self.pos - 1;
        match self.peek_char() {
            Some('/') => {
                self.pos += 1;
                let doc = self.peek_char() == Some('/');
                if doc {
                    self.pos += 1;
                }
                let text_start = self.pos;
                while let Some(c) = self.peek_char() {
                    if c == '\n' {
                        break;
                    }
                    if is_banned_char(c) {
                        self.error("banned Unicode control character in comment (decision 51)");
                    }
                    self.pos += c.len_utf8();
                }
                if doc {
                    let text = self.src[text_start..self.pos].trim_end().to_string();
                    self.push(TokenKind::DocComment(text), start);
                }
            }
            Some('*') => {
                self.pos += 1;
                let mut closed = false;
                while self.pos < self.bytes.len() {
                    let c = match self.next_char() {
                        Some(c) => c,
                        None => break,
                    };
                    if is_banned_char(c) {
                        self.error("banned Unicode control character in comment (decision 51)");
                    }
                    if c == '*' && self.peek_char() == Some('/') {
                        self.pos += 1;
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    let sp = Span::new(start, self.pos);
                    self.diagnostics.push(
                        Diagnostic::error("unterminated block comment", sp)
                            .with_suggestion("add closing `*/`"),
                    );
                }
            }
            _ => self.push(TokenKind::Slash, start),
        }
    }

    /// 字符串：转义 + 插值切分。插值内换行 = 错误（19.1）。
    fn string(&mut self) {
        let start = self.pos - 1;
        let mut parts: Vec<StrPart> = Vec::new();
        let mut lit = String::new();
        loop {
            let c = match self.peek_char() {
                Some(c) => c,
                None => {
                    let sp = Span::new(start, self.pos);
                    self.diagnostics.push(
                        Diagnostic::error("unterminated string literal", sp)
                            .with_suggestion("add closing `\"`"),
                    );
                    break;
                }
            };
            self.pos += c.len_utf8();
            match c {
                '"' => break,
                '\n' => {
                    let sp = Span::new(start, self.pos);
                    self.diagnostics.push(
                        Diagnostic::error("newline inside string literal", sp)
                            .with_suggestion("use \\n escape"),
                    );
                    break;
                }
                '\\' => {
                    let esc = self.peek_char();
                    self.pos += esc.map_or(0, |c| c.len_utf8());
                    let ch = match esc {
                        Some('n') => '\n',
                        Some('r') => '\r',
                        Some('t') => '\t',
                        Some('0') => '\0',
                        Some('\\') => '\\',
                        Some('"') => '"',
                        Some('$') => '$',
                        Some(other) => {
                            let sp = Span::new(self.pos - 1, self.pos);
                            self.diagnostics.push(
                                Diagnostic::error(format!("unknown escape `\\{other}`"), sp)
                                    .with_suggestion("supported: \\n \\r \\t \\0 \\\\ \\\" \\$"),
                            );
                            other
                        }
                        None => {
                            let sp = Span::new(self.pos, self.pos);
                            self.diagnostics
                                .push(Diagnostic::error("unexpected end after `\\`", sp));
                            break;
                        }
                    };
                    lit.push(ch);
                }
                '$' if self.peek_char() == Some('{') => {
                    self.pos += 1; // consume '{'
                    if !lit.is_empty() {
                        parts.push(StrPart::Lit(std::mem::take(&mut lit)));
                    }
                    match self.interp_body(start) {
                        Some(tokens) => parts.push(StrPart::Interp(tokens)),
                        None => break, // 未闭合插值已报错，终止字符串
                    }
                }
                other => {
                    if is_banned_char(other) {
                        self.error("banned Unicode control character in string (decision 51)");
                    }
                    lit.push(other);
                }
            }
        }
        if !lit.is_empty() || parts.is_empty() {
            parts.push(StrPart::Lit(lit));
        }
        self.push(TokenKind::Str(parts), start);
    }

    /// 插值体：递归词法 `${...}` 至匹配 `}`（跟踪嵌套深度）。返回 token 序列。
    fn interp_body(&mut self, str_start: usize) -> Option<Vec<Token>> {
        let mut sub = Lexer {
            src: self.src,
            bytes: self.bytes,
            pos: self.pos,
            diagnostics: Vec::new(),
            out: Vec::new(),
            depth: 1, // 进入插值的 `{`
        };
        let mut depth = 1usize;
        while sub.pos < sub.bytes.len() {
            let c = match sub.next_char() {
                Some(c) => c,
                None => break,
            };
            if is_banned_char(c) {
                sub.error("banned Unicode control character (decision 51)");
                continue;
            }
            match c {
                '\n' => {
                    let sp = Span::new(str_start, sub.pos);
                    self.diagnostics.push(
                        Diagnostic::error("newline inside string interpolation", sp)
                            .with_suggestion("interpolation must stay on one line (19.1)"),
                    );
                    self.pos = sub.pos;
                    return None;
                }
                '"' => {
                    sub.string();
                }
                '/' => sub.slash(),
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        self.pos = sub.pos;
                        self.diagnostics.append(&mut sub.diagnostics);
                        // 注意：插值 token 进 StrPart，绝不并入主流 token 流
                        return Some(std::mem::take(&mut sub.out));
                    }
                }
                ' ' | '\t' | '\r' => {}
                '0'..='9' => sub.number(),
                'A'..='Z' | 'a'..='z' | '_' => sub.ident(c),
                _ => sub.punct(c),
            }
        }
        let sp = Span::new(str_start, sub.pos);
        self.diagnostics.push(
            Diagnostic::error("unterminated interpolation `${...}`", sp)
                .with_suggestion("add closing `}`"),
        );
        self.pos = sub.pos;
        None
    }

    /// 数字：int（`0x` 十六进制 / 十进制含 `_` 分隔）或 float（`.` + 指数）。
    fn number(&mut self) {
        let start = self.pos - 1;
        let hex = self.src[start..].starts_with("0x") || self.src[start..].starts_with("0X");
        if hex {
            self.pos += 1; // 越过 x
            let ds = self.pos;
            while matches!(self.peek_char(), Some(c) if c.is_ascii_hexdigit() || c == '_') {
                self.pos += 1;
            }
            let raw: String = self.src[ds..self.pos].chars().filter(|c| *c != '_').collect();
            if raw.is_empty() {
                let sp = Span::new(start, self.pos);
                self.diagnostics.push(
                    Diagnostic::error("hex literal requires at least one digit", sp)
                        .with_suggestion("e.g. 0xFF"),
                );
                self.push(TokenKind::Int(0), start);
                return;
            }
            match i64::from_str_radix(&raw, 16) {
                Ok(v) => self.push(TokenKind::Int(v), start),
                Err(_) => {
                    let sp = Span::new(start, self.pos);
                    self.diagnostics
                        .push(Diagnostic::error("hex literal overflows i64", sp));
                    self.push(TokenKind::Int(0), start);
                }
            }
            return;
        }
        // 十进制整数部分
        while matches!(self.peek_char(), Some(c) if c.is_ascii_digit() || c == '_') {
            self.pos += 1;
        }
        // float：`.` 后必须跟数字（19.1 float 文法；`1.foo` 非法，`x.len()` 不受影响）
        if self.peek_char() == Some('.') {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
                while matches!(self.peek_char(), Some(c) if c.is_ascii_digit() || c == '_') {
                    self.pos += 1;
                }
                // 指数
                if matches!(self.peek_char(), Some('e') | Some('E')) {
                    let esave = self.pos;
                    self.pos += 1;
                    if matches!(self.peek_char(), Some('+') | Some('-')) {
                        self.pos += 1;
                    }
                    if matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
                        while matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
                            self.pos += 1;
                        }
                    } else {
                        self.pos = esave; // 不是指数（如 `1.5e` 后无数字——回退，后面会报 ident 紧邻）
                    }
                }
                let raw: String = self.src[start..self.pos]
                    .chars()
                    .filter(|c| *c != '_')
                    .collect();
                match raw.parse::<f64>() {
                    Ok(v) => {
                        self.push(TokenKind::Float(v), start);
                        return;
                    }
                    Err(_) => {
                        let sp = Span::new(start, self.pos);
                        self.diagnostics
                            .push(Diagnostic::error("invalid float literal", sp));
                        self.push(TokenKind::Float(0.0), start);
                        return;
                    }
                }
            }
            self.pos = save; // `1.` 后非数字 → 按整数处理（parser 会对 `1 .x` 报错）
        }
        let raw: String = self.src[start..self.pos]
            .chars()
            .filter(|c| *c != '_')
            .collect();
        match raw.parse::<i64>() {
            Ok(v) => self.push(TokenKind::Int(v), start),
            Err(_) => {
                let sp = Span::new(start, self.pos);
                self.diagnostics
                    .push(Diagnostic::error("integer literal overflows i64", sp));
                self.push(TokenKind::Int(0), start);
            }
        }
    }

    /// 标识符/关键字：ASCII-only（决策 51）。查表区分关键字。
    fn ident(&mut self, first: char) {
        let start = self.pos - first.len_utf8();
        while matches!(self.peek_char(), Some(c) if c.is_ascii_alphanumeric() || c == '_') {
            self.pos += 1;
        }
        // 非 ASCII 字母紧随其后 = 非法标识符
        if let Some(c) = self.peek_char() {
            if !c.is_ascii() && c.is_alphabetic() {
                let sp = Span::new(start, self.pos + c.len_utf8());
                self.diagnostics.push(
                    Diagnostic::error("identifiers must be ASCII-only (decision 51)", sp)
                        .with_suggestion("use ASCII letters, digits and `_`"),
                );
                self.pos += c.len_utf8();
                self.push(TokenKind::Ident(String::new()), start);
                return;
            }
        }
        let name = &self.src[start..self.pos];
        let kind = match name {
            "let" => TokenKind::Let,
            "mut" => TokenKind::Mut,
            "fn" => TokenKind::Fn,
            "return" => TokenKind::Return,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "while" => TokenKind::While,
            "for" => TokenKind::For,
            "in" => TokenKind::In,
            "match" => TokenKind::Match,
            "struct" => TokenKind::Struct,
            "tag" => TokenKind::Tag,
            "protocol" => TokenKind::Protocol,
            "impl" => TokenKind::Impl,
            "type" => TokenKind::Type,
            "import" => TokenKind::Import,
            "const" => TokenKind::Const,
            "as" => TokenKind::As,
            "try" => TokenKind::Try,
            "throw" => TokenKind::Throw,
            "await" => TokenKind::Await,
            "distinct" => TokenKind::Distinct,
            "pub" => TokenKind::Pub,
            "break" => TokenKind::Break,
            "continue" => TokenKind::Continue,
            "rune" => TokenKind::Rune,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "nil" => TokenKind::Nil,
            _ => TokenKind::Ident(name.to_string()),
        };
        self.push(kind, start);
    }

    fn punct(&mut self, c: char) {
        let start = self.pos - 1;
        let kind = match c {
            '+' => two!(self, '=', TokenKind::PlusAssign, TokenKind::Plus),
            '-' => two!(self, '>', TokenKind::Arrow, TokenKind::Minus),
            '*' => TokenKind::Star,
            '%' => TokenKind::Percent,
            '=' => match self.peek_char() {
                Some('=') => {
                    self.pos += 1;
                    TokenKind::Eq
                }
                _ => TokenKind::Assign,
            },
            '!' => match self.peek_char() {
                Some('=') => {
                    self.pos += 1;
                    TokenKind::Ne
                }
                _ => TokenKind::Bang,
            },
            '<' => match self.peek_char() {
                Some('<') => {
                    self.pos += 1;
                    TokenKind::Shl
                }
                Some('=') => {
                    self.pos += 1;
                    TokenKind::Le
                }
                _ => TokenKind::Lt,
            },
            '>' => match self.peek_char() {
                Some('>') => {
                    self.pos += 1;
                    TokenKind::Shr
                }
                Some('=') => {
                    self.pos += 1;
                    TokenKind::Ge
                }
                _ => TokenKind::Gt,
            },
            '&' => two!(self, '&', TokenKind::AndAnd, TokenKind::Amp),
            '|' => match self.peek_char() {
                Some('|') => {
                    self.pos += 1;
                    TokenKind::PipePipe
                }
                Some('>') => {
                    self.pos += 1;
                    TokenKind::PipeGt
                }
                _ => TokenKind::Pipe,
            },
            '^' => TokenKind::Caret,
            '.' => match self.peek_char() {
                Some('.') => {
                    self.pos += 1;
                    TokenKind::DotDot
                }
                _ => TokenKind::Dot,
            },
            ',' => TokenKind::Comma,
            ':' => TokenKind::Colon,
            '?' => TokenKind::Question,
            '(' => {
                self.depth += 1;
                TokenKind::LParen
            }
            ')' => {
                self.depth = self.depth.saturating_sub(1);
                TokenKind::RParen
            }
            '[' => {
                self.depth += 1;
                TokenKind::LBracket
            }
            ']' => {
                self.depth = self.depth.saturating_sub(1);
                TokenKind::RBracket
            }
            '{' => TokenKind::LBrace,
            '}' => TokenKind::RBrace,
            other => {
                let sp = Span::new(start, self.pos);
                self.diagnostics.push(
                    Diagnostic::error(format!("unexpected character `{other}`"), sp)
                        .with_suggestion("Yoru source must be ASCII except strings/comments"),
                );
                return;
            }
        };
        self.push(kind, start);
    }
}
