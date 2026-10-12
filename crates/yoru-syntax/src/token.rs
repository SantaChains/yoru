//! Token 定义：19 号语法规范 v2 词法面。
//!
//! 记号一义性（19.1）：前缀 `!` 与后缀 `!` 共用 Bang（位置分离）；
//! `|` 同时作按位或与 tag 变体分隔（位置分离）；`..` 同时作区间与字段展开（位置分离）。

/// 字符串字面量的组成部分：纯文本块或 `${expr}` 插值（词法期已递归切分）。
#[derive(Debug, Clone, PartialEq)]
pub enum StrPart {
    /// 转义处理后的文本块。
    Lit(String),
    /// `${...}` 内的 token 序列（由 parser 二次解析为表达式）。
    Interp(Vec<Token>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // ── 字面量 ──
    Int(i64),
    Float(f64),
    Str(Vec<StrPart>),
    True,
    False,
    Nil,

    // ── 标识符与关键字 ──
    /// ASCII 标识符（决策 51）。`_` 也走此处（match 万用模式）。
    Ident(String),
    Let,
    Mut,
    Fn,
    Return,
    If,
    Else,
    /// `while`：19.2 EBNF 有 while_stmt 但关键字冻结表（19.7）漏登——按 EBNF 实现，
    /// 文档修订待办（27 → 28 关键字的偏差需在 19 号修订时统一）。
    While,
    For,
    In,
    Match,
    Struct,
    Tag,
    Protocol,
    Impl,
    Type,
    Import,
    Const,
    As,
    Try,
    Throw,
    Await,
    Distinct,
    Pub,
    Break,
    Continue,
    /// rune 是冻结关键字，但仅类型位合法（决策 46）。
    Rune,

    // ── 运算符与标点 ──
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Assign,
    PlusAssign,
    MinusAssign,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    AndAnd,
    OrOr,
    Bang,
    Caret,
    Amp,
    Shl,
    Shr,
    Pipe,
    /// 管道操作符 `|>`（决策 25；与按位或 `|` 是不同 token）
    PipeGt,
    PipePipe,
    Arrow,
    Dot,
    DotDot,
    Comma,
    Colon,
    Question,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,

    // ── 结构 ──
    /// 词法结束哨兵（lexer 恒在流末尾追加；parser 各循环以此收敛）。
    Eof,
    /// 语句终止符（换行）；括号内/行尾延续时由 lexer 抑制。
    Newline,
    /// `///` 文档注释内容（parser 当前忽略，doctest 提取属工具链 T5）。
    DocComment(String),
}

impl TokenKind {
    /// 行尾延续判定（19.1 确定性规则）：这些记号结尾的行不终止语句。
    pub fn continues_line(&self) -> bool {
        matches!(
            self,
            Self::Plus
                | Self::Minus
                | Self::Star
                | Self::Slash
                | Self::Percent
                | Self::Assign
                | Self::PlusAssign
                | Self::MinusAssign
                | Self::Eq
                | Self::Ne
                | Self::Lt
                | Self::Gt
                | Self::Le
                | Self::Ge
                | Self::AndAnd
                | Self::OrOr
                | Self::Caret
                | Self::Amp
                | Self::Shl
                | Self::Shr
                | Self::Pipe
                | Self::PipeGt
                | Self::Arrow
                | Self::Dot
                | Self::DotDot
                | Self::Comma
                | Self::Colon
                | Self::Question
                | Self::Else
        )
    }

    pub fn describe(&self) -> String {
        match self {
            TokenKind::Int(_) => "integer".into(),
            TokenKind::Float(_) => "float".into(),
            TokenKind::Str(_) => "string".into(),
            TokenKind::Ident(n) => format!("`{n}`"),
            TokenKind::Newline => "newline".into(),
            TokenKind::DocComment(_) => "doc comment".into(),
            other => format!("`{other:?}`"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: crate::diagnostic::Span,
}
