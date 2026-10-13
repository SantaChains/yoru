//! Parser：Token 流 → AST（19 号语法规范 v2，递归下降 + 错误恢复）。
//!
//! 设计要点：
//! - 优先级链按 19.2 EBNF 展开（or → range → and → pipe → cmp → bitwise → add → mul → unary → postfix）。
//! - **条件位受限文法**：if/while 条件与 match 判别式内禁 struct 字面量（19.4 歧义规则），
//!   `if flag {` 中 `{` 恒为 block 起点。
//! - 语句分隔 = Newline（lexer 产出的确定性终止符）；块内宽松跳过空行。
//! - **注释预过滤（决策 83）**：`parse()` 入口剥除 trivia（Comment/DocComment），
//!   解析循环对注释零感知；词法流保留全量注释供 fmt/LSP 经 span 消费。
//! - 错误恢复：语句级失败跳到下一 Newline/`}`，不中断整个文件。
//! - `or`/`and` 为**软关键字**：仅运算符位识别为 `||`/`&&`（19.2 EBNF 与 19.5 示例的
//!   命名偏差之调和；正式修法待文档修订）。
//! - 集合 API：自由函数 + 数据第一参数，parser 不特殊处理；`.field` 仅 struct 字段（19.2）。

use crate::ast::*;
use crate::diagnostic::{Diagnostic, Severity, Span};
use crate::token::{StrPart, Token, TokenKind};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    diagnostics: Vec<Diagnostic>,
    /// >0 时禁用 struct 字面量（条件位受限文法）。
    no_struct: usize,
    /// 递归深度计（expr/type/block 共用；T2 硬化：恶意深嵌套会爆原生栈，
    /// 100k 嵌套实测进程崩溃）。
    depth: u32,
    depth_reported: bool,
    /// 迭代包装计数（T2 硬化：Binary/Postfix 链在循环中构造 Box 链，
    /// 递归深度闸对其不可见）。
    wraps: u32,
    chain_reported: bool,
}

/// 嵌套深度上限。500 层 ≈ 数千个原生栈帧，远低于默认栈；正常代码 <100 层。
const MAX_NEST_DEPTH: u32 = 500;

/// 迭代链长上限。`1+1+1+...` 长链与错误恢复中的括号沙漠经循环包装构造
/// 任意深的 Box 链——递归深度闸只对递归可见，循环包装绕过它
/// （T2 硬化实测：50k 项加法链 parse 成功后 drop 爆栈）。
/// 与 MAX_NEST_DEPTH 相加，AST 总深度钳制在 ~1500 层内。
const MAX_CHAIN_LEN: u32 = 1000;

pub fn parse(tokens: &[Token]) -> Result<Program, Vec<Diagnostic>> {
    // trivia 预过滤（rust-analyzer 分层微缩，决策 83）：parser 只见有效 token
    // 与 Newline——注释不进任何解析循环，未来 parser 演进零注释负担。全保真
    // 流由 lex() 提供，F1 fmt / LSP 直接消费（文本经 span 切片复原）。
    let tokens: Vec<Token> = tokens
        .iter()
        .filter(|t| !t.kind.is_trivia())
        .cloned()
        .collect();
    let mut p = Parser {
        tokens,
        pos: 0,
        diagnostics: Vec::new(),
        no_struct: 0,
        depth: 0,
        depth_reported: false,
        wraps: 0,
        chain_reported: false,
    };
    let program = p.parse_program();
    if p.diagnostics.iter().any(|d| d.is_error()) {
        Err(p.diagnostics)
    } else {
        Ok(program)
    }
}

impl Parser {
    // ---- 基础设施 ----

    fn peek(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    /// 是否已到 EOF 哨兵（lexer 恒以 Eof 收尾）。
    fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    fn peek2(&self) -> &Token {
        &self.tokens[(self.pos + 1).min(self.tokens.len() - 1)]
    }

    fn at(&self, kind: &TokenKind) -> bool {
        std::mem::discriminant(&self.peek().kind) == std::mem::discriminant(kind)
    }

    fn bump(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind, what: &str) -> Result<Token, ()> {
        if self.at(kind) {
            Ok(self.bump())
        } else {
            let t = self.peek().clone();
            self.diagnostics.push(
                Diagnostic::error(
                    format!("expected {what}, found {}", t.kind.describe()),
                    t.span,
                )
                .with_suggestion(format!("insert {what} here")),
            );
            Err(())
        }
    }

    /// 跳过空行（注释已由 `parse()` 入口预过滤，此处只见 Newline）。
    fn skip_newlines(&mut self) {
        while matches!(self.peek().kind, TokenKind::Newline) {
            self.bump();
        }
    }

    fn error_current(&mut self, msg: impl Into<String>) {
        let t = self.peek().clone();
        self.diagnostics.push(Diagnostic::error(msg, t.span));
    }

    /// 语句级错误恢复：跳到下一个 Newline 或 `}`（不消费 `}`）。
    fn recover_stmt(&mut self) {
        while !matches!(self.peek().kind, TokenKind::Newline | TokenKind::RBrace) {
            if self.pos >= self.tokens.len() - 1 {
                return;
            }
            self.bump();
        }
    }

    // ---- 程序与顶层声明 ----

    fn parse_program(&mut self) -> Program {
        let mut decls = Vec::new();
        loop {
            self.skip_newlines();
            if self.pos >= self.tokens.len() - 1 {
                break;
            }
            match self.parse_top_decl() {
                Some(d) => decls.push(d),
                None => self.recover_top(),
            }
        }
        Program { decls }
    }

    /// 顶层错误恢复：跳到下一个 Newline。
    fn recover_top(&mut self) {
        while !matches!(self.peek().kind, TokenKind::Newline) {
            if self.pos >= self.tokens.len() - 1 {
                return;
            }
            self.bump();
        }
    }

    fn parse_top_decl(&mut self) -> Option<Decl> {
        let start = self.peek().span;
        let pub_ = self.eat(&TokenKind::Pub);
        let kind = match self.peek().kind.clone() {
            TokenKind::Import => self.parse_import(),
            TokenKind::Type => self.parse_type_decl(),
            TokenKind::Tag => self.parse_tag_decl(),
            TokenKind::Struct => self.parse_struct_decl()?,
            TokenKind::Protocol => self.parse_protocol_decl()?,
            TokenKind::Impl => self.parse_impl_decl()?,
            TokenKind::Const => self.parse_const_decl(),
            TokenKind::Fn => DeclKind::Fn(self.parse_fn_decl()?),
            TokenKind::Newline => return None, // 空行，调用方已 skip，防御
            other => {
                let t = self.peek().clone();
                self.diagnostics.push(
                    Diagnostic::error(
                        format!("expected top-level declaration, found {}", other.describe()),
                        t.span,
                    )
                    .with_suggestion(
                        "top level allows: import / type / tag / struct / protocol / impl / const / fn (optionally `pub`)",
                    ),
                );
                return None;
            }
        };
        if pub_
            && !matches!(
                kind,
                DeclKind::Fn(_)
                    | DeclKind::Struct { .. }
                    | DeclKind::Tag { .. }
                    | DeclKind::Type { .. }
                    | DeclKind::Const { .. }
            )
        {
            self.error_current("`pub` is not allowed on this declaration");
        }
        let span = start.merge(self.peek().span);
        Some(Decl { pub_, kind, span })
    }

    fn parse_import(&mut self) -> DeclKind {
        self.bump(); // import
        let name = match self.expect_ident("module name") {
            Ok(n) => n,
            Err(_) => {
                return DeclKind::Import {
                    name: String::new(),
                    alias: None,
                    path: String::new(),
                };
            }
        };
        let alias = if self.at(&TokenKind::As) {
            self.bump();
            self.expect_ident("alias").ok()
        } else {
            None
        };
        // `from` 是软关键字（不在 27 个冻结关键字内）
        if !matches!(&self.peek().kind, TokenKind::Ident(w) if w == "from") {
            self.error_current("expected `from` in import declaration");
            return DeclKind::Import {
                name,
                alias,
                path: String::new(),
            };
        }
        self.bump();
        let path = match self.peek().kind.clone() {
            TokenKind::Str(parts) => {
                self.bump();
                plain_string(&parts)
            }
            other => {
                let t = self.peek().clone();
                self.diagnostics.push(Diagnostic::error(
                    format!("expected module path string, found {}", other.describe()),
                    t.span,
                ));
                String::new()
            }
        };
        DeclKind::Import { name, alias, path }
    }

    fn expect_ident(&mut self, what: &str) -> Result<String, ()> {
        match self.peek().kind.clone() {
            TokenKind::Ident(n) => {
                self.bump();
                Ok(n)
            }
            other => {
                let t = self.peek().clone();
                self.diagnostics.push(Diagnostic::error(
                    format!("expected {what}, found {}", other.describe()),
                    t.span,
                ));
                Err(())
            }
        }
    }

    /// 类型参数：`<T, U: Ord>`（19.2 文法）或 `[T]`（19.5 示例括号式，兼容）。
    fn parse_type_params(&mut self) -> Vec<String> {
        let mut params = Vec::new();
        let bracket = if self.eat(&TokenKind::Lt) {
            false
        } else if self.eat(&TokenKind::LBracket) {
            true
        } else {
            return params;
        };
        loop {
            if let Ok(n) = self.expect_ident("type parameter") {
                params.push(n);
            }
            if self.eat(&TokenKind::Colon) {
                // 协议约束：typeck 校验，parser 仅消费
                let _ = self.expect_ident("protocol name");
            }
            if self.eat(&TokenKind::Comma) {
                continue;
            }
            break;
        }
        if bracket {
            let _ = self.expect(&TokenKind::RBracket, "`]`");
        } else {
            let _ = self.expect(&TokenKind::Gt, "`>`");
        }
        params
    }

    fn parse_type_decl(&mut self) -> DeclKind {
        self.bump(); // type
        let name = self.expect_ident("type name").unwrap_or_default();
        let params = self.parse_type_params();
        if self.expect(&TokenKind::Assign, "`=`").is_err() {
            return DeclKind::Type {
                name,
                params,
                body: TypeBody::Alias(Type::Unit),
            };
        }
        if self.eat(&TokenKind::Distinct) {
            let ty = self.parse_type();
            return DeclKind::Type {
                name,
                params,
                body: TypeBody::Distinct(ty),
            };
        }
        // tag 联合：`Node(Node2[T]) | Leaf`（19.5；变体 = ident [ `(` type `)` ]）
        if let TokenKind::Ident(_) = self.peek().kind {
            let save = self.pos;
            let vname = self.expect_ident("variant").unwrap_or_default();
            let payload = if self.eat(&TokenKind::LParen) {
                let ty = self.parse_type();
                let _ = self.expect(&TokenKind::RParen, "`)`");
                Some(ty)
            } else {
                None
            };
            if self.at(&TokenKind::Pipe) {
                let mut variants = vec![Variant {
                    name: vname,
                    payload,
                    span: Span::new(0, 0),
                }];
                while self.eat(&TokenKind::Pipe) {
                    let vn = self.expect_ident("variant").unwrap_or_default();
                    let pl = if self.eat(&TokenKind::LParen) {
                        let ty = self.parse_type();
                        let _ = self.expect(&TokenKind::RParen, "`)`");
                        Some(ty)
                    } else {
                        None
                    };
                    variants.push(Variant {
                        name: vn,
                        payload: pl,
                        span: Span::new(0, 0),
                    });
                }
                return DeclKind::Type {
                    name,
                    params,
                    body: TypeBody::Union(variants),
                };
            }
            // 单 ident 且无 `(` 无 `|` → 普通别名，回退统一走 type 文法
            if payload.is_none() {
                self.pos = save;
            } else {
                return DeclKind::Type {
                    name,
                    params,
                    body: TypeBody::Union(vec![Variant {
                        name: vname,
                        payload,
                        span: Span::new(0, 0),
                    }]),
                };
            }
        }
        let ty = self.parse_type();
        DeclKind::Type {
            name,
            params,
            body: TypeBody::Alias(ty),
        }
    }

    fn parse_tag_decl(&mut self) -> DeclKind {
        self.bump(); // tag
        let name = self.expect_ident("tag name").unwrap_or_default();
        let params = self.parse_type_params();
        let _ = self.expect(&TokenKind::Assign, "`=`");
        let mut variants = Vec::new();
        loop {
            let vstart = self.peek().span;
            let vn = self.expect_ident("variant").unwrap_or_default();
            let payload = if self.eat(&TokenKind::LParen) {
                let ty = self.parse_type();
                let _ = self.expect(&TokenKind::RParen, "`)`");
                Some(ty)
            } else {
                None
            };
            variants.push(Variant {
                name: vn,
                payload,
                span: vstart,
            });
            if !self.eat(&TokenKind::Pipe) {
                break;
            }
        }
        DeclKind::Tag {
            name,
            params,
            variants,
        }
    }

    fn parse_struct_decl(&mut self) -> Option<DeclKind> {
        self.bump(); // struct
        let name = self.expect_ident("struct name").ok()?;
        let params = self.parse_type_params();
        self.expect(&TokenKind::LBrace, "`{`").ok()?;
        let mut fields = Vec::new();
        loop {
            self.skip_newlines();
            if self.eat(&TokenKind::RBrace) {
                break;
            }
            let fstart = self.peek().span;
            let fname = self.expect_ident("field name").ok()?;
            self.expect(&TokenKind::Colon, "`:` after field name")
                .ok()?;
            let ty = self.parse_type();
            fields.push(Field {
                name: fname,
                ty,
                span: fstart.merge(self.peek().span),
            });
            // 字段由换行分隔（19.2 struct_decl）；行尾逗号容错
            self.eat(&TokenKind::Comma);
        }
        Some(DeclKind::Struct {
            name,
            params,
            fields,
        })
    }

    fn parse_protocol_decl(&mut self) -> Option<DeclKind> {
        self.bump(); // protocol
        let name = self.expect_ident("protocol name").ok()?;
        let params = self.parse_type_params();
        self.expect(&TokenKind::LBrace, "`{`").ok()?;
        let mut fns = Vec::new();
        loop {
            self.skip_newlines();
            if self.eat(&TokenKind::RBrace) {
                break;
            }
            if self.at_eof() {
                let t = self.peek().clone();
                self.diagnostics.push(Diagnostic::error(
                    "unclosed protocol body, expected `}`",
                    t.span,
                ));
                break;
            }
            if !self.eat(&TokenKind::Fn) {
                self.error_current("expected `fn` in protocol body");
                self.recover_stmt();
                continue;
            }
            let fstart = self.peek().span;
            let fname = self.expect_ident("function name").ok()?;
            let fparams = self.parse_params();
            let ret = if self.eat(&TokenKind::Arrow) {
                Some(self.parse_type())
            } else {
                None
            };
            fns.push(ProtocolFn {
                name: fname,
                params: fparams,
                ret,
                span: fstart.merge(self.peek().span),
            });
        }
        Some(DeclKind::Protocol { name, params, fns })
    }

    fn parse_impl_decl(&mut self) -> Option<DeclKind> {
        self.bump(); // impl
        let proto = self.expect_ident("protocol name").ok()?;
        // protocol_ref 可带类型实参（parser 消费，typeck 校验）
        let _ = self.parse_type_args();
        self.expect(&TokenKind::For, "`for`").ok()?;
        let for_ = self.expect_ident("type name").ok()?;
        let params = self.parse_type_params();
        self.expect(&TokenKind::LBrace, "`{`").ok()?;
        let mut fns = Vec::new();
        loop {
            self.skip_newlines();
            if self.eat(&TokenKind::RBrace) {
                break;
            }
            if self.at_eof() {
                let t = self.peek().clone();
                self.diagnostics.push(Diagnostic::error(
                    "unclosed impl body, expected `}`",
                    t.span,
                ));
                break;
            }
            if !self.at(&TokenKind::Fn) {
                self.error_current("expected `fn` in impl body");
                self.recover_stmt();
                continue;
            }
            fns.push(self.parse_fn_decl()?);
        }
        Some(DeclKind::Impl {
            proto,
            for_,
            params,
            fns,
        })
    }

    fn parse_const_decl(&mut self) -> DeclKind {
        self.bump(); // const
        let name = self.expect_ident("constant name").unwrap_or_default();
        self.expect(&TokenKind::Colon, "`:`").ok();
        let ty = self.parse_type();
        self.expect(&TokenKind::Assign, "`=`").ok();
        let value = self.parse_expr();
        DeclKind::Const { name, ty, value }
    }

    fn parse_params(&mut self) -> Vec<Param> {
        let mut params = Vec::new();
        if !self.eat(&TokenKind::LParen) {
            return params;
        }
        if self.eat(&TokenKind::RParen) {
            return params;
        }
        loop {
            let pstart = self.peek().span;
            let pname = self.expect_ident("parameter name").unwrap_or_default();
            self.expect(&TokenKind::Colon, "`:` after parameter name")
                .ok();
            let ty = self.parse_type();
            params.push(Param {
                name: pname,
                ty,
                span: pstart.merge(self.peek().span),
            });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
            if self.at(&TokenKind::RParen) {
                break; // 尾逗号
            }
        }
        let _ = self.expect(&TokenKind::RParen, "`)`");
        params
    }

    fn parse_fn_decl(&mut self) -> Option<FnDecl> {
        // span 从 `fn` 关键字起（与 ProtocolFn 的 fstart 同口径）——
        // F 线 fmt 依赖 FnDecl.span.start 定位 impl 体内 fn 的行首（29 号 §2.1
        // API 缺口反向扩展）；此前从名字起，诊断定位亦少覆盖签名头。
        let start = self.peek().span;
        self.bump(); // fn
        let name = self.expect_ident("function name").ok()?;
        let type_params = self.parse_type_params();
        let fparams = self.parse_params();
        let ret = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type())
        } else {
            None
        };
        let body = if self.eat(&TokenKind::Assign) {
            FnBody::Expr(self.parse_expr())
        } else {
            FnBody::Block(self.parse_block()?)
        };
        Some(FnDecl {
            name,
            type_params,
            params: fparams,
            ret,
            body,
            span: start.merge(self.peek().span),
        })
    }

    // ---- 类型文法 v2 ----

    fn parse_type(&mut self) -> Type {
        if !self.enter() {
            return Type::Named("int".into(), vec![]);
        }
        let t = self.parse_type_inner();
        self.leave();
        t
    }

    fn parse_type_inner(&mut self) -> Type {
        let base = self.parse_type_base();
        // 后缀 `?`（Option；T?? 交 typeck 拒绝——决策 39）
        let mut ty = base;
        while self.eat(&TokenKind::Question) {
            ty = Type::Option(Box::new(ty));
        }
        ty
    }

    fn parse_type_base(&mut self) -> Type {
        match self.peek().kind.clone() {
            TokenKind::Rune => {
                self.bump();
                Type::Rune
            }
            TokenKind::Ident(n) => match n.as_str() {
                // 基础类型是上下文名（`string` 可同时是 std 模块名，19.5 示例）
                "int" => {
                    self.bump();
                    Type::Int
                }
                "float" => {
                    self.bump();
                    Type::Float
                }
                "bool" => {
                    self.bump();
                    Type::Bool
                }
                "string" if !matches!(self.peek2().kind, TokenKind::Dot) => {
                    self.bump();
                    Type::Str
                }
                "unit" => {
                    self.bump();
                    Type::Unit
                }
                "Json" => {
                    self.bump();
                    Type::Json
                }
                "Result" if matches!(self.peek2().kind, TokenKind::Lt) => {
                    self.bump();
                    self.bump(); // <
                    let ok = self.parse_type();
                    self.expect(&TokenKind::Comma, "`,` in Result<T, E>").ok();
                    let err = self.parse_type();
                    self.expect(&TokenKind::Gt, "`>`").ok();
                    Type::Result(Box::new(ok), Box::new(err))
                }
                _ => {
                    self.bump();
                    // 类型实参双形式：文法 v2 的 `Name<T>` 与 19.5 示例的 `Name[T]`（兼容）
                    let args = if self.at(&TokenKind::Lt) {
                        self.parse_type_args()
                    } else if self.at(&TokenKind::LBracket) {
                        self.parse_bracket_type_args()
                    } else {
                        Vec::new()
                    };
                    Type::Named(n, args)
                }
            },
            TokenKind::LBracket => {
                self.bump();
                let k = self.parse_type();
                if self.eat(&TokenKind::Colon) {
                    let v = self.parse_type();
                    self.expect(&TokenKind::RBracket, "`]`").ok();
                    Type::Map(Box::new(k), Box::new(v))
                } else {
                    self.expect(&TokenKind::RBracket, "`]`").ok();
                    Type::List(Box::new(k))
                }
            }
            TokenKind::LParen => {
                // fn 类型：`(T, U) -> R`
                self.bump();
                let mut ps = Vec::new();
                if !self.eat(&TokenKind::RParen) {
                    loop {
                        ps.push(self.parse_type());
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(&TokenKind::RParen, "`)`").ok();
                }
                self.expect(&TokenKind::Arrow, "`->` in function type").ok();
                let ret = self.parse_type();
                Type::Fn(ps, Box::new(ret))
            }
            other => {
                let t = self.peek().clone();
                self.diagnostics.push(Diagnostic::error(
                    format!("expected type, found {}", other.describe()),
                    t.span,
                ));
                Type::Unit
            }
        }
    }

    /// 类型实参 `[T, U]` 形式（19.5 示例用 `Tree[int]`/`Result[int, Err]`，兼容 19.2 的 `<>`）。
    fn parse_bracket_type_args(&mut self) -> Vec<Type> {
        let mut args = Vec::new();
        self.bump(); // [
        if !self.eat(&TokenKind::RBracket) {
            loop {
                args.push(self.parse_type());
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RBracket, "`]`").ok();
        }
        args
    }

    /// 类型实参 `<T, U>`（named 后缀）。
    fn parse_type_args(&mut self) -> Vec<Type> {
        let mut args = Vec::new();
        if !self.at(&TokenKind::Lt) {
            return args;
        }
        // 防御：`a < b` 比较与 `a<T>` 实参的词法冲突——仅当下一个 token 是类型起点时按实参处理
        if !matches!(
            self.peek2().kind,
            TokenKind::Ident(_) | TokenKind::Rune | TokenKind::LBracket | TokenKind::LParen
        ) {
            return args;
        }
        self.bump();
        loop {
            args.push(self.parse_type());
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::Gt, "`>`").ok();
        args
    }

    // ---- 块与语句 ----

    fn parse_block(&mut self) -> Option<Block> {
        if !self.enter() {
            return None;
        }
        let b = self.parse_block_inner();
        self.leave();
        b
    }

    fn parse_block_inner(&mut self) -> Option<Block> {
        let start = self.expect(&TokenKind::LBrace, "`{`").ok()?.span;
        let mut stmts = Vec::new();
        loop {
            self.skip_newlines();
            if self.eat(&TokenKind::RBrace) {
                let span = start.merge(self.peek().span);
                return Some(Block { stmts, span });
            }
            if self.pos >= self.tokens.len() - 1 {
                let t = self.peek().clone();
                self.diagnostics.push(Diagnostic::error(
                    "unexpected end of file, expected `}`",
                    t.span,
                ));
                return None;
            }
            let stmt = self.parse_stmt();
            if let Some(s) = stmt {
                stmts.push(s);
            } else {
                self.recover_stmt();
            }
            // 语句分隔：换行或 `}`；其余 = 缺换行
            if self.at(&TokenKind::RBrace) {
                continue;
            }
            if self.eat(&TokenKind::Newline) {
                while self.eat(&TokenKind::Newline) {}
                continue;
            }
            let t = self.peek().clone();
            self.diagnostics.push(
                Diagnostic::error(
                    format!(
                        "expected newline after statement, found {}",
                        t.kind.describe()
                    ),
                    t.span,
                )
                .with_suggestion("statements are terminated by newline (19.1)"),
            );
            self.recover_stmt();
        }
    }

    fn parse_stmt(&mut self) -> Option<Stmt> {
        let start = self.peek().span;
        let kind = match self.peek().kind.clone() {
            TokenKind::Let => {
                self.bump();
                let mut_ = self.eat(&TokenKind::Mut);
                let name = self.expect_ident("variable name").ok()?;
                let ty = if self.eat(&TokenKind::Colon) {
                    Some(self.parse_type())
                } else {
                    None
                };
                self.expect(&TokenKind::Assign, "`=` in let binding").ok()?;
                let value = self.parse_expr();
                StmtKind::Let {
                    mut_,
                    name,
                    ty,
                    value,
                }
            }
            TokenKind::While => {
                self.bump();
                let cond = self.parse_cond();
                let body = self.parse_block()?;
                StmtKind::While { cond, body }
            }
            TokenKind::For => {
                self.bump();
                let var = self.expect_ident("loop variable").ok()?;
                self.expect(&TokenKind::In, "`in`").ok()?;
                let iter = self.parse_cond();
                let body = self.parse_block()?;
                StmtKind::For { var, iter, body }
            }
            TokenKind::Return => {
                self.bump();
                if self.at(&TokenKind::Newline) || self.at(&TokenKind::RBrace) {
                    StmtKind::Return(None)
                } else {
                    StmtKind::Return(Some(self.parse_expr()))
                }
            }
            TokenKind::Throw => {
                self.bump();
                StmtKind::Throw(self.parse_expr())
            }
            TokenKind::Break => {
                self.bump();
                StmtKind::Break
            }
            TokenKind::Continue => {
                self.bump();
                StmtKind::Continue
            }
            TokenKind::LBrace => StmtKind::Block(self.parse_block()?),
            _ => StmtKind::Expr(self.parse_expr()),
        };
        let span = start.merge(self.peek().span);
        Some(Stmt { kind, span })
    }

    /// 条件位表达式：受限文法（禁 struct 字面量，19.4）。
    fn parse_cond(&mut self) -> Expr {
        self.no_struct += 1;
        let e = self.parse_expr();
        self.no_struct -= 1;
        e
    }

    // ---- 表达式（优先级链，19.2/19.3）----

    /// 深度闸：超限只报一次诊断（防诊断风暴），调用方以占位值回退。
    fn enter(&mut self) -> bool {
        if self.depth < MAX_NEST_DEPTH {
            self.depth += 1;
            return true;
        }
        if !self.depth_reported {
            self.depth_reported = true;
            let span = self.peek().span;
            self.diagnostics.push(Diagnostic::coded(
                "Y-S9501",
                Severity::Error,
                "nesting depth limit exceeded (input rejected for safety)",
                span,
            ));
        }
        false
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// 迭代包装闸：Binary 优先级链与 Postfix 链在循环中包装 Box——深度计
    /// （enter/leave）只对递归可见，循环包装可构造任意深的链。全局计数，
    /// 超限停止包装（Y-S9502，只报一次），剩余 token 交外层错误恢复。
    fn wrap_guard(&mut self) -> bool {
        self.wraps += 1;
        if self.wraps <= MAX_CHAIN_LEN {
            return true;
        }
        if !self.chain_reported {
            self.chain_reported = true;
            let span = self.peek().span;
            self.diagnostics.push(Diagnostic::coded(
                "Y-S9502",
                Severity::Error,
                "expression chain too long (input rejected for safety)",
                span,
            ));
        }
        false
    }

    pub fn parse_expr(&mut self) -> Expr {
        if !self.enter() {
            return Expr {
                kind: ExprKind::Int(0),
                span: self.peek().span,
            };
        }
        let e = self.parse_expr_inner();
        self.leave();
        e
    }

    fn parse_expr_inner(&mut self) -> Expr {
        let e = self.parse_or();
        // assign_expr ::= lvalue ( "=" | "+=" | "-=" ) expr | or_expr（19.2）
        let op = match self.peek().kind {
            TokenKind::Assign => Some(AssignOp::Assign),
            TokenKind::PlusAssign => Some(AssignOp::AddAssign),
            TokenKind::MinusAssign => Some(AssignOp::SubAssign),
            _ => None,
        };
        if let Some(op) = op {
            if matches!(
                e.kind,
                ExprKind::Ident(_) | ExprKind::Field { .. } | ExprKind::Index { .. }
            ) {
                self.bump();
                let value = self.parse_expr();
                let span = e.span.merge(value.span);
                return Expr {
                    span,
                    kind: ExprKind::Assign {
                        target: Box::new(e),
                        op,
                        value: Box::new(value),
                    },
                };
            }
            let t = self.peek().clone();
            self.diagnostics.push(Diagnostic::error(
                "assignment target must be a variable, field, or index",
                t.span,
            ));
        }
        e
    }

    fn parse_or(&mut self) -> Expr {
        let mut lhs = self.parse_range();
        while !self.at_eof() {
            if !self.wrap_guard() {
                break;
            }
            let op = match self.peek().kind.clone() {
                TokenKind::OrOr => BinOp::Or,
                TokenKind::Ident(w) if w == "or" => BinOp::Or, // 软关键字
                _ => break,
            };
            self.bump();
            let rhs = self.parse_range();
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                span,
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
            };
        }
        lhs
    }

    fn parse_range(&mut self) -> Expr {
        let lhs = self.parse_and();
        if self.at(&TokenKind::DotDot) {
            self.bump();
            let rhs = self.parse_and();
            let span = lhs.span.merge(rhs.span);
            return Expr {
                span,
                kind: ExprKind::Binary {
                    op: BinOp::Range,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
            };
        }
        lhs
    }

    fn parse_and(&mut self) -> Expr {
        let mut lhs = self.parse_pipe();
        while !self.at_eof() {
            if !self.wrap_guard() {
                break;
            }
            let op = match self.peek().kind.clone() {
                TokenKind::AndAnd => BinOp::And,
                TokenKind::Ident(w) if w == "and" => BinOp::And, // 软关键字
                _ => break,
            };
            self.bump();
            let rhs = self.parse_pipe();
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                span,
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
            };
        }
        lhs
    }

    fn parse_pipe(&mut self) -> Expr {
        let mut lhs = self.parse_cmp();
        while !self.at_eof() && self.at(&TokenKind::PipeGt) {
            if !self.wrap_guard() {
                break;
            }
            self.bump();
            let rhs = self.parse_cmp();
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                span,
                kind: ExprKind::Binary {
                    op: BinOp::Pipe,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
            };
        }
        lhs
    }

    fn parse_cmp(&mut self) -> Expr {
        let lhs = self.parse_bitor();
        // 非结合（19.3 级 5※）：至多一次
        let op = match self.peek().kind.clone() {
            TokenKind::Eq => BinOp::Eq,
            TokenKind::Ne => BinOp::Ne,
            TokenKind::Lt => BinOp::Lt,
            TokenKind::Gt => BinOp::Gt,
            TokenKind::Le => BinOp::Le,
            TokenKind::Ge => BinOp::Ge,
            _ => return lhs,
        };
        self.bump();
        let rhs = self.parse_bitor();
        let span = lhs.span.merge(rhs.span);
        Expr {
            span,
            kind: ExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        }
    }

    fn parse_bitor(&mut self) -> Expr {
        self.chain(BinOp::BitOr, &[TokenKind::Pipe], Self::parse_bitxor)
    }

    fn parse_bitxor(&mut self) -> Expr {
        self.chain(BinOp::BitXor, &[TokenKind::Caret], Self::parse_bitand)
    }

    fn parse_bitand(&mut self) -> Expr {
        self.chain(BinOp::BitAnd, &[TokenKind::Amp], Self::parse_shift)
    }

    fn parse_shift(&mut self) -> Expr {
        let mut lhs = self.parse_add();
        'outer: while !self.at_eof() {
            if !self.wrap_guard() {
                break;
            }
            for (k, op) in [(TokenKind::Shl, BinOp::Shl), (TokenKind::Shr, BinOp::Shr)] {
                if self.at(&k) {
                    self.bump();
                    let rhs = self.parse_add();
                    let span = lhs.span.merge(rhs.span);
                    lhs = Expr {
                        span,
                        kind: ExprKind::Binary {
                            op,
                            lhs: Box::new(lhs),
                            rhs: Box::new(rhs),
                        },
                    };
                    continue 'outer;
                }
            }
            break;
        }
        lhs
    }

    fn parse_add(&mut self) -> Expr {
        self.chain_multi(&[TokenKind::Plus, TokenKind::Minus], Self::parse_mul)
    }

    fn parse_mul(&mut self) -> Expr {
        self.chain_multi(
            &[TokenKind::Star, TokenKind::Slash, TokenKind::Percent],
            Self::parse_unary,
        )
    }

    fn chain(&mut self, op: BinOp, kinds: &[TokenKind], next: fn(&mut Self) -> Expr) -> Expr {
        let mut lhs = next(self);
        'outer: while !self.at_eof() {
            if !self.wrap_guard() {
                break;
            }
            for k in kinds {
                if self.at(k) {
                    self.bump();
                    let rhs = next(self);
                    let span = lhs.span.merge(rhs.span);
                    lhs = Expr {
                        span,
                        kind: ExprKind::Binary {
                            op,
                            lhs: Box::new(lhs),
                            rhs: Box::new(rhs),
                        },
                    };
                    continue 'outer;
                }
            }
            break;
        }
        lhs
    }

    fn chain_multi(&mut self, kinds: &[TokenKind], next: fn(&mut Self) -> Expr) -> Expr {
        let mut lhs = next(self);
        'outer: while !self.at_eof() {
            if !self.wrap_guard() {
                break;
            }
            for k in kinds {
                if self.at(k) {
                    let op = match k {
                        TokenKind::Plus => BinOp::Add,
                        TokenKind::Minus => BinOp::Sub,
                        TokenKind::Star => BinOp::Mul,
                        TokenKind::Slash => BinOp::Div,
                        TokenKind::Percent => BinOp::Mod,
                        _ => unreachable!(),
                    };
                    self.bump();
                    let rhs = next(self);
                    let span = lhs.span.merge(rhs.span);
                    lhs = Expr {
                        span,
                        kind: ExprKind::Binary {
                            op,
                            lhs: Box::new(lhs),
                            rhs: Box::new(rhs),
                        },
                    };
                    continue 'outer;
                }
            }
            break;
        }
        lhs
    }

    fn parse_unary(&mut self) -> Expr {
        let start = self.peek().span;
        let op = match self.peek().kind {
            TokenKind::Bang => Some(UnOp::Not),
            TokenKind::Minus => Some(UnOp::Neg),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let operand = self.parse_unary();
            let span = start.merge(operand.span);
            return Expr {
                span,
                kind: ExprKind::Unary {
                    op,
                    operand: Box::new(operand),
                },
            };
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Expr {
        let mut expr = self.parse_primary();
        loop {
            // D-02：插值子解析器 token 切片无 Eof 哨兵，peek() 钳制回末 token
            // 会使本循环永不前进 → pos 越界即断（wrap_guard 触顶前的死自旋根因）。
            if self.pos >= self.tokens.len() {
                break;
            }
            if !self.wrap_guard() {
                break;
            }
            match self.peek().kind.clone() {
                TokenKind::LParen => {
                    self.bump();
                    let mut args = Vec::new();
                    if !self.eat(&TokenKind::RParen) {
                        loop {
                            args.push(self.parse_expr());
                            if !self.eat(&TokenKind::Comma) {
                                break;
                            }
                        }
                        self.expect(&TokenKind::RParen, "`)`").ok();
                    }
                    let span = expr.span.merge(self.peek().span);
                    expr = Expr {
                        span,
                        kind: ExprKind::Call {
                            callee: Box::new(expr),
                            args,
                        },
                    };
                }
                TokenKind::Dot => {
                    self.bump();
                    let field = self.expect_ident("field name").unwrap_or_default();
                    let span = expr.span.merge(self.peek().span);
                    expr = Expr {
                        span,
                        kind: ExprKind::Field {
                            base: Box::new(expr),
                            field,
                        },
                    };
                }
                TokenKind::LBracket => {
                    self.bump();
                    let first = self.parse_expr();
                    // `..` 优先级低于下标上下文：`xs[0..2]` 的区间在 parse_expr 层
                    // 已整体成型为 Binary{Range}，此处按形状拆分（决策 42）
                    let index = if let ExprKind::Binary {
                        op: BinOp::Range,
                        lhs,
                        rhs,
                    } = first.kind
                    {
                        IndexKind::Range(lhs, rhs)
                    } else if self.eat(&TokenKind::DotDot) {
                        // 半开切片：`xs[a..b]`；`xs[a..]` 不合法（终点必填）
                        let end = self.parse_expr();
                        IndexKind::Range(Box::new(first), Box::new(end))
                    } else {
                        IndexKind::Single(Box::new(first))
                    };
                    self.expect(&TokenKind::RBracket, "`]`").ok();
                    let span = expr.span.merge(self.peek().span);
                    expr = Expr {
                        span,
                        kind: ExprKind::Index {
                            base: Box::new(expr),
                            index,
                        },
                    };
                }
                TokenKind::Bang => {
                    self.bump();
                    let span = expr.span.merge(self.peek().span);
                    expr = Expr {
                        span,
                        kind: ExprKind::Unwrap(Box::new(expr)),
                    };
                }
                TokenKind::As => {
                    // 决策 26：as 是唯一显式转换（EBNF 19.2 缺产式，按 19.5 示例补入）
                    self.bump();
                    let ty = self.parse_type();
                    let span = expr.span.merge(self.peek().span);
                    expr = Expr {
                        span,
                        kind: ExprKind::Cast {
                            operand: Box::new(expr),
                            ty,
                        },
                    };
                }
                _ => break,
            }
        }
        expr
    }

    fn parse_primary(&mut self) -> Expr {
        let start = self.peek().span;
        let kind = match self.peek().kind.clone() {
            TokenKind::Int(v) => {
                self.bump();
                ExprKind::Int(v)
            }
            TokenKind::Float(v) => {
                self.bump();
                ExprKind::Float(v)
            }
            TokenKind::Str(parts) => {
                self.bump();
                ExprKind::Str(self.parse_str_parts(parts))
            }
            TokenKind::True => {
                self.bump();
                ExprKind::Bool(true)
            }
            TokenKind::False => {
                self.bump();
                ExprKind::Bool(false)
            }
            TokenKind::Nil => {
                self.bump();
                ExprKind::Nil
            }
            TokenKind::Fn => return self.parse_lambda(),
            TokenKind::If => return self.parse_if(),
            TokenKind::Match => return self.parse_match(),
            TokenKind::Try => {
                self.bump();
                let operand = self.parse_unary();
                ExprKind::Try(Box::new(operand))
            }
            TokenKind::Await => {
                self.bump();
                let operand = self.parse_unary();
                ExprKind::Await(Box::new(operand))
            }
            TokenKind::LBracket => {
                // list 或 map 字面量：`[a, b]` / `[k: v]` / `[:]`
                self.bump();
                if self.eat(&TokenKind::Colon) {
                    self.expect(&TokenKind::RBracket, "`]`").ok();
                    ExprKind::Map(Vec::new())
                } else if self.eat(&TokenKind::RBracket) {
                    ExprKind::List(Vec::new())
                } else {
                    let first = self.parse_expr();
                    if self.eat(&TokenKind::Colon) {
                        let mut entries = vec![(first, self.parse_expr())];
                        while self.eat(&TokenKind::Comma) {
                            if self.at(&TokenKind::RBracket) {
                                break;
                            }
                            let k = self.parse_expr();
                            self.expect(&TokenKind::Colon, "`:` in map literal").ok();
                            entries.push((k, self.parse_expr()));
                        }
                        self.expect(&TokenKind::RBracket, "`]`").ok();
                        ExprKind::Map(entries)
                    } else {
                        let mut items = vec![first];
                        while self.eat(&TokenKind::Comma) {
                            if self.at(&TokenKind::RBracket) {
                                break;
                            }
                            items.push(self.parse_expr());
                        }
                        self.expect(&TokenKind::RBracket, "`]`").ok();
                        ExprKind::List(items)
                    }
                }
            }
            TokenKind::LParen => {
                self.bump();
                let e = self.parse_expr();
                self.expect(&TokenKind::RParen, "`)`").ok();
                return e;
            }
            TokenKind::Ident(name) => {
                // struct 字面量：`Type { f: v }` / `Type<T> { .. }`——条件位受限（19.4）
                let is_struct_lit = matches!(self.peek2().kind, TokenKind::LBrace)
                    || (matches!(self.peek2().kind, TokenKind::Lt)
                        && self.no_struct == 0
                        && self.generic_struct_lit_ahead());
                if is_struct_lit && self.no_struct == 0 {
                    self.bump();
                    let type_args = self.parse_type_args();
                    self.bump(); // {
                    let mut inits = Vec::new();
                    let mut spread = None;
                    loop {
                        if self.eat(&TokenKind::RBrace) {
                            break;
                        }
                        if self.eat(&TokenKind::DotDot) {
                            // 函数式字段更新（决策 41）：`..base`；顺序不限，typeck 限一次
                            let base = self.parse_expr();
                            spread = Some(Box::new(base));
                            if !self.eat(&TokenKind::Comma) {
                                self.expect(&TokenKind::RBrace, "`}`").ok();
                                break;
                            }
                            continue;
                        }
                        let fstart = self.peek().span;
                        let fname = self.expect_ident("field name").unwrap_or_default();
                        self.expect(&TokenKind::Colon, "`:` in struct literal").ok();
                        let value = self.parse_expr();
                        inits.push(FieldInit {
                            name: fname,
                            value,
                            span: fstart.merge(self.peek().span),
                        });
                        if !self.eat(&TokenKind::Comma) {
                            self.expect(&TokenKind::RBrace, "`}`").ok();
                            break;
                        }
                    }
                    ExprKind::StructLit {
                        name,
                        type_args,
                        inits,
                        spread,
                    }
                } else {
                    self.bump();
                    ExprKind::Ident(name)
                }
            }
            other => {
                let t = self.peek().clone();
                self.diagnostics.push(Diagnostic::error(
                    format!("expected expression, found {}", other.describe()),
                    t.span,
                ));
                self.bump();
                ExprKind::Nil
            }
        };
        let span = start.merge(self.peek().span);
        Expr { span, kind }
    }

    /// 前瞻 `Ident < ... > {` 是否为泛型 struct 字面量（受限文法下不调用）。
    fn generic_struct_lit_ahead(&self) -> bool {
        // 从 pos+1 开始扫描 `<` ... `>` 后是否紧跟 `{`（浅扫描，嵌套泛型忽略）
        let mut i = self.pos + 2; // 越过 Ident 与 <
        let mut depth = 1usize;
        while i < self.tokens.len() {
            match &self.tokens[i].kind {
                TokenKind::Lt => depth += 1,
                TokenKind::Gt => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(
                            self.tokens.get(i + 1).map(|t| &t.kind),
                            Some(TokenKind::LBrace)
                        );
                    }
                }
                TokenKind::Newline => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    fn parse_str_parts(&mut self, parts: Vec<StrPart>) -> Vec<StrItem> {
        parts
            .into_iter()
            .map(|p| match p {
                StrPart::Lit(s) => StrItem::Lit(s),
                StrPart::Interp(mut tokens) => {
                    // D-02（26 号登记）：子解析器 token 切片必须以 Eof 哨兵收尾。
                    // lexer 产物恒以 Eof 结尾，但插值切片是截取的裸序列——无 Eof 时
                    // at_eof() 恒假、bump() 钳制在 len-1，postfix 循环永不前进，
                    // 直至 Y-S9502 wrap 预算触顶（`${o!}` 以 `!` 结尾即复现）。
                    if let Some(last) = tokens.last() {
                        let span = last.span;
                        tokens.push(Token {
                            kind: TokenKind::Eof,
                            span,
                        });
                    }
                    let mut sub = Parser {
                        tokens,
                        pos: 0,
                        diagnostics: Vec::new(),
                        no_struct: 0,
                        depth: 0,
                        depth_reported: false,
                        wraps: 0,
                        chain_reported: false,
                    };
                    let e = sub.parse_expr();
                    self.diagnostics.append(&mut sub.diagnostics);
                    StrItem::Interp(Box::new(e))
                }
            })
            .collect()
    }

    /// lambda：`fn(params) = expr` / `fn(params) { block }`（参数类型可省）。
    fn parse_lambda(&mut self) -> Expr {
        let start = self.peek().span;
        self.bump(); // fn
        let mut params = Vec::new();
        if self.eat(&TokenKind::LParen) && !self.eat(&TokenKind::RParen) {
            loop {
                let pstart = self.peek().span;
                let pname = self.expect_ident("parameter name").unwrap_or_default();
                let ty = if self.eat(&TokenKind::Colon) {
                    Some(self.parse_type())
                } else {
                    None
                };
                params.push(LamParam {
                    name: pname,
                    ty,
                    span: pstart.merge(self.peek().span),
                });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
                if self.at(&TokenKind::RParen) {
                    break;
                }
            }
            self.expect(&TokenKind::RParen, "`)`").ok();
        }
        let body = if self.eat(&TokenKind::Assign) {
            FnBody::Expr(self.parse_expr())
        } else {
            match self.parse_block() {
                Some(b) => FnBody::Block(b),
                None => FnBody::Expr(Expr {
                    span: start,
                    kind: ExprKind::Nil,
                }),
            }
        };
        let span = start.merge(self.peek().span);
        Expr {
            span,
            kind: ExprKind::Lambda {
                params,
                body: Box::new(body),
            },
        }
    }

    /// `if cond { } else (if ... | { ... })`；else 缺省仅语句位合法（typeck 校验）。
    fn parse_if(&mut self) -> Expr {
        let start = self.peek().span;
        self.bump(); // if
        let cond = self.parse_cond();
        let then_ = match self.parse_block() {
            Some(b) => b,
            None => {
                return Expr {
                    span: start,
                    kind: ExprKind::Nil,
                };
            }
        };
        let else_ = if self.eat(&TokenKind::Else) {
            if self.at(&TokenKind::If) {
                Some(Box::new(self.parse_if()))
            } else {
                match self.parse_block() {
                    Some(b) => Some(Box::new(Expr {
                        span: b.span,
                        kind: ExprKind::Block(b),
                    })),
                    None => None,
                }
            }
        } else {
            None
        };
        let span = start.merge(self.peek().span);
        Expr {
            span,
            kind: ExprKind::If {
                cond: Box::new(cond),
                then_,
                else_,
            },
        }
    }

    /// `match expr { pattern -> (expr | block), ... }`；判别式受限文法。
    fn parse_match(&mut self) -> Expr {
        let start = self.peek().span;
        self.bump(); // match
        let scrutinee = self.parse_cond();
        if self
            .expect(&TokenKind::LBrace, "`{` to open match arms")
            .is_err()
        {
            return Expr {
                span: start,
                kind: ExprKind::Nil,
            };
        }
        let mut arms = Vec::new();
        loop {
            self.skip_newlines();
            if self.eat(&TokenKind::RBrace) {
                break;
            }
            if self.at_eof() {
                let t = self.peek().clone();
                self.diagnostics
                    .push(Diagnostic::error("unclosed match, expected `}`", t.span));
                break;
            }
            let astart = self.peek().span;
            let pattern = self.parse_pattern();
            self.expect(&TokenKind::Arrow, "`->` in match arm").ok();
            let body = if self.at(&TokenKind::LBrace) {
                match self.parse_block() {
                    Some(b) => Expr {
                        span: b.span,
                        kind: ExprKind::Block(b),
                    },
                    None => Expr {
                        span: astart,
                        kind: ExprKind::Nil,
                    },
                }
            } else {
                self.parse_expr()
            };
            arms.push(MatchArm {
                pattern,
                body,
                span: astart.merge(self.peek().span),
            });
            // arm 分隔：换行（行尾 `|`/运算符延续由 lexer 处理；此处宽松接受尾逗号）
            self.eat(&TokenKind::Comma);
            if !self.eat(&TokenKind::Newline) && !self.at(&TokenKind::RBrace) {
                let t = self.peek().clone();
                self.diagnostics.push(Diagnostic::error(
                    format!(
                        "expected newline after match arm, found {}",
                        t.kind.describe()
                    ),
                    t.span,
                ));
                self.recover_stmt();
            }
        }
        let span = start.merge(self.peek().span);
        Expr {
            span,
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
        }
    }

    /// `pattern ::= literal | identifier | variant_pattern | "_"`（无解构模式）。
    fn parse_pattern(&mut self) -> Pattern {
        match self.peek().kind.clone() {
            TokenKind::Int(v) => {
                self.bump();
                Pattern::Int(v)
            }
            TokenKind::Float(v) => {
                self.bump();
                Pattern::Float(v)
            }
            TokenKind::Str(parts) => {
                self.bump();
                Pattern::Str(self.parse_str_parts(parts))
            }
            TokenKind::True => {
                self.bump();
                Pattern::Bool(true)
            }
            TokenKind::False => {
                self.bump();
                Pattern::Bool(false)
            }
            TokenKind::Nil => {
                self.bump();
                Pattern::Nil
            }
            TokenKind::Ident(n) => {
                self.bump();
                if n == "_" {
                    Pattern::Wildcard
                } else if self.eat(&TokenKind::LParen) {
                    let payload = self.expect_ident("variant payload binding").ok();
                    self.expect(&TokenKind::RParen, "`)`").ok();
                    Pattern::Variant { name: n, payload }
                } else {
                    Pattern::Bind(n)
                }
            }
            other => {
                let t = self.peek().clone();
                self.diagnostics.push(Diagnostic::error(
                    format!("expected pattern, found {}", other.describe()),
                    t.span,
                ));
                self.bump();
                Pattern::Wildcard
            }
        }
    }
}

/// 纯文本字符串（import 路径用）：含插值则仅拼接文本（路径含插值属非法，typeck 报）。
fn plain_string(parts: &[StrPart]) -> String {
    let mut out = String::new();
    for p in parts {
        if let StrPart::Lit(s) = p {
            out.push_str(s);
        }
    }
    out
}
