//! 布局计划：AST 区间指导（29 号 D1 混合架构的 AST 半边）。
//!
//! 产物两件：
//! - `block_opens`：块式 `{` 的字节偏移——其体换行、缩进 +1。Block 区间直接取
//!   `Block.span.start`（parser 实证 = LBrace token 起点精确偏移）；match 臂区 /
//!   struct / impl / protocol 体由「内容首 token 之前最近的 LBrace」回扫定位
//!   （模式 / 字段 / fn 签名不含花括号，回扫必中；空体不登记 → 走行内 `{ }`）。
//! - `breaks`：必须起新行的 token 偏移（顶层 decl / 块内 stmt / struct 字段 /
//!   match 臂 / impl·protocol 内 fn）。表达式内部一律不折行（F1 80%：折行策略
//!   第二批，29 号 §6）。
//!
//! 铁律由构造保证：所有断点的首 token 都是词/字面量（stmt 以关键字或 Ident 起、
//! 字段以 Ident 起、臂以模式起、decl 以 Pub/Fn/Struct… 起）——永不行首运算符（L1）。

use std::collections::HashSet;

use yoru_syntax::ast::{self, DeclKind, ExprKind, FnBody, StmtKind};

#[derive(Debug, Default)]
pub struct Plan {
    /// 块式 `{` 偏移集合。
    pub block_opens: HashSet<usize>,
    /// 断点偏移集合（该偏移上的 token 起新行）。
    pub breaks: HashSet<usize>,
}

/// 从 AST + token 流构建布局计划。
pub fn build_plan(prog: &ast::Program, tokens: &[yoru_syntax::Token]) -> Plan {
    let mut p = Plan::default();
    let mut brace_queries: Vec<usize> = Vec::new(); // 内容首 token 偏移 → 回扫最近 LBrace
    for d in &prog.decls {
        p.breaks.insert(d.span.start);
        visit_decl(d, &mut p, &mut brace_queries);
    }
    resolve_brace_queries(tokens, brace_queries, &mut p);
    p
}

fn resolve_brace_queries(tokens: &[yoru_syntax::Token], mut queries: Vec<usize>, p: &mut Plan) {
    if queries.is_empty() {
        return;
    }
    queries.sort_unstable();
    let mut last_lbrace: Option<usize> = None;
    let mut qi = 0;
    for t in tokens {
        while qi < queries.len() && queries[qi] < t.span.start {
            // 查询点严格小于当前 token：记录此前最近的 LBrace
            if let Some(off) = last_lbrace {
                p.block_opens.insert(off);
            }
            qi += 1;
        }
        if t.kind == yoru_syntax::TokenKind::LBrace {
            last_lbrace = Some(t.span.start);
        }
    }
    // 查询点在 Eof 之后的残余（理论不可达：内容首 token 必在流内）
    while qi < queries.len() {
        if let Some(off) = last_lbrace {
            p.block_opens.insert(off);
        }
        qi += 1;
    }
}

fn visit_decl(d: &ast::Decl, p: &mut Plan, q: &mut Vec<usize>) {
    match &d.kind {
        DeclKind::Import { .. }
        | DeclKind::Const { .. }
        | DeclKind::Type { .. }
        | DeclKind::Tag { .. } => {}
        DeclKind::Struct { fields, .. } => {
            if let Some(f) = fields.first() {
                q.push(f.span.start);
                for f in fields {
                    p.breaks.insert(f.span.start);
                }
            }
        }
        DeclKind::Protocol { fns, .. } => {
            if let Some(f) = fns.first() {
                q.push(f.span.start);
                for f in fns {
                    p.breaks.insert(f.span.start);
                }
            }
        }
        DeclKind::Impl { fns, .. } => {
            if let Some(f) = fns.first() {
                q.push(f.span.start);
                for f in fns {
                    p.breaks.insert(f.span.start);
                    visit_fn(f, p, q);
                }
            }
        }
        DeclKind::Fn(f) => visit_fn(f, p, q),
    }
}

fn visit_fn(f: &ast::FnDecl, p: &mut Plan, q: &mut Vec<usize>) {
    match &f.body {
        FnBody::Block(b) => visit_block(b, p, q),
        FnBody::Expr(e) => visit_expr(e, p, q),
    }
}

fn visit_block(b: &ast::Block, p: &mut Plan, q: &mut Vec<usize>) {
    if b.stmts.is_empty() {
        return; // 空块走行内 `{ }`
    }
    p.block_opens.insert(b.span.start);
    for s in &b.stmts {
        p.breaks.insert(s.span.start);
        visit_stmt(s, p, q);
    }
}

fn visit_stmt(s: &ast::Stmt, p: &mut Plan, q: &mut Vec<usize>) {
    match &s.kind {
        StmtKind::Let { value, .. } => visit_expr(value, p, q),
        StmtKind::Expr(e) => visit_expr(e, p, q),
        StmtKind::While { body, .. } | StmtKind::For { body, .. } => visit_block(body, p, q),
        StmtKind::Return(Some(e)) | StmtKind::Throw(e) => visit_expr(e, p, q),
        StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
        StmtKind::Block(b) => visit_block(b, p, q),
    }
}

fn visit_expr(e: &ast::Expr, p: &mut Plan, q: &mut Vec<usize>) {
    match &e.kind {
        ExprKind::If { then_, else_, .. } => {
            visit_block(then_, p, q);
            if let Some(el) = else_ {
                visit_expr(el, p, q);
            }
        }
        ExprKind::Match { arms, .. } => {
            if let Some(a) = arms.first() {
                q.push(a.span.start);
            }
            for a in arms {
                p.breaks.insert(a.span.start);
                visit_expr(&a.body, p, q);
            }
        }
        ExprKind::Lambda { body, .. } => match &**body {
            FnBody::Block(b) => visit_block(b, p, q),
            FnBody::Expr(e) => visit_expr(e, p, q),
        },
        ExprKind::Block(b) => visit_block(b, p, q),
        ExprKind::Unary { operand, .. } => visit_expr(operand, p, q),
        ExprKind::Binary { lhs, rhs, .. } => {
            visit_expr(lhs, p, q);
            visit_expr(rhs, p, q);
        }
        ExprKind::Call { callee, args } => {
            visit_expr(callee, p, q);
            for a in args {
                visit_expr(a, p, q);
            }
        }
        ExprKind::Field { base, .. } => visit_expr(base, p, q),
        ExprKind::Index { base, index } => {
            visit_expr(base, p, q);
            match index {
                ast::IndexKind::Single(i) => visit_expr(i, p, q),
                ast::IndexKind::Range(a, b) => {
                    visit_expr(a, p, q);
                    visit_expr(b, p, q);
                }
            }
        }
        ExprKind::Unwrap(e) | ExprKind::Try(e) | ExprKind::Await(e) => visit_expr(e, p, q),
        ExprKind::Cast { operand, .. } => visit_expr(operand, p, q),
        ExprKind::Assign { target, value, .. } => {
            visit_expr(target, p, q);
            visit_expr(value, p, q);
        }
        ExprKind::StructLit { inits, spread, .. } => {
            for i in inits {
                visit_expr(&i.value, p, q);
            }
            if let Some(s) = spread {
                visit_expr(s, p, q);
            }
        }
        ExprKind::List(items) => {
            for i in items {
                visit_expr(i, p, q);
            }
        }
        ExprKind::Map(pairs) => {
            for (k, v) in pairs {
                visit_expr(k, p, q);
                visit_expr(v, p, q);
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bool(_)
        | ExprKind::Nil
        | ExprKind::Ident(_) => {}
    }
}
