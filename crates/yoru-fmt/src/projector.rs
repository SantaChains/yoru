//! 结构投影打印器（29 号 D3 双断言之二）。
//!
//! span-blind：只输出 AST 节点结构与名字/字面量值，绝不输出 Span——fmt 前后
//! 投影字符串相等 = 换行重排未改变语义结构。这一条专抓 token 序列断言抓不到的
//! 「换行歧义重排」（fmt 发出的换行被 lexer 断错句时投影立变）。
//!
//! match 全覆盖由编译器穷尽性保证：AST 新增节点时本文件编译失败，即 29 号
//! 风险登记「投影器遗漏新 AST 节点」的机械化缓解。

use std::fmt::Write as _;

use yoru_syntax::ast::{
    self, AssignOp, BinOp, DeclKind, ExprKind, FnBody, IndexKind, StmtKind, Type, TypeBody, UnOp,
};

pub fn project(prog: &ast::Program) -> String {
    let mut s = String::new();
    for d in &prog.decls {
        decl(d, &mut s);
        s.push('\n');
    }
    s
}

fn decl(d: &ast::Decl, s: &mut String) {
    if d.pub_ {
        s.push_str("(pub ");
    }
    s.push('(');
    match &d.kind {
        DeclKind::Import { name, alias, path } => {
            let _ = (name, alias, path);
            s.push_str("import");
        }
        DeclKind::Type { name, params, body } => {
            let _ = params;
            s.push_str("type ");
            s.push_str(name);
            s.push('=');
            match body {
                TypeBody::Alias(t) => ty(t, s),
                TypeBody::Distinct(t) => {
                    s.push_str("distinct ");
                    ty(t, s);
                }
                TypeBody::Union(vs) => {
                    for v in vs {
                        s.push('|');
                        variant(v, s);
                    }
                }
            }
        }
        DeclKind::Tag {
            name,
            params,
            variants,
        } => {
            let _ = params;
            s.push_str("tag ");
            s.push_str(name);
            for v in variants {
                s.push('|');
                variant(v, s);
            }
        }
        DeclKind::Struct {
            name,
            params,
            fields,
        } => {
            let _ = params;
            s.push_str("struct ");
            s.push_str(name);
            for f in fields {
                s.push_str(" (");
                s.push_str(&f.name);
                s.push(':');
                ty(&f.ty, s);
                s.push(')');
            }
        }
        DeclKind::Protocol { name, params, fns } => {
            let _ = params;
            s.push_str("protocol ");
            s.push_str(name);
            for f in fns {
                s.push_str(" (fn ");
                s.push_str(&f.name);
                for p in &f.params {
                    s.push(' ');
                    s.push_str(&p.name);
                    s.push(':');
                    ty(&p.ty, s);
                }
                if let Some(r) = &f.ret {
                    s.push_str("->");
                    ty(r, s);
                }
                s.push(')');
            }
        }
        DeclKind::Impl {
            proto,
            for_,
            params,
            fns,
        } => {
            let _ = params;
            s.push_str("impl ");
            s.push_str(proto);
            s.push_str(" for ");
            s.push_str(for_);
            for f in fns {
                s.push(' ');
                fn_decl(f, s);
            }
        }
        DeclKind::Const { name, ty: t, value } => {
            s.push_str("const ");
            s.push_str(name);
            s.push(':');
            ty(t, s);
            s.push('=');
            expr(value, s);
        }
        DeclKind::Fn(f) => fn_decl(f, s),
    }
    s.push(')');
    if d.pub_ {
        s.push(')');
    }
}

fn variant(v: &ast::Variant, s: &mut String) {
    s.push_str(&v.name);
    if let Some(p) = &v.payload {
        s.push('(');
        ty(p, s);
        s.push(')');
    }
}

fn fn_decl(f: &ast::FnDecl, s: &mut String) {
    s.push_str("(fn ");
    s.push_str(&f.name);
    for p in &f.params {
        s.push(' ');
        s.push_str(&p.name);
        s.push(':');
        ty(&p.ty, s);
    }
    if let Some(r) = &f.ret {
        s.push_str("->");
        ty(r, s);
    }
    s.push(' ');
    match &f.body {
        FnBody::Block(b) => block(b, s),
        FnBody::Expr(e) => expr(e, s),
    }
    s.push(')');
}

fn block(b: &ast::Block, s: &mut String) {
    s.push('{');
    for st in &b.stmts {
        stmt(st, s);
        s.push(';');
    }
    s.push('}');
}

fn stmt(st: &ast::Stmt, s: &mut String) {
    s.push('(');
    match &st.kind {
        StmtKind::Let {
            mut_,
            name,
            ty: t,
            value,
        } => {
            s.push_str("let ");
            if *mut_ {
                s.push_str("mut ");
            }
            s.push_str(name);
            if let Some(t) = t {
                s.push(':');
                ty(t, s);
            }
            s.push('=');
            expr(value, s);
        }
        StmtKind::Expr(e) => expr(e, s),
        StmtKind::While { cond, body } => {
            s.push_str("while ");
            expr(cond, s);
            block(body, s);
        }
        StmtKind::For { var, iter, body } => {
            s.push_str("for ");
            s.push_str(var);
            s.push_str(" in ");
            expr(iter, s);
            block(body, s);
        }
        StmtKind::Return(Some(e)) => {
            s.push_str("return ");
            expr(e, s);
        }
        StmtKind::Return(None) => s.push_str("return"),
        StmtKind::Throw(e) => {
            s.push_str("throw ");
            expr(e, s);
        }
        StmtKind::Break => s.push_str("break"),
        StmtKind::Continue => s.push_str("continue"),
        StmtKind::Block(b) => block(b, s),
    }
    s.push(')');
}

fn expr(e: &ast::Expr, s: &mut String) {
    s.push('(');
    match &e.kind {
        ExprKind::Int(v) => {
            let _ = write!(s, "int({v})");
        }
        ExprKind::Float(v) => {
            let _ = write!(s, "float({v:?})");
        }
        ExprKind::Str(parts) => {
            s.push_str("str[");
            for p in parts {
                match p {
                    ast::StrItem::Lit(t) => {
                        let _ = write!(s, "\"{t}\"");
                    }
                    ast::StrItem::Interp(x) => {
                        s.push('$');
                        expr(x, s);
                    }
                }
            }
            s.push(']');
        }
        ExprKind::Bool(v) => {
            let _ = write!(s, "bool({v})");
        }
        ExprKind::Nil => s.push_str("nil"),
        ExprKind::Ident(n) => s.push_str(n),
        ExprKind::Lambda { params, body } => {
            s.push_str("(fn");
            for p in params {
                s.push(' ');
                s.push_str(&p.name);
                if let Some(t) = &p.ty {
                    s.push(':');
                    ty(t, s);
                }
            }
            match &**body {
                FnBody::Block(b) => block(b, s),
                FnBody::Expr(x) => {
                    s.push('=');
                    expr(x, s);
                }
            }
            s.push(')');
        }
        ExprKind::If { cond, then_, else_ } => {
            s.push_str("if ");
            expr(cond, s);
            block(then_, s);
            if let Some(el) = else_ {
                s.push_str(" else ");
                expr(el, s);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            s.push_str("match ");
            expr(scrutinee, s);
            s.push('{');
            for a in arms {
                pat(&a.pattern, s);
                s.push_str("->");
                expr(&a.body, s);
                s.push(';');
            }
            s.push('}');
        }
        ExprKind::Try(x) => {
            s.push_str("try ");
            expr(x, s);
        }
        ExprKind::Await(x) => {
            s.push_str("await ");
            expr(x, s);
        }
        ExprKind::Unary { op, operand } => {
            s.push_str(match op {
                UnOp::Not => "!",
                UnOp::Neg => "-",
            });
            expr(operand, s);
        }
        ExprKind::Binary { op, lhs, rhs } => {
            s.push('(');
            expr(lhs, s);
            s.push(' ');
            s.push_str(bin_op(*op));
            s.push(' ');
            expr(rhs, s);
            s.push(')');
        }
        ExprKind::Call { callee, args } => {
            expr(callee, s);
            s.push('(');
            for a in args {
                expr(a, s);
                s.push(',');
            }
            s.push(')');
        }
        ExprKind::Field { base, field } => {
            expr(base, s);
            s.push('.');
            s.push_str(field);
        }
        ExprKind::Index { base, index } => {
            expr(base, s);
            s.push('[');
            match index {
                IndexKind::Single(i) => expr(i, s),
                IndexKind::Range(a, b) => {
                    expr(a, s);
                    s.push_str("..");
                    expr(b, s);
                }
            }
            s.push(']');
        }
        ExprKind::Unwrap(x) => {
            expr(x, s);
            s.push('!');
        }
        ExprKind::Cast { operand, ty: t } => {
            expr(operand, s);
            s.push_str(" as ");
            ty(t, s);
        }
        ExprKind::StructLit {
            name,
            type_args,
            inits,
            spread,
        } => {
            s.push_str(name);
            for t in type_args {
                s.push('<');
                ty(t, s);
                s.push('>');
            }
            s.push('{');
            for i in inits {
                s.push_str(&i.name);
                s.push(':');
                expr(&i.value, s);
                s.push(',');
            }
            if let Some(sp) = spread {
                s.push_str("..");
                expr(sp, s);
            }
            s.push('}');
        }
        ExprKind::List(items) => {
            s.push('[');
            for i in items {
                expr(i, s);
                s.push(',');
            }
            s.push(']');
        }
        ExprKind::Map(pairs) => {
            s.push_str("map{");
            for (k, v) in pairs {
                expr(k, s);
                s.push(':');
                expr(v, s);
                s.push(',');
            }
            s.push('}');
        }
        ExprKind::Block(b) => block(b, s),
        ExprKind::Assign { target, op, value } => {
            expr(target, s);
            s.push(' ');
            s.push_str(match op {
                AssignOp::Assign => "=",
                AssignOp::AddAssign => "+=",
                AssignOp::SubAssign => "-=",
            });
            s.push(' ');
            expr(value, s);
        }
    }
    s.push(')');
}

fn bin_op(op: BinOp) -> &'static str {
    use BinOp::*;
    match op {
        Or => "||",
        And => "&&",
        Pipe => "|>",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Gt => ">",
        Le => "<=",
        Ge => ">=",
        BitOr => "|",
        BitXor => "^",
        BitAnd => "&",
        Shl => "<<",
        Shr => ">>",
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        Range => "..",
    }
}

fn pat(p: &ast::Pattern, s: &mut String) {
    match p {
        ast::Pattern::Int(v) => {
            let _ = write!(s, "int({v})");
        }
        ast::Pattern::Float(v) => {
            let _ = write!(s, "float({v:?})");
        }
        ast::Pattern::Str(parts) => {
            s.push_str("str[");
            for x in parts {
                match x {
                    ast::StrItem::Lit(t) => {
                        let _ = write!(s, "\"{t}\"");
                    }
                    ast::StrItem::Interp(e) => {
                        s.push('$');
                        expr(e, s);
                    }
                }
            }
            s.push(']');
        }
        ast::Pattern::Bool(v) => {
            let _ = write!(s, "bool({v})");
        }
        ast::Pattern::Nil => s.push_str("nil"),
        ast::Pattern::Bind(n) => s.push_str(n),
        ast::Pattern::Variant { name, payload } => {
            s.push_str(name);
            match payload {
                Some(ast::Payload::Bind(pl)) => {
                    s.push('(');
                    s.push_str(pl);
                    s.push(')');
                }
                Some(ast::Payload::Ignore) => s.push_str("(_)"),
                None => {}
            }
        }
        ast::Pattern::Wildcard => s.push('_'),
    }
}

fn ty(t: &Type, s: &mut String) {
    match t {
        Type::Int => s.push_str("int"),
        Type::Float => s.push_str("float"),
        Type::Bool => s.push_str("bool"),
        Type::Str => s.push_str("string"),
        Type::Rune => s.push_str("rune"),
        Type::Unit => s.push_str("()"),
        Type::Json => s.push_str("json"),
        Type::Named(n, args) => {
            s.push_str(n);
            for a in args {
                s.push('<');
                ty(a, s);
                s.push('>');
            }
        }
        Type::Option(inner) => {
            ty(inner, s);
            s.push('?');
        }
        Type::Result(ok, err) => {
            s.push_str("Result[");
            ty(ok, s);
            s.push(',');
            ty(err, s);
            s.push(']');
        }
        Type::List(el) => {
            s.push('[');
            ty(el, s);
            s.push(']');
        }
        Type::Map(k, v) => {
            s.push('[');
            ty(k, s);
            s.push(':');
            ty(v, s);
            s.push(']');
        }
        Type::Fn(params, ret) => {
            s.push_str("fn(");
            for p in params {
                ty(p, s);
                s.push(',');
            }
            s.push(')');
            s.push_str("->");
            ty(ret, s);
        }
        Type::Distinct(inner) => {
            s.push_str("distinct ");
            ty(inner, s);
        }
    }
}
