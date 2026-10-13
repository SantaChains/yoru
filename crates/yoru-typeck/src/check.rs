//! 类型检查器：声明收集 + 按需类型解析 + 流敏感 nil 收窄 + 调用期泛型 unify。
//!
//! 实现的决策面（阶段 1，20 号路线图退出标准）：
//! - 26 int/float 严格 `as`；27 T→T? 隐式加宽、反向永不；30 深相等 + Map 键约束；
//! - 34 忽略 Result 警告；39 禁 T??；40 nil 智能收窄；41 函数式字段更新 + owned mut lvalue；
//! - 42 半开切片；44 floor-mod 仅 int；46 禁 `s[i]`；49 泛型实参校验；
//! - 52 空字面量须标注；53/54/55 tag 构造、try 双传播、Map 索引 → V?；
//! - 21 协议显式 impl 校验；23 distinct 不互换；47 break/continue 无标签；
//! - 43 未使用符号警告；50 顶层纯声明。

use std::collections::{HashMap, HashSet};

use yoru_syntax::ast::{self, BinOp, Expr, ExprKind, Pattern, Payload, StmtKind, UnOp};
use yoru_syntax::{Diagnostic, Severity, Span};

use crate::env::{BUILTIN_PROTOCOLS, DeclTable, Env, Resolver, TypeKind};
use crate::ty::Ty;

// ---------- 诊断辅助 ----------

fn d(code: &str, sev: Severity, msg: impl Into<String>, span: Span) -> Diagnostic {
    Diagnostic::coded(code, sev, msg, span)
}

fn lev(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for ca in &a {
        let mut cur = vec![prev[0] + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

fn did_you_mean(name: &str, candidates: &[String]) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for c in candidates {
        let dist = lev(name, c);
        if dist <= 2 && best.as_ref().is_none_or(|(bd, _)| dist < *bd) {
            best = Some((dist, c.clone()));
        }
    }
    best.map(|(_, c)| format!("是否想写 `{c}`？"))
}

// ---------- 作用域 ----------

#[derive(Clone)]
struct Binding {
    ty: Ty,
    mutable: bool,
    used: bool,
}

#[derive(Default)]
struct Scope {
    vars: HashMap<String, Binding>,
    /// 流敏感收窄事实（决策 40）：变量名 → 收窄后类型
    facts: HashMap<String, Ty>,
}

enum Resolved {
    Ty(Ty),
    Module { module: String },
}

#[derive(Clone, Copy, PartialEq)]
enum Term {
    No,
    Return,
    Throw,
    Loop,
}

// ---------- 检查器 ----------

pub struct Checker<'e> {
    env: &'e Env,
    decls: DeclTable,
    program: &'e ast::Program,
    diags: Vec<Diagnostic>,
    scopes: Vec<Scope>,
    fn_ret: Option<Ty>,
    /// 当前检查中 fn 的泛型形参（D-13）：resolver() 注入 Ty::Generic，
    /// body 内 `let x: T` 等注解位与签名位统一泛型感知；非 fn 上下文为空。
    fn_typarams: Vec<String>,
    loop_depth: usize,
    used_imports: HashMap<String, bool>,
}

/// 类型检查入口：返回诊断（错误 + 警告）。
pub fn check(program: &ast::Program, env: &Env) -> Vec<Diagnostic> {
    let mut c = match Checker::new(program, env) {
        Ok(c) => c,
        Err(dg) => return vec![dg],
    };
    c.validate_impls();
    c.check_consts();
    c.check_fns();
    c.report_unused_imports();
    dedup_diags(c.diags)
}

/// 级联抑制（rustc error-cascade suppression 同款）：同 code + 同 span +
/// 同 message 的**精确重复**只留首条。重复发射是诊断实现瑕疵（双路径
/// 检查同一节点），不是两个错误——重复诊断浪费 AI 修复上下文并双倍
/// 计数错误数（优先级 #2：AI 错误少）。只去精确重复，不合并不同
/// span/不同码的合法派生诊断。
fn dedup_diags(diags: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut seen: HashSet<(Option<String>, usize, usize, String)> = HashSet::new();
    let mut out = Vec::with_capacity(diags.len());
    for d in diags {
        let key = (
            d.code.clone(),
            d.span.map(|s| s.start).unwrap_or(usize::MAX),
            d.span.map(|s| s.end).unwrap_or(usize::MAX),
            d.message.clone(),
        );
        if seen.insert(key) {
            out.push(d);
        }
    }
    out
}

/// 收窄事实表：变量名 → 收窄后类型。
type Facts = Vec<(String, Ty)>;

impl<'e> Checker<'e> {
    fn new(program: &'e ast::Program, env: &'e Env) -> Result<Checker<'e>, Diagnostic> {
        let decls = DeclTable::collect(program)?;
        let mut used_imports = HashMap::new();
        for (bind, (path, module, span)) in &decls.imports {
            used_imports.insert(bind.clone(), false);
            let known = env
                .modules
                .get(path)
                .map(|m| m.contains_key(module))
                .unwrap_or(false);
            if !known {
                return Err(d(
                    "Y-T0007",
                    Severity::Error,
                    format!("模块 `{module}` 未找到（路径 `{path}`）"),
                    *span,
                )
                .with_suggestion("阶段 1 仅内置 std（math/json/string）；宿主模块在阶段 4 注册"));
            }
        }
        Ok(Checker {
            env,
            decls,
            program,
            diags: Vec::new(),
            scopes: vec![Scope::default()],
            fn_ret: None,
            fn_typarams: Vec::new(),
            loop_depth: 0,
            used_imports,
        })
    }

    fn err(&mut self, dg: Diagnostic) {
        self.diags.push(dg);
    }

    fn resolver(&self) -> Resolver<'_> {
        Resolver {
            decls: &self.decls,
            env: self.env,
            subst: self
                .fn_typarams
                .iter()
                .cloned()
                .map(|t| (t.clone(), Ty::Generic(t)))
                .collect(),
        }
    }

    // ===== 顶层 =====

    /// 协议显式 impl 校验（决策 21）：方法集一致 + 签名匹配。
    fn validate_impls(&mut self) {
        let impls = std::mem::take(&mut self.decls.impls);
        for imp in &impls {
            let self_ty = Ty::named(
                &imp.for_,
                imp.params.iter().map(|p| Ty::Generic(p.clone())).collect(),
            );
            let expected: Vec<(String, Vec<Ty>, Ty)> = if imp.proto == "Ord" {
                vec![(
                    "cmp".into(),
                    vec![self_ty.clone(), self_ty.clone()],
                    Ty::Int,
                )]
            } else if imp.proto == "Hash" {
                vec![("hash".into(), vec![self_ty.clone()], Ty::Int)]
            } else if let Some(bp) = BUILTIN_PROTOCOLS.iter().find(|p| p.name == imp.proto) {
                vec![(
                    bp.method.into(),
                    vec![self_ty.clone(); bp.n_params],
                    self_ty.clone(),
                )]
            } else if let Some((pparams, sigs)) = self.decls.protocols.get(&imp.proto).cloned() {
                let subst: HashMap<String, Ty> = pparams
                    .iter()
                    .cloned()
                    .zip(std::iter::repeat(self_ty.clone()))
                    .collect();
                let r = Resolver {
                    decls: &self.decls,
                    env: self.env,
                    subst,
                };
                sigs.iter()
                    .map(|(fname, ps, ret)| {
                        let rps: Vec<Ty> = ps
                            .iter()
                            .map(|p| r.resolve(p).unwrap_or(Ty::Unit))
                            .collect();
                        let rret = ret
                            .as_ref()
                            .map(|t| r.resolve(t).unwrap_or(Ty::Unit))
                            .unwrap_or(Ty::Unit);
                        (fname.clone(), rps, rret)
                    })
                    .collect()
            } else {
                self.err(
                    d(
                        "Y-T0007",
                        Severity::Error,
                        format!("未知协议 `{}`", imp.proto),
                        imp.span,
                    )
                    .with_suggestion("内置协议：Add/Sub/Mul/Div/Ord/Hash；自定义协议先声明"),
                );
                continue;
            };
            for (mname, formal, ret) in &expected {
                match imp.methods.iter().find(|m| &m.name == mname) {
                    None => {
                        self.err(d(
                            "Y-T0009",
                            Severity::Error,
                            format!(
                                "`impl {} for {}` 缺少方法 `{mname}`（决策 21：显式 impl 必须完整）",
                                imp.proto, imp.for_
                            ),
                            imp.span,
                        )
                        .with_suggestion(format!("补齐 `fn {mname}(...)`")));
                    }
                    Some(m) => {
                        if m.params.len() != formal.len() {
                            self.err(d(
                                "Y-T0009",
                                Severity::Error,
                                format!(
                                    "`{}.{}` 需要 {} 个参数，实现给出 {}",
                                    mname,
                                    imp.for_,
                                    formal.len(),
                                    m.params.len()
                                ),
                                m.span,
                            ));
                        } else {
                            let actuals: Vec<Ty> = {
                                let r = self.resolver();
                                m.params
                                    .iter()
                                    .map(|(_, pty)| r.resolve(pty).unwrap_or(Ty::Unit))
                                    .collect()
                            };
                            let rret = {
                                let r = self.resolver();
                                m.ret
                                    .as_ref()
                                    .map(|t| r.resolve(t).unwrap_or(Ty::Unit))
                                    .unwrap_or(Ty::Unit)
                            };
                            for (i, actual) in actuals.iter().enumerate() {
                                if !actual.same(&formal[i]) {
                                    self.err(d(
                                        "Y-T0009",
                                        Severity::Error,
                                        format!(
                                            "`{}.{}` 第 {} 参数类型不符：期望 `{}`，实现为 `{}`",
                                            mname,
                                            imp.for_,
                                            i + 1,
                                            formal[i],
                                            actual
                                        ),
                                        m.span,
                                    ));
                                }
                            }
                            if !rret.same(ret) {
                                self.err(d(
                                    "Y-T0009",
                                    Severity::Error,
                                    format!(
                                        "`{}.{}` 返回类型不符：期望 `{}`，实现为 `{}`",
                                        mname, imp.for_, ret, rret
                                    ),
                                    m.span,
                                ));
                            }
                        }
                    }
                }
            }
            for m in &imp.methods {
                if !expected.iter().any(|(n, _, _)| n == &m.name) {
                    self.err(
                        d(
                            "Y-T0009",
                            Severity::Error,
                            format!(
                                "`impl {} for {}` 实现了协议没有的方法 `{}`",
                                imp.proto, imp.for_, m.name
                            ),
                            m.span,
                        )
                        .with_suggestion("协议方法集 = impl 方法集（一操作一形式）"),
                    );
                }
            }
        }
        self.decls.impls = impls;
    }

    fn check_consts(&mut self) {
        let consts: Vec<(String, Ty, Option<ast::Expr>, Span)> = self
            .decls
            .consts
            .iter()
            .map(|(name, (ty, value, span))| {
                let r = self.resolver();
                (
                    name.clone(),
                    r.resolve(ty).unwrap_or(Ty::Unit),
                    value.clone(),
                    *span,
                )
            })
            .collect();
        for (_name, ty, value, _span) in consts {
            if let Some(v) = &value {
                let vt = self.check_expr(v, Some(&ty));
                if !vt.same(&ty) {
                    let s = v.span;
                    self.err(self.mismatch(ty, vt, s));
                }
            }
        }
    }

    fn check_fns(&mut self) {
        let program = self.program;
        for decl in &program.decls {
            match &decl.kind {
                ast::DeclKind::Fn(f) => self.check_fn(
                    f.type_params.clone(),
                    f.params
                        .iter()
                        .map(|p| (p.name.clone(), p.ty.clone()))
                        .collect(),
                    &f.ret,
                    Some(&f.body),
                    f.span,
                ),
                ast::DeclKind::Impl { fns, .. } => {
                    for f in fns {
                        self.check_fn(
                            Vec::new(), // impl 方法暂不承诺泛型（协议分发独立批次）
                            f.params
                                .iter()
                                .map(|p| (p.name.clone(), p.ty.clone()))
                                .collect(),
                            &f.ret,
                            Some(&f.body),
                            f.span,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn check_fn(
        &mut self,
        typarams: Vec<String>,
        params: Vec<(String, ast::Type)>,
        ret: &Option<ast::Type>,
        body: Option<&ast::FnBody>,
        span: Span,
    ) {
        // D-13：泛型形参注入 resolver（签名位 T → Ty::Generic），body 检查
        // 期间生效（let 注解等），退出恢复（非 fn 上下文零泄漏）。
        let saved = std::mem::replace(&mut self.fn_typarams, typarams);
        let resolved: Vec<Result<Ty, Diagnostic>> = {
            let r = self.resolver();
            params.iter().map(|(_, t)| r.resolve(t)).collect()
        };
        let ptys: Vec<Ty> = resolved
            .into_iter()
            .map(|x| {
                x.unwrap_or_else(|e| {
                    self.err(e);
                    Ty::Unit
                })
            })
            .collect();
        let rty = match ret {
            Some(t) => {
                let x = { self.resolver().resolve(t) };
                Some(x.unwrap_or_else(|e| {
                    self.err(e);
                    Ty::Unit
                }))
            }
            None => None,
        };
        if let Some(b) = body {
            self.fn_ret = rty.clone();
            self.scopes.push(Scope::default());
            for ((pname, _), ty) in params.iter().zip(&ptys) {
                self.scopes.last_mut().unwrap().vars.insert(
                    pname.clone(),
                    Binding {
                        ty: ty.clone(),
                        mutable: false,
                        used: false,
                    },
                );
            }
            let terminated = self.check_body(b);
            self.pop_scope();
            if !matches!(rty, Some(Ty::Unit) | None) && !terminated {
                self.err(
                    d(
                        "Y-T0016",
                        Severity::Error,
                        "函数可能未返回值（return 是唯一返回形式，19.2）",
                        span,
                    )
                    .with_suggestion("为所有分支补充 return，或将返回类型改为 unit"),
                );
            }
            self.fn_ret = None;
        }
        self.fn_typarams = saved;
    }

    fn pop_scope(&mut self) {
        let scope = self.scopes.pop().unwrap();
        for (name, b) in &scope.vars {
            if !b.used && name != "_" {
                self.diags.push(
                    d(
                        "W-T0002",
                        Severity::Warning,
                        format!("变量 `{name}` 未使用"),
                        Span::new(0, 0),
                    )
                    .with_suggestion("删除该绑定，或用 `let _ = ...` 显式丢弃"),
                );
            }
        }
    }

    fn report_unused_imports(&mut self) {
        let imports = std::mem::take(&mut self.decls.imports);
        for (bind, (path, module, span)) in &imports {
            if !*self.used_imports.get(bind).unwrap_or(&true) {
                self.diags.push(
                    d(
                        "W-T0003",
                        Severity::Warning,
                        format!("导入 `{bind}` 未使用（{path}/{module}）"),
                        *span,
                    )
                    .with_suggestion("删除该导入（决策 43：import 必用）"),
                );
            }
        }
        self.decls.imports = imports;
    }

    // ===== 函数体 / 块 =====

    /// 检查函数体；返回是否以 return/throw 终止。
    fn check_body(&mut self, body: &ast::FnBody) -> bool {
        match body {
            ast::FnBody::Expr(e) => {
                let exp = self.fn_ret.clone();
                let t = self.check_expr(e, exp.as_ref());
                if let Some(exp) = exp {
                    if !t.same(&exp) && !self.assignable(&t, &exp) {
                        let s = e.span;
                        self.err(self.mismatch(exp, t, s));
                    }
                }
                true
            }
            ast::FnBody::Block(b) => {
                let mut term = Term::No;
                self.check_block(b, &mut term);
                matches!(term, Term::Return | Term::Throw)
            }
        }
    }

    /// 检查块：返回（块值 = 末尾裸表达式类型，是否终止）。推送并弹出独立作用域。
    fn check_block(&mut self, block: &ast::Block, term: &mut Term) -> Ty {
        self.scopes.push(Scope::default());
        let mut value = Ty::Unit;
        for stmt in &block.stmts {
            if *term != Term::No {
                self.diags.push(
                    d("W-T0004", Severity::Warning, "不可达代码", stmt.span)
                        .with_suggestion("删除终止语句之后的代码"),
                );
                break;
            }
            if let Some(v) = self.check_stmt(stmt, term) {
                value = v;
            }
        }
        self.pop_scope();
        value
    }

    /// 返回 Some(ty) 仅当语句产出裸表达式值（块值语义用）。
    fn check_stmt(&mut self, stmt: &ast::Stmt, term: &mut Term) -> Option<Ty> {
        match &stmt.kind {
            StmtKind::Let {
                mut_,
                name,
                ty,
                value,
            } => {
                let expected = ty.as_ref().map(|t| {
                    let r = self.resolver();
                    r.resolve(t).unwrap_or_else(|e| {
                        self.err(e);
                        Ty::Unit
                    })
                });
                let vt = self.check_expr(value, expected.as_ref());
                // 绑定类型：有标注用标注（决策：局部推断但标注即权威）；
                // 无标注用值类型。nil 初始化必须带标注（推断无依据）。
                let bound = match &expected {
                    Some(exp) => {
                        if !self.assignable(&vt, exp) {
                            let s = stmt.span;
                            self.err(self.mismatch(exp.clone(), vt.clone(), s));
                        }
                        exp.clone()
                    }
                    None => {
                        if matches!(vt, Ty::Nil) {
                            self.err(
                                d(
                                    "Y-T0013",
                                    Severity::Error,
                                    "nil 初始化无法推断类型（决策 52）",
                                    stmt.span,
                                )
                                .with_suggestion("加类型标注：`let x: Item? = nil`"),
                            );
                            Ty::Unit
                        } else {
                            vt
                        }
                    }
                };
                if name != "_" {
                    self.scopes.last_mut().unwrap().vars.insert(
                        name.clone(),
                        Binding {
                            ty: bound,
                            mutable: *mut_,
                            used: false,
                        },
                    );
                }
                None
            }
            StmtKind::Expr(e) => {
                // if 语句位：流敏感收窄（决策 40）
                if let ExprKind::If { cond, then_, else_ } = &e.kind {
                    let (ty, then_term, else_facts) =
                        self.check_if(cond, then_, else_.as_deref(), None, e.span);
                    if matches!(ty, Ty::Res(..)) {
                        self.err(
                            d(
                                "W-T0001",
                                Severity::Warning,
                                "Result 值被忽略（决策 34）",
                                e.span,
                            )
                            .with_suggestion("用 `let _ = ...` 显式丢弃，或 `try` 传播错误"),
                        );
                    }
                    // then 分支终止 → else 事实在延续中成立
                    if then_term {
                        for (n, t) in else_facts {
                            self.scopes.last_mut().unwrap().facts.insert(n, t);
                        }
                    }
                    return None;
                }
                let t = self.check_expr(e, None);
                if matches!(t, Ty::Res(..)) {
                    self.err(
                        d(
                            "W-T0001",
                            Severity::Warning,
                            "Result 值被忽略（决策 34）",
                            e.span,
                        )
                        .with_suggestion("用 `let _ = ...` 显式丢弃，或 `try` 传播错误"),
                    );
                }
                Some(t)
            }
            StmtKind::While { cond, body } => {
                let ct = self.check_expr(cond, None);
                if ct != Ty::Bool {
                    let s = cond.span;
                    self.err(self.not_bool(ct, s));
                }
                self.loop_depth += 1;
                let mut inner = Term::No;
                self.check_block(body, &mut inner);
                self.loop_depth -= 1;
                None
            }
            StmtKind::For { var, iter, body } => {
                let it = self.check_expr(iter, None);
                let vty = match &it {
                    Ty::List(t) => (**t).clone(),
                    Ty::Str => Ty::Rune,  // 决策 46：按码点迭代
                    Ty::Range => Ty::Int, // 决策 77：range 产物迭代为 int（VM ForPrep 同构）
                    // 决策 82 第二步（泛型批次落地）：Map 迭代产出 KV entry
                    // struct（`.key`/`.value`，VM Cursor::Map 同语义）；键
                    // 通道迁移为 `for k in keys(m)`（keys 内建既有）。
                    Ty::Map(k, v) => Ty::named("KV", vec![(**k).clone(), (**v).clone()]),
                    other => {
                        let s = iter.span;
                        self.err(
                            d(
                                "Y-T0002",
                                Severity::Error,
                                format!("for 只能迭代 list/string/map/range，得到 `{other}`"),
                                s,
                            )
                        .with_suggestion(
                            "Map 迭代产出 KV entry（.key/.value）；纯键用 `for k in keys(m)`（决策 82）",
                        ),
                        );
                        Ty::Unit
                    }
                };
                self.scopes.push(Scope::default());
                self.scopes.last_mut().unwrap().vars.insert(
                    var.clone(),
                    Binding {
                        ty: vty,
                        mutable: false,
                        used: true,
                    }, // for 变量不参与未使用警告
                );
                self.loop_depth += 1;
                let mut inner = Term::No;
                self.check_block(body, &mut inner);
                self.loop_depth -= 1;
                self.scopes.pop();
                None
            }
            StmtKind::Return(e) => {
                match (&self.fn_ret.clone(), e) {
                    (Some(exp), Some(expr)) => {
                        let t = self.check_expr(expr, Some(exp));
                        if !t.same(exp) && !self.assignable(&t, exp) {
                            let s = expr.span;
                            self.err(self.mismatch(exp.clone(), t, s));
                        }
                    }
                    (Some(Ty::Unit), None) => {}
                    (Some(exp), None) => {
                        self.err(d(
                            "Y-T0002",
                            Severity::Error,
                            format!("裸 return 与返回类型 `{exp}` 不符"),
                            stmt.span,
                        ));
                    }
                    (None, Some(expr)) => {
                        self.check_expr(expr, None);
                    }
                    (None, None) => {}
                }
                *term = Term::Return;
                None
            }
            StmtKind::Throw(e) => {
                let t = self.check_expr(e, None);
                match &self.fn_ret.clone() {
                    Some(Ty::Res(_, err)) => {
                        if !t.same(err) {
                            let s = e.span;
                            self.err(self.mismatch((**err).clone(), t, s));
                        }
                    }
                    Some(other) => {
                        self.err(d(
                            "Y-T0032",
                            Severity::Error,
                            format!("throw 需要函数返回 Result，当前返回 `{other}`"),
                            stmt.span,
                        ));
                    }
                    None => {
                        self.err(d(
                            "Y-T0032",
                            Severity::Error,
                            "throw 需要函数返回 Result",
                            stmt.span,
                        ));
                    }
                }
                *term = Term::Throw;
                None
            }
            StmtKind::Break | StmtKind::Continue => {
                if self.loop_depth == 0 {
                    self.err(d(
                        "Y-T0031",
                        Severity::Error,
                        "break/continue 只能出现在循环内（决策 47：无标签形式）",
                        stmt.span,
                    ));
                }
                *term = Term::Loop;
                None
            }
            StmtKind::Block(b) => {
                let mut inner = Term::No;
                self.check_block(b, &mut inner);
                None
            }
        }
    }

    // ===== nil 收窄（决策 40）=====

    /// 从条件提取收窄事实：(then 事实, else 事实)。
    /// 仅处理单比较与取反——复合条件不收窄（保守，无假收窄）。
    fn narrow_facts(&self, cond: &Expr) -> (Facts, Facts) {
        let (lhs, rhs, is_eq) = match &cond.kind {
            ExprKind::Binary {
                op: BinOp::Eq,
                lhs,
                rhs,
            } => (lhs, rhs, true),
            ExprKind::Binary {
                op: BinOp::Ne,
                lhs,
                rhs,
            } => (lhs, rhs, false),
            ExprKind::Unary {
                op: UnOp::Not,
                operand,
            } => {
                let (t, e) = self.narrow_facts(operand);
                return (e, t);
            }
            _ => return (Vec::new(), Vec::new()),
        };
        let var_expr = if matches!(rhs.kind, ExprKind::Nil) {
            lhs
        } else if matches!(lhs.kind, ExprKind::Nil) {
            rhs
        } else {
            return (Vec::new(), Vec::new());
        };
        let name = match &var_expr.kind {
            ExprKind::Ident(n) => n.clone(),
            _ => return (Vec::new(), Vec::new()),
        };
        let base = match self.lookup_declared(&name) {
            Some(Ty::Opt(t)) => (*t).clone(),
            _ => return (Vec::new(), Vec::new()),
        };
        // `x != nil` → then 收窄；`x == nil` → else 收窄
        if !is_eq {
            (vec![(name, base)], Vec::new())
        } else {
            (Vec::new(), vec![(name, base)])
        }
    }

    fn lookup_declared(&self, name: &str) -> Option<Ty> {
        for s in self.scopes.iter().rev() {
            if let Some(b) = s.vars.get(name) {
                return Some(b.ty.clone());
            }
        }
        None
    }

    /// if 的统一检查（表达式/语句位共用）。返回（类型, then 是否终止, else 事实）。
    fn check_if(
        &mut self,
        cond: &Expr,
        then_: &ast::Block,
        else_: Option<&Expr>,
        expected: Option<&Ty>,
        span: Span,
    ) -> (Ty, bool, Vec<(String, Ty)>) {
        let ct = self.check_expr(cond, None);
        if ct != Ty::Bool {
            let s = cond.span;
            self.err(self.not_bool(ct, s));
        }
        let (then_facts, else_facts) = self.narrow_facts(cond);
        self.scopes.push(Scope {
            vars: HashMap::new(),
            facts: then_facts.into_iter().collect(),
        });
        let mut term = Term::No;
        let tt = self.check_block(then_, &mut term);
        let then_term = matches!(term, Term::Return | Term::Throw);
        self.pop_scope();
        let (ety, _else_term) = match else_ {
            Some(e) => {
                self.scopes.push(Scope {
                    vars: HashMap::new(),
                    facts: else_facts.iter().cloned().collect(),
                });
                let t = self.check_expr(e, Some(&tt));
                self.pop_scope();
                (t, true)
            }
            None => {
                if !matches!(expected, None | Some(Ty::Unit)) {
                    self.err(
                        d("Y-T0029", Severity::Error, "if 作为值使用必须带 else", span)
                            .with_suggestion("补全 else 分支，或将 if 改为语句"),
                    );
                }
                (Ty::Unit, false)
            }
        };
        if else_.is_some() && !ety.same(&tt) {
            let s = span;
            self.err(self.mismatch(tt.clone(), ety, s));
        }
        (tt, then_term, else_facts)
    }

    // ===== 名字解析 =====

    /// 无诊断副作用解析（决策 78 配套）：Field 分支的模块探针专用——
    /// 未命中/非模块时交回 `check_expr(base)` 单次发射，消除
    /// 「lookup 发一次 + check_expr 再发一次」的同 span 双诊断。
    /// 第二返回值 = 变体带载荷裸用的待报义务（发射保留在 `lookup`）。
    fn lookup_opt(&mut self, name: &str) -> Option<(Resolved, bool)> {
        // 收窄事实优先（决策 40），随后是普通绑定——先取出克隆再标记使用，避免借用冲突
        if let Some(t) = self.find_in_scopes(name) {
            self.mark_used(name);
            return Some((Resolved::Ty(t), false));
        }
        if let Some(f) = self.decls.fns.get(name) {
            // D-13：被调泛型 fn 的签名解析注入其形参（T → Ty::Generic），
            // 返回含 Generic 的签名，apply_sig 调用位实例化。显式构造，
            // 不用 self.resolver()（那带的是「当前 fn」的形参，语义不同）。
            let r = Resolver {
                decls: &self.decls,
                env: self.env,
                subst: f
                    .type_params
                    .iter()
                    .cloned()
                    .map(|t| (t.clone(), Ty::Generic(t)))
                    .collect(),
            };
            let ps: Vec<Ty> = f
                .params
                .iter()
                .map(|(_, t)| r.resolve(t).unwrap_or(Ty::Unit))
                .collect();
            let ret = f
                .ret
                .as_ref()
                .map(|t| r.resolve(t).unwrap_or(Ty::Unit))
                .unwrap_or(Ty::Unit);
            return Some((Resolved::Ty(Ty::Fn(ps, Box::new(ret))), false));
        }
        if let Some((ty, _, _)) = self.decls.consts.get(name) {
            let r = self.resolver();
            let t = r.resolve(ty).unwrap_or(Ty::Unit);
            return Some((Resolved::Ty(t), false));
        }
        if let Some((owner, payload)) = self.decls.variants.get(name).cloned() {
            return Some((Resolved::Ty(Ty::named(&owner, vec![])), payload.is_some()));
        }
        if let Some((path, module, _)) = self.decls.imports.get(name).cloned() {
            self.used_imports.insert(name.to_string(), true);
            // path 仅校验期用（Checker::new 的模块存在性检查）；函数面查注册表
            // （决策 77），Resolved 只携带模块名。
            let _ = path;
            return Some((Resolved::Module { module }, false));
        }
        if let Some(t) = self.env.host_types.get(name) {
            return Some((Resolved::Ty(t.clone()), false));
        }
        if let Some(t) = self.env.free_fns.get(name) {
            return Some((Resolved::Ty(t.clone()), false));
        }
        None
    }

    fn lookup(&mut self, name: &str, span: Span) -> Resolved {
        match self.lookup_opt(name) {
            Some((r, variant_payload_err)) => {
                if variant_payload_err {
                    self.err(d(
                        "Y-T0002",
                        Severity::Error,
                        format!("变体 `{name}` 带载荷，需以 `{name}(...)` 构造"),
                        span,
                    ));
                }
                r
            }
            None => {
                let names: Vec<String> = self.visible_names();
                let mut dg = d(
                    "Y-T0001",
                    Severity::Error,
                    format!("未定义的名称 `{name}`"),
                    span,
                );
                if let Some(s) = did_you_mean(name, &names) {
                    dg = dg.with_suggestion(s);
                }
                self.err(dg);
                Resolved::Ty(Ty::Unit)
            }
        }
    }

    /// 作用域查找：收窄事实优先于普通绑定（决策 40）。
    fn find_in_scopes(&self, name: &str) -> Option<Ty> {
        for s in self.scopes.iter().rev() {
            if let Some(t) = s.facts.get(name) {
                return Some(t.clone());
            }
        }
        for s in self.scopes.iter().rev() {
            if let Some(b) = s.vars.get(name) {
                return Some(b.ty.clone());
            }
        }
        None
    }

    fn mark_used(&mut self, name: &str) {
        for s in self.scopes.iter_mut().rev() {
            if let Some(b) = s.vars.get_mut(name) {
                b.used = true;
                return;
            }
        }
    }

    fn visible_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        names.extend(self.decls.fns.keys().cloned());
        names.extend(self.decls.consts.keys().cloned());
        names.extend(self.decls.imports.keys().cloned());
        names.extend(self.env.free_fns.keys().cloned());
        names.extend(self.env.host_types.keys().cloned());
        names
    }

    // ===== 可赋值性 =====

    /// 相等，或 T → T? 加宽（决策 27）。
    fn assignable(&self, actual: &Ty, expected: &Ty) -> bool {
        if actual.same(expected) {
            return true;
        }
        match (actual, expected) {
            (Ty::Nil, Ty::Opt(_)) => true,
            (a, Ty::Opt(e)) => a.same(e),
            _ => false,
        }
    }

    fn mismatch(&self, expected: Ty, actual: Ty, span: Span) -> Diagnostic {
        if matches!(expected, Ty::Int | Ty::Float)
            && matches!(actual, Ty::Int | Ty::Float)
            && expected != actual
        {
            return d(
                "Y-T0003",
                Severity::Error,
                format!("int 与 float 不可混算：期望 `{expected}`，得到 `{actual}`"),
                span,
            )
            .with_suggestion(format!(
                "显式转换：`... as {expected}`（决策 26：as 是唯一转换）"
            ));
        }
        d(
            "Y-T0002",
            Severity::Error,
            format!("类型不匹配：期望 `{expected}`，得到 `{actual}`"),
            span,
        )
        .with_suggestion("检查值来源；int/float 转换用 `as`，可空值用 `!` 解包或 nil 检查")
    }

    fn not_bool(&self, actual: Ty, span: Span) -> Diagnostic {
        d(
            "Y-T0005",
            Severity::Error,
            format!("条件必须是 bool（无 truthiness，19.2），得到 `{actual}`"),
            span,
        )
        .with_suggestion("显式比较：`x != nil`、`x == 0` 等")
    }

    // ===== 表达式 =====

    fn check_expr(&mut self, e: &Expr, expected: Option<&Ty>) -> Ty {
        match &e.kind {
            ExprKind::Int(_) => Ty::Int,
            ExprKind::Float(_) => Ty::Float,
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Nil => Ty::Nil,
            ExprKind::Str(parts) => {
                for p in parts {
                    if let ast::StrItem::Interp(inner) = p {
                        self.check_expr(inner, None); // 显示位任意类型（Display 转换）
                    }
                }
                Ty::Str
            }
            ExprKind::Ident(n) => match self.lookup(n, e.span) {
                Resolved::Ty(t) => t,
                Resolved::Module { module, .. } => {
                    self.err(d(
                        "Y-T0002",
                        Severity::Error,
                        format!("模块 `{module}` 不能作为值使用"),
                        e.span,
                    ));
                    Ty::Unit
                }
            },
            ExprKind::Lambda { params, body } => self.check_lambda(params, body, expected),
            ExprKind::If { cond, then_, else_ } => {
                let (t, _, _) = self.check_if(cond, then_, else_.as_deref(), expected, e.span);
                t
            }
            ExprKind::Match { scrutinee, arms } => self.check_match(scrutinee, arms, expected),
            ExprKind::Try(operand) => {
                let t = self.check_expr(operand, None);
                match (&t, self.fn_ret.clone()) {
                    (Ty::Res(o, err), Some(Ty::Res(_, ferr))) if err.same(&ferr) => (**o).clone(),
                    (Ty::Res(_, err), Some(fr)) => {
                        self.err(
                            d(
                                "Y-T0019",
                                Severity::Error,
                                format!("try 的错误类型 `{err}` 与函数返回类型 `{fr}` 不一致"),
                                e.span,
                            )
                            .with_suggestion("统一错误类型，或调整函数返回签名"),
                        );
                        Ty::Unit
                    }
                    (Ty::Opt(t), Some(Ty::Opt(_))) => (**t).clone(), // 决策 54：nil 传播
                    (Ty::Opt(_), Some(fr)) => {
                        self.err(d(
                            "Y-T0019",
                            Severity::Error,
                            format!("try 传播 nil 要求函数返回 `T?`，当前 `{fr}`（决策 54）"),
                            e.span,
                        ));
                        Ty::Unit
                    }
                    (_, None) => {
                        self.err(d(
                            "Y-T0019",
                            Severity::Error,
                            "try 只能在返回 Result/T? 的函数内使用",
                            e.span,
                        ));
                        Ty::Unit
                    }
                    (other, Some(fr)) => {
                        self.err(d(
                            "Y-T0019",
                            Severity::Error,
                            format!(
                                "try 操作数须为 Result/T?，得到 `{other}`（函数返回 `{fr}`）"
                            ),
                            e.span,
                        )
                        .with_suggestion(
                            "失败路径词汇表：try（传播）/ !（解包）/ throw（构造）/ match（处理）",
                        ));
                        Ty::Unit
                    }
                }
            }
            ExprKind::TryCatch { inner, arms } => {
                // 决策 87 阶段①：throw 通道的本地捕获。错误值类型任意
                // （throw 可抛任何值）→ 臂模式对 Ty::Any 检查、必须含兜底臂
                // （check_arms 内 Any 非穷尽路径天然强制）；臂结果须与 try 体
                // 类型统一（唯一表达式类型，无 union）。
                let t = self.check_expr(inner, None);
                let u = self.check_arms(&Ty::Any, arms, Some(&t), "catch", e.span);
                if !u.same(&t) {
                    self.err(self.mismatch(t.clone(), u, e.span));
                }
                t
            }
            ExprKind::Await(operand) => {
                let t = self.check_expr(operand, None);
                match t {
                    Ty::Async(inner) => *inner,
                    other => {
                        self.err(d(
                            "Y-T0020",
                            Severity::Error,
                            format!("await 仅作用于宿主异步调用结果（14.5），得到 `{other}`"),
                            e.span,
                        ));
                        Ty::Unit
                    }
                }
            }
            ExprKind::Unary { op, operand } => {
                let t = self.check_expr(operand, None);
                match op {
                    UnOp::Not => {
                        if t != Ty::Bool {
                            let s = e.span;
                            self.err(self.not_bool(t, s));
                        }
                        Ty::Bool
                    }
                    UnOp::Neg => match t {
                        Ty::Int | Ty::Float => t,
                        other => {
                            let s = e.span;
                            self.err(self.mismatch(Ty::Int, other, s));
                            Ty::Int
                        }
                    },
                }
            }
            ExprKind::Binary { op, lhs, rhs } => self.check_binary(*op, lhs, rhs, expected, e.span),
            ExprKind::Call { callee, args } => {
                self.check_call(callee, args, expected, None, e.span)
            }
            ExprKind::Field { base, field } => {
                // D1 值位归一（决策 78）：模块限定内建名作值（`map(xs, math.floor)`）
                // ——与调用位（check_call Field 分支）同一判定：base 是 import 的
                // std 模块且 field 命中注册表，即得内建函数面，与裸名作值
                // （`map(xs, floor)`，Const(Str) 一等值）完全同构。lower 侧
                // module_builtin_ref 两位置归一已就位（决策 77），此处补 typeck
                // 面，消除「模块不能作为值」→「unit 无字段」级联误诊。
                if let ExprKind::Ident(n) = &base.kind {
                    // 探针走 lookup_opt（无诊断副作用）：未命中/非模块时落到
                    // 下方 check_expr(base) 单次发射——修复前此处先 lookup
                    // （发 Y-T0001）再 check_expr（再发一次），同 span 双诊断。
                    if let Some((Resolved::Module { module }, _)) = self.lookup_opt(n) {
                        if let Some(b) = yoru_stdlib::module_fn(&module, field) {
                            return crate::env::builtin_face(b);
                        }
                        // std 模块面无此函数：与调用位同一诊断（Y-T0007），
                        // 不再落入「模块不能作为值」的误导性级联。
                        if yoru_stdlib::BUILTINS
                            .iter()
                            .any(|b| b.module == Some(module.as_str()))
                        {
                            self.err(d(
                                "Y-T0007",
                                Severity::Error,
                                format!("模块 `{module}` 没有函数 `{field}`"),
                                e.span,
                            ));
                            return Ty::Unit;
                        }
                        // 宿主模块（非注册表）：模块值维持原边界，落到
                        // check_expr(base) 单次发射「模块不能作为值」。
                    }
                }
                let bt = self.check_expr(base, None);
                self.check_field(bt, field, e.span, base.span)
            }
            ExprKind::Index { base, index } => {
                let bt = self.check_expr(base, None);
                self.check_index(bt, index, e.span)
            }
            ExprKind::Unwrap(operand) => {
                let t = self.check_expr(operand, None);
                match t {
                    Ty::Opt(inner) => *inner,
                    other => {
                        // D-04（26 号登记）：nil 收窄（决策 40）后的 `T` 上允许 `!`
                        // 作恒等解包——值已非可空，`!` 不再要求重复 nil 检查
                        //（诊断自相矛盾：帮助文本自己建议「先做 nil 检查」）。
                        let narrowed = matches!(
                            &operand.kind,
                            ExprKind::Ident(name) if self
                                .scopes
                                .iter()
                                .rev()
                                .any(|s| s.facts.contains_key(name))
                        );
                        if narrowed {
                            other
                        } else {
                            self.err(
                                d(
                                    "Y-T0004",
                                    Severity::Error,
                                    format!("`!` 仅可解包 `T?`，得到 `{other}`"),
                                    e.span,
                                )
                                .with_suggestion(
                                    "先做 nil 检查获得智能收窄（决策 40），或用 `try` 传播",
                                ),
                            );
                            Ty::Unit
                        }
                    }
                }
            }
            ExprKind::Cast { operand, ty } => {
                let t = self.check_expr(operand, None);
                let target = { self.resolver().resolve(ty) }.unwrap_or_else(|err| {
                    self.err(err);
                    Ty::Unit
                });
                self.check_cast(&t, &target, e.span);
                target
            }
            ExprKind::StructLit {
                name,
                type_args,
                inits,
                spread,
            } => self.check_struct_lit(name, type_args, inits, spread.as_deref(), e.span),
            ExprKind::List(items) => {
                if items.is_empty() {
                    return match expected {
                        Some(Ty::List(t)) => Ty::list((**t).clone()),
                        _ => {
                            self.err(
                                d(
                                    "Y-T0013",
                                    Severity::Error,
                                    "空 list 字面量无法推断元素类型（决策 52）",
                                    e.span,
                                )
                                .with_suggestion("加类型标注：`let xs: [int] = []`"),
                            );
                            Ty::list(Ty::Unit)
                        }
                    };
                }
                // 元素类型：上下文期望 [T?]（等标注）时以其驱动——元素经
                // assignable（Nil→T?、T→T? 兼容）判定；无标注时按首元素推导。
                // 泛型模式位（Ty::Generic，如 map 的 [T] 参数）不吃 expected——
                // 首元素反推正是 T 的绑定来源。
                let elem = match expected {
                    Some(Ty::List(t)) if !matches!(**t, Ty::Generic(_)) => {
                        let et = (**t).clone();
                        let t0 = self.check_expr(&items[0], Some(&et));
                        if !self.assignable(&t0, &et) {
                            let s = items[0].span;
                            self.err(self.mismatch(et.clone(), t0, s));
                        }
                        et
                    }
                    _ => self.check_expr(&items[0], None),
                };
                for item in &items[1..] {
                    let t = self.check_expr(item, Some(&elem));
                    if !self.assignable(&t, &elem) {
                        let s = item.span;
                        self.err(self.mismatch(elem.clone(), t, s));
                    }
                }
                Ty::list(elem)
            }
            ExprKind::Map(items) => {
                if items.is_empty() {
                    return match expected {
                        Some(Ty::Map(k, v)) => Ty::Map(k.clone(), v.clone()),
                        _ => {
                            self.err(
                                d(
                                    "Y-T0013",
                                    Severity::Error,
                                    "空 map 字面量无法推断键值类型（决策 52）",
                                    e.span,
                                )
                                .with_suggestion("加类型标注：`let m: [string: int] = [:]`"),
                            );
                            Ty::Map(Box::new(Ty::Str), Box::new(Ty::Unit))
                        }
                    };
                }
                // 与 List 同构：上下文期望 [K: V?] 时以标注驱动键值类型，
                // 元素经 assignable 判定（nil 值 → T? 通过，决策 82）。
                let (kt, vt) = match expected {
                    Some(Ty::Map(k, v))
                        if !matches!(**k, Ty::Generic(_)) && !matches!(**v, Ty::Generic(_)) =>
                    {
                        let (kt, vt) = ((**k).clone(), (**v).clone());
                        let (k0, v0) = &items[0];
                        let kt0 = self.check_expr(k0, Some(&kt));
                        if !self.assignable(&kt0, &kt) {
                            let s = k0.span;
                            self.err(self.mismatch(kt.clone(), kt0, s));
                        }
                        let vt0 = self.check_expr(v0, Some(&vt));
                        if !self.assignable(&vt0, &vt) {
                            let s = v0.span;
                            self.err(self.mismatch(vt.clone(), vt0, s));
                        }
                        (kt, vt)
                    }
                    _ => {
                        let (k0, v0) = &items[0];
                        (self.check_expr(k0, None), self.check_expr(v0, None))
                    }
                };
                for (k, v) in &items[1..] {
                    let kti = self.check_expr(k, Some(&kt));
                    if !self.assignable(&kti, &kt) {
                        let s = k.span;
                        self.err(self.mismatch(kt.clone(), kti, s));
                    }
                    let vti = self.check_expr(v, Some(&vt));
                    if !self.assignable(&vti, &vt) {
                        let s = v.span;
                        self.err(self.mismatch(vt.clone(), vti, s));
                    }
                }
                Ty::Map(Box::new(kt), Box::new(vt))
            }
            ExprKind::Block(b) => {
                let mut term = Term::No;
                self.check_block(b, &mut term)
            }
            ExprKind::Assign { target, op, value } => self.check_assign(target, *op, value, e.span),
        }
    }

    fn check_lambda(
        &mut self,
        params: &[ast::LamParam],
        body: &ast::FnBody,
        expected: Option<&Ty>,
    ) -> Ty {
        let formal: Option<(Vec<Ty>, Ty)> = match expected {
            Some(Ty::Fn(ps, r)) => Some((ps.clone(), (**r).clone())),
            _ => None,
        };
        let any_untyped = params.iter().any(|p| p.ty.is_none());
        if any_untyped && formal.is_none() {
            let span = params.first().map(|p| p.span).unwrap_or(Span::new(0, 0));
            self.err(
                d(
                    "Y-T0014",
                    Severity::Error,
                    "lambda 参数类型无法推断（需要调用上下文）",
                    span,
                )
                .with_suggestion("为参数补类型标注，或确保 lambda 直接作为函数实参"),
            );
            return Ty::Fn(vec![], Box::new(Ty::Unit));
        }
        let mut ptys: Vec<Ty> = Vec::new();
        self.scopes.push(Scope::default());
        for (i, p) in params.iter().enumerate() {
            let ty = match (&p.ty, &formal) {
                (Some(t), _) => {
                    let r = self.resolver();
                    r.resolve(t).unwrap_or(Ty::Unit)
                }
                (None, Some((fps, _))) => fps.get(i).cloned().unwrap_or(Ty::Unit),
                (None, None) => Ty::Unit,
            };
            ptys.push(ty.clone());
            self.scopes.last_mut().unwrap().vars.insert(
                p.name.clone(),
                Binding {
                    ty,
                    mutable: false,
                    used: false,
                },
            );
        }
        let body_ty = match body {
            ast::FnBody::Expr(e) => self.check_expr(e, formal.as_ref().map(|(_, r)| r)),
            ast::FnBody::Block(b) => {
                let mut term = Term::No;
                self.check_block(b, &mut term)
            }
        };
        self.pop_scope();
        if let Some((_, fr)) = &formal {
            // 泛型返回类型（如 map 的 U）由调用方 unify 绑定，此处不报
            if !contains_generic(fr) && !body_ty.same(fr) && !self.assignable(&body_ty, fr) {
                // lambda 体与形参返回类型不符——直接报，保证错误不静默
                let span = body_expr_span(body);
                self.err(self.mismatch(fr.clone(), body_ty.clone(), span));
            }
        }
        Ty::Fn(ptys, Box::new(body_ty))
    }

    fn check_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[ast::MatchArm],
        expected: Option<&Ty>,
    ) -> Ty {
        let st = self.check_expr(scrutinee, None);
        self.check_arms(&st, arms, expected, "match", scrutinee.span)
    }

    /// match/catch 共用的臂检查（决策 87）：逐臂 check_pattern → 绑定入作用域
    /// → 臂体类型统一；穷尽性 = 兜底臂或 tag/Result 全变体覆盖。
    /// `what` 仅用于错误文案（"match" / "catch"）。
    fn check_arms(
        &mut self,
        st: &Ty,
        arms: &[ast::MatchArm],
        expected: Option<&Ty>,
        what: &str,
        st_span: Span,
    ) -> Ty {
        let mut result: Option<Ty> = None;
        let mut covered: Vec<String> = Vec::new();
        let mut catch_all = false;
        for arm in arms {
            self.scopes.push(Scope::default());
            let binds = self.check_pattern(&arm.pattern, st, arm.span);
            match &arm.pattern {
                Pattern::Wildcard => catch_all = true,
                Pattern::Bind(n) => {
                    if n == "_" {
                        catch_all = true;
                    } else if let Some((owner, payload)) = self.decls.variants.get(n) {
                        let is_scrut_variant = matches!(st, Ty::Named { name, .. } if name == owner)
                            && payload.is_none();
                        if is_scrut_variant {
                            covered.push(n.clone());
                        } else {
                            catch_all = true; // 绑定模式恒匹配
                        }
                    } else {
                        catch_all = true;
                    }
                }
                Pattern::Variant { name, .. } => covered.push(name.clone()),
                _ => {} // 字面量/nil 模式不提供穷尽性
            }
            for (n, t) in binds {
                self.scopes.last_mut().unwrap().vars.insert(
                    n,
                    Binding {
                        ty: t,
                        mutable: false,
                        used: true,
                    },
                );
            }
            let bt = self.check_expr(&arm.body, expected.or(result.as_ref()));
            match &result {
                None => result = Some(bt),
                Some(prev) => {
                    if !bt.same(prev) {
                        let s = arm.span;
                        self.err(self.mismatch(prev.clone(), bt, s));
                    }
                }
            }
            self.pop_scope();
        }
        // 穷尽性（决策：match 穷尽处理；catch 的错误值类型任意 → 必有兜底臂）
        let exhaustive = catch_all
            || match st {
                Ty::Named { name, .. } => match self.decls.types.get(name) {
                    Some(TypeKind::Tag { variants, .. }) => {
                        variants.iter().all(|(vn, _)| covered.contains(vn))
                    }
                    _ => false,
                },
                // 内建 Result：Ok/Err 双臂齐备即穷尽（决策 54）
                Ty::Res(..) => {
                    covered.contains(&"Ok".to_string()) && covered.contains(&"Err".to_string())
                }
                _ => false,
            };
        if !exhaustive {
            self.err(
                d(
                    "Y-T0015",
                    Severity::Error,
                    format!("{what} 未穷尽所有情况（决策：穷尽处理）"),
                    st_span,
                )
                .with_suggestion("补齐缺失变体分支，或加兜底分支（绑定名 / _）"),
            );
        }
        result.unwrap_or(Ty::Unit)
    }

    fn check_pattern(&mut self, pat: &Pattern, scrut: &Ty, span: Span) -> Vec<(String, Ty)> {
        // Ty::Any（catch 错误值类型，决策 87）：throw 可抛任意值，字面量模式
        // 恒可能命中；变体模式在 Variant 臂单独放行（只查存在性）。
        let any = scrut == &Ty::Any;
        match pat {
            Pattern::Int(_) if scrut == &Ty::Int || any => vec![],
            Pattern::Float(_) if scrut == &Ty::Float || any => vec![],
            Pattern::Bool(_) if scrut == &Ty::Bool || any => vec![],
            Pattern::Str(_) if scrut == &Ty::Str || any => vec![],
            Pattern::Nil => {
                if !matches!(scrut, Ty::Opt(_)) {
                    self.err(d(
                        "Y-T0002",
                        Severity::Error,
                        format!("nil 模式要求 `T?`，得到 `{scrut}`"),
                        span,
                    ));
                }
                vec![]
            }
            Pattern::Bind(n) => {
                if n == "_" {
                    return vec![];
                }
                if let Some((owner, payload)) = self.decls.variants.get(n) {
                    if payload.is_none() && scrut.same(&Ty::named(owner, vec![])) {
                        return vec![];
                    }
                }
                vec![(n.clone(), scrut.clone())]
            }
            Pattern::Variant { name, payload } => {
                // 内建 Result 变体（决策 54）：`Ty::Res` 的 Ok/Err 模式。
                // 运行时（lower/VM）早已支持而 typeck 缺位——Result-match 被
                // Y-T0002 整体拒绝（showcase.yoru 红证，26 号类落差）。
                // 语义对齐 lower：payload 是绑定名（`Err(e)` 绑 e: err 位；
                // `Err(v)` 绑 v 为载荷值，不做嵌套变体检查）。
                if name == "Ok" || name == "Err" {
                    if let Ty::Res(o, e) = scrut {
                        let want = if name == "Ok" {
                            (**o).clone()
                        } else {
                            (**e).clone()
                        };
                        return match payload {
                            Some(Payload::Bind(p)) => vec![(p.clone(), want)],
                            Some(Payload::Ignore) | None => vec![], // 载荷被忽略（lower Pop）
                        };
                    }
                }
                let (owner, tag_args) = match scrut {
                    // Ty::Any（catch 错误值，决策 87）：owner 未知——变体只查
                    // 存在性，载荷类型按声明原样绑定（无 tag 实参代换）。
                    Ty::Any => (String::new(), Vec::new()),
                    Ty::Named { name, args } => (name.clone(), args.clone()),
                    other => {
                        self.err(d(
                            "Y-T0002",
                            Severity::Error,
                            format!("变体模式要求 tag 类型，得到 `{other}`"),
                            span,
                        ));
                        return vec![];
                    }
                };
                match self.decls.variants.get(name).cloned() {
                    Some((tag, pty)) if tag == owner || owner.is_empty() => {
                        match (pty, payload) {
                            (Some(pt), Some(Payload::Bind(binder))) => {
                                // 载荷类型以 tag 实参代换（Tree[int] → Node2[int]）
                                let subst = self.tag_subst(&owner, &tag_args);
                                let r = Resolver {
                                    decls: &self.decls,
                                    env: self.env,
                                    subst,
                                };
                                let t = r.resolve(&pt).unwrap_or(Ty::Unit);
                                vec![(binder.clone(), t)]
                            }
                            // `Node(_)`：带载荷变体的显式丢弃（决策 86）
                            (Some(_), Some(Payload::Ignore)) => vec![],
                            (None, None) => vec![],
                            (Some(_), None) => {
                                self.err(d(
                                "Y-T0002",
                                Severity::Error,
                                format!("变体 `{name}` 带载荷，需 `{name}(p)` 绑定或 `{name}(_)` 丢弃"),
                                span,
                            ));
                                vec![]
                            }
                            (None, Some(_)) => {
                                self.err(d(
                                    "Y-T0002",
                                    Severity::Error,
                                    format!("变体 `{name}` 无载荷，不能绑定"),
                                    span,
                                ));
                                vec![]
                            }
                        }
                    }
                    _ => {
                        let msg = if owner.is_empty() {
                            format!("未知变体 `{name}`")
                        } else {
                            format!("`{owner}` 没有变体 `{name}`")
                        };
                        self.err(d("Y-T0002", Severity::Error, msg, span));
                        vec![]
                    }
                }
            }
            Pattern::Wildcard => vec![],
            _ => {
                self.err(d(
                    "Y-T0002",
                    Severity::Error,
                    format!("字面量模式与 scrutinee `{scrut}` 类型不符"),
                    span,
                ));
                vec![]
            }
        }
    }

    fn tag_subst(&self, tag: &str, args: &[Ty]) -> HashMap<String, Ty> {
        match self.decls.types.get(tag) {
            Some(TypeKind::Tag { params, .. }) => {
                params.iter().cloned().zip(args.iter().cloned()).collect()
            }
            _ => HashMap::new(),
        }
    }

    fn check_binary(
        &mut self,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
        expected: Option<&Ty>,
        span: Span,
    ) -> Ty {
        if op == BinOp::Pipe {
            // 管道左线程（决策 25）：x |> f(y) = f(x, y)
            let lt = self.check_expr(lhs, None);
            return match &rhs.kind {
                ExprKind::Call { callee, args } => {
                    self.check_call(callee, args, expected, Some(lt), rhs.span)
                }
                _ => {
                    self.err(d(
                        "Y-T0021",
                        Severity::Error,
                        "管道右侧必须是函数调用（决策 25：左线程 x |> f(y) = f(x, y)）",
                        rhs.span,
                    ));
                    Ty::Unit
                }
            };
        }
        let lt = self.check_expr(lhs, None);
        let rt = self.check_expr(rhs, None);
        match op {
            BinOp::Or | BinOp::And => {
                if lt != Ty::Bool || rt != Ty::Bool {
                    self.err(d(
                        "Y-T0002",
                        Severity::Error,
                        format!("逻辑运算要求 bool，得到 `{lt}` / `{rt}`"),
                        span,
                    ));
                }
                Ty::Bool
            }
            BinOp::Eq | BinOp::Ne => {
                self.check_eq(&lt, &rt, span);
                Ty::Bool
            }
            BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                if !(lt.same(&rt) && matches!(lt, Ty::Int | Ty::Float)) {
                    self.err(
                        d(
                            "Y-T0002",
                            Severity::Error,
                            format!("比较要求同型 int/float，得到 `{lt}` / `{rt}`"),
                            span,
                        )
                        .with_suggestion("float 排序走 sort_floats（决策 45：float 无全序）"),
                    );
                }
                Ty::Bool
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
                self.check_arith(op, &lt, &rt, span)
            }
            BinOp::Mod => {
                if lt == Ty::Int && rt == Ty::Int {
                    Ty::Int
                } else {
                    self.err(d(
                        "Y-T0002",
                        Severity::Error,
                        format!("% 是 floor-mod，仅限 int（决策 44），得到 `{lt}` / `{rt}`"),
                        span,
                    ));
                    Ty::Int
                }
            }
            BinOp::BitOr | BinOp::BitXor | BinOp::BitAnd | BinOp::Shl | BinOp::Shr => {
                if lt != Ty::Int || rt != Ty::Int {
                    self.err(d(
                        "Y-T0002",
                        Severity::Error,
                        format!("位运算仅限 int，得到 `{lt}` / `{rt}`"),
                        span,
                    ));
                }
                Ty::Int
            }
            BinOp::Range => {
                if lt != Ty::Int || rt != Ty::Int {
                    self.err(d(
                        "Y-T0002",
                        Severity::Error,
                        format!("`..` 区间仅限 int（决策 42），得到 `{lt}` / `{rt}`"),
                        span,
                    ));
                }
                Ty::Range
            }
            BinOp::Pipe => unreachable!("pipe handled above"),
        }
    }

    fn check_arith(&mut self, op: BinOp, lt: &Ty, rt: &Ty, span: Span) -> Ty {
        match (lt, rt) {
            (Ty::Int, Ty::Int) => Ty::Int,
            (Ty::Float, Ty::Float) => Ty::Float,
            (Ty::Int, Ty::Float) | (Ty::Float, Ty::Int) => {
                let other = if matches!(lt, Ty::Float) {
                    lt.clone()
                } else {
                    rt.clone()
                };
                self.err(
                    d(
                        "Y-T0003",
                        Severity::Error,
                        format!("int 与 float 不可混算（决策 26）：`{lt}` 与 `{rt}`"),
                        span,
                    )
                    .with_suggestion(format!("显式转换：`x as {other}`")),
                );
                other
            }
            (Ty::Named { name, .. }, _) if lt.same(rt) => {
                // D-03 fail-fast（30 号定夺）：协议算术的执行管线未接入
                // （lower 发裸 Add、VM Y-R9007 崩）——**已 impl 也拒绝**：
                // spec 承诺兑现前放行 = 虚假承诺，违 strict 健全（静态通过
                // 则运行期成立）。随泛型批次接入协议调用后此臂再放行。
                let proto = match op {
                    BinOp::Add => "Add",
                    BinOp::Sub => "Sub",
                    BinOp::Mul => "Mul",
                    _ => "Div",
                };
                let has_impl = self
                    .decls
                    .impls
                    .iter()
                    .any(|i| i.proto == proto && i.for_ == *name);
                let detail = if has_impl {
                    format!(
                        "类型 `{name}` 已 impl `{proto}`，但协议算术的执行管线尚未实现（随泛型批次接入，30 号 D-03 fail-fast）——已 impl 的算术同样编译期拒绝"
                    )
                } else {
                    format!("类型 `{name}` 未 impl `{proto}`，运算符不可用（决策 21）")
                };
                self.err(
                    d("Y-T0022", Severity::Error, detail, span).with_suggestion(format!(
                        "改用自由函数：`fn add(a: {name}, b: {name}) -> {name}`"
                    )),
                );
                lt.clone()
            }
            _ => {
                // 同型不可算术（如 List/List）：报「不支持运算」而非
                // mismatch——「期望 [U] 得到 [U]」自相矛盾诊断是噪音。
                if lt.same(rt) {
                    let opname = match op {
                        BinOp::Add => "+",
                        BinOp::Sub => "-",
                        BinOp::Mul => "*",
                        _ => "/",
                    };
                    self.err(
                        d(
                            "Y-T0022",
                            Severity::Error,
                            format!(
                                "类型 `{lt}` 不支持算术运算 `{opname}`（决策 21：显式 impl 协议）"
                            ),
                            span,
                        )
                        .with_suggestion(
                            "集合构建用字面量与内建（fold/map），或为自定义类型 impl 协议",
                        ),
                    );
                } else {
                    self.err(self.mismatch(lt.clone(), rt.clone(), span));
                }
                lt.clone()
            }
        }
    }

    fn check_eq(&mut self, lt: &Ty, rt: &Ty, span: Span) {
        if matches!(lt, Ty::Nil) || matches!(rt, Ty::Nil) {
            let other = if matches!(lt, Ty::Nil) { rt } else { lt };
            if !matches!(other, Ty::Opt(_)) {
                self.err(d(
                    "Y-T0002",
                    Severity::Error,
                    format!("nil 只能与 `T?` 比较，得到 `{other}`"),
                    span,
                ));
            }
            return;
        }
        if lt.same(rt) {
            return;
        }
        let is_distinct = |t: &Ty| matches!(t, Ty::Named { name, .. } if matches!(self.decls.types.get(name), Some(TypeKind::Distinct { .. })));
        if is_distinct(lt) || is_distinct(rt) {
            self.err(
                d(
                    "Y-T0010",
                    Severity::Error,
                    format!("distinct 类型不可与其它类型互换比较（决策 23）：`{lt}` vs `{rt}`"),
                    span,
                )
                .with_suggestion("用 `as` 显式转换到同一类型再比较"),
            );
        } else {
            self.err(self.mismatch(lt.clone(), rt.clone(), span));
        }
    }

    fn check_field(&mut self, base: Ty, field: &str, span: Span, base_span: Span) -> Ty {
        match &base {
            Ty::Named { name, args } => {
                // 决策 82 第二步：内建 KV entry（不在用户 decls，特判）
                if name == "KV" && args.len() == 2 {
                    return match field {
                        "key" => args[0].clone(),
                        "value" => args[1].clone(),
                        _ => {
                            self.err(d(
                                "Y-T0026",
                                Severity::Error,
                                format!("KV entry 只有 `key`/`value` 字段，得到 `{field}`"),
                                span,
                            ));
                            Ty::Unit
                        }
                    };
                }
                if let Some(TypeKind::Struct { params, fields }) = self.decls.types.get(name) {
                    if let Some((_, fty)) = fields.iter().find(|(f, _)| f == field) {
                        let r = Resolver {
                            decls: &self.decls,
                            env: self.env,
                            subst: params.iter().cloned().zip(args.iter().cloned()).collect(),
                        };
                        return r.resolve(fty).unwrap_or(Ty::Unit);
                    }
                    self.err(d(
                        "Y-T0026",
                        Severity::Error,
                        format!("`{name}` 没有字段 `{field}`"),
                        span,
                    ));
                    return Ty::Unit;
                }
                if let Some(f) = self.host_field(name, field) {
                    return f;
                }
                self.err(d(
                    "Y-T0026",
                    Severity::Error,
                    format!("`{name}` 没有字段 `{field}`"),
                    span,
                ));
                Ty::Unit
            }
            Ty::Opt(_) => {
                self.err(
                    d(
                        "Y-T0004",
                        Severity::Error,
                        "可空值先解包再取字段",
                        base_span,
                    )
                    .with_suggestion("用 `x!` 解包，或在 `if x != nil` 内使用智能收窄（决策 40）"),
                );
                Ty::Unit
            }
            Ty::Str
            | Ty::List(_)
            | Ty::Map(..)
            | Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Rune
            | Ty::Json => {
                self.err(
                    d("Y-T0023", Severity::Error, "内建集合无方法（19.2）", span)
                        .with_suggestion("使用自由函数 + 数据第一参数：`len(xs)`、`xs |> map(f)`"),
                );
                Ty::Unit
            }
            other => {
                self.err(d(
                    "Y-T0002",
                    Severity::Error,
                    format!("类型 `{other}` 无字段 `{field}`"),
                    span,
                ));
                Ty::Unit
            }
        }
    }

    /// 宿主 struct 字段（阶段 4 由注册表导出；此处内建演示面）。
    fn host_field(&self, ty_name: &str, field: &str) -> Option<Ty> {
        match (ty_name, field) {
            ("Ctx", "log") => Some(Ty::AsyncFn(vec![Ty::Str], Box::new(Ty::Unit))),
            ("Item", "size") => Some(Ty::Int),
            ("Item", "title") => Some(Ty::Str),
            _ => None,
        }
    }

    fn host_fields(&self, ty_name: &str) -> Vec<String> {
        match ty_name {
            "Ctx" => vec!["log".into()],
            "Item" => vec!["size".into(), "title".into()],
            _ => vec![],
        }
    }

    fn check_index(&mut self, base: Ty, index: &ast::IndexKind, span: Span) -> Ty {
        match (&base, index) {
            (Ty::List(t), ast::IndexKind::Single(ix)) => {
                let it = self.check_expr(ix, Some(&Ty::Int));
                if it != Ty::Int {
                    let s = span;
                    self.err(self.mismatch(Ty::Int, it, s));
                }
                (**t).clone()
            }
            (Ty::List(t), ast::IndexKind::Range(a, b)) => {
                for x in [a, b] {
                    let t2 = self.check_expr(x, Some(&Ty::Int));
                    if t2 != Ty::Int {
                        let s = span;
                        self.err(self.mismatch(Ty::Int, t2, s));
                    }
                }
                Ty::list((**t).clone()) // 决策 42：半开切片
            }
            (Ty::Map(k, v), ast::IndexKind::Single(ix)) => {
                let it = self.check_expr(ix, Some(k.as_ref()));
                if !it.same(k) {
                    let s = span;
                    self.err(self.mismatch((**k).clone(), it, s));
                }
                Ty::opt((**v).clone()) // 决策 55
            }
            (Ty::Map(..), ast::IndexKind::Range(..)) => {
                self.err(d("Y-T0024", Severity::Error, "Map 不支持切片", span));
                Ty::Unit
            }
            (Ty::Str, _) => {
                self.err(
                    d(
                        "Y-T0025",
                        Severity::Error,
                        "禁 `s[i]`：码点下标是 O(n) 伪装 O(1)（决策 46）",
                        span,
                    )
                    .with_suggestion("用 `string.to_runes(s)[i]` 取码点"),
                );
                Ty::Rune // 降低级联噪声
            }
            (other, _) => {
                self.err(d(
                    "Y-T0002",
                    Severity::Error,
                    format!("类型 `{other}` 不可索引"),
                    span,
                ));
                Ty::Unit
            }
        }
    }

    fn check_cast(&mut self, from: &Ty, to: &Ty, span: Span) {
        let r = self.resolver();
        let base_of = |c: &Checker, t: &Ty| -> Option<Ty> {
            match t {
                Ty::Named { name, .. } => match c.decls.types.get(name) {
                    // distinct ↔ 基类型（双向，决策 23）
                    Some(TypeKind::Distinct { base }) => r.resolve(base).ok(),
                    _ => None,
                },
                _ => None,
            }
        };
        let ok = match (from, to) {
            (Ty::Int, Ty::Float) | (Ty::Float, Ty::Int) => true,
            // rune↔int（决策 81，27 号 A4 裁决方案①）：rune = distinct int
            // 内建等价（18 §46），distinct 往返本在决策 26 as 许可范围；
            // lower CONVERT_RUNE / VM 码点域检查（char::from_u32）早已实现，
            // 此前仅本处漏列内建 Ty::Rune 导致三处矛盾。
            (Ty::Rune, Ty::Int) | (Ty::Int, Ty::Rune) | (Ty::Rune, Ty::Rune) => true,
            _ => {
                if let Some(fb) = base_of(self, to) {
                    from.same(&fb) // int as EntityId
                } else if let Some(fa) = base_of(self, from) {
                    to.same(&fa) // EntityId as int
                } else {
                    from.same(to)
                }
            }
        };
        if !ok {
            self.err(
                d(
                    "Y-T0011",
                    Severity::Error,
                    format!(
                        "非法转换：`{from}` as `{to}`（决策 26：as 仅限数值互转与 distinct 往返）"
                    ),
                    span,
                )
                .with_suggestion("int↔float、distinct↔基类型之外的结构变化请用显式函数"),
            );
        }
    }

    fn check_struct_lit(
        &mut self,
        name: &str,
        type_args: &[ast::Type],
        inits: &[ast::FieldInit],
        spread: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let (params, fields, host) = match self.decls.types.get(name) {
            Some(TypeKind::Struct { params, fields }) => (
                params.clone(),
                fields
                    .iter()
                    .map(|(f, t)| (f.clone(), t.clone()))
                    .collect::<Vec<_>>(),
                false,
            ),
            _ if self.env.host_types.contains_key(name) => (Vec::new(), Vec::new(), true),
            _ => {
                self.err(d(
                    "Y-T0007",
                    Severity::Error,
                    format!("`{name}` 不是 struct（tag 变体用变体名构造，决策 53）"),
                    span,
                ));
                return Ty::Unit;
            }
        };
        let rargs: Vec<Ty> = type_args
            .iter()
            .map(|t| self.resolver().resolve(t).unwrap_or(Ty::Unit))
            .collect();
        if !host && params.len() != rargs.len() {
            self.err(d(
                "Y-T0007",
                Severity::Error,
                format!(
                    "`{name}` 需要 {} 个类型实参，得到 {}",
                    params.len(),
                    rargs.len()
                ),
                span,
            ));
        }
        let subst: HashMap<String, Ty> =
            params.iter().cloned().zip(rargs.iter().cloned()).collect();
        let field_ty = |c: &Checker, f: &str| -> Option<Ty> {
            if host {
                return c.host_field(name, f);
            }
            fields.iter().find(|(n, _)| n == f).map(|(_, t)| {
                Resolver {
                    decls: &c.decls,
                    env: c.env,
                    subst: subst.clone(),
                }
                .resolve(t)
                .unwrap_or(Ty::Unit)
            })
        };
        if let Some(sp) = spread {
            let st = self.check_expr(sp, None);
            let ok = matches!(&st, Ty::Named { name: n, args: a }
                if n == name
                    && a.len() == rargs.len()
                    && a.iter().zip(&rargs).all(|(x, y)| x.same(y)));
            if !ok {
                self.err(d(
                    "Y-T0002",
                    Severity::Error,
                    format!("`..` 展开基必须是 `{name}`，得到 `{st}`（决策 41）"),
                    sp.span,
                ));
            }
        }
        let mut seen: Vec<String> = Vec::new();
        for init in inits {
            if seen.contains(&init.name) {
                self.err(d(
                    "Y-T0027",
                    Severity::Error,
                    format!("字段 `{}` 重复初始化", init.name),
                    init.span,
                ));
                continue;
            }
            seen.push(init.name.clone());
            match field_ty(self, &init.name) {
                Some(ft) => {
                    let vt = self.check_expr(&init.value, Some(&ft));
                    if !vt.same(&ft) && !self.assignable(&vt, &ft) {
                        let s = init.span;
                        self.err(self.mismatch(ft, vt, s));
                    }
                }
                None => {
                    self.err(d(
                        "Y-T0026",
                        Severity::Error,
                        format!("`{name}` 没有字段 `{}`", init.name),
                        init.span,
                    ));
                    self.check_expr(&init.value, None);
                }
            }
        }
        let all: Vec<String> = if host {
            self.host_fields(name)
        } else {
            fields.iter().map(|(n, _)| n.clone()).collect()
        };
        if spread.is_none() {
            let missing: Vec<String> = all.iter().filter(|f| !seen.contains(f)).cloned().collect();
            if !missing.is_empty() {
                self.err(
                    d(
                        "Y-T0028",
                        Severity::Error,
                        format!("`{name}` 缺少字段：{}", missing.join(", ")),
                        span,
                    )
                    .with_suggestion(
                        "补齐字段，或用 `Type { ..base, field: v }` 基于已有值更新（决策 41）",
                    ),
                );
            }
        }
        Ty::named(name, rargs)
    }

    fn check_assign(&mut self, target: &Expr, op: ast::AssignOp, value: &Expr, span: Span) -> Ty {
        fn root_ident(e: &Expr) -> Option<String> {
            match &e.kind {
                ExprKind::Ident(n) => Some(n.clone()),
                ExprKind::Field { base, .. } | ExprKind::Index { base, .. } => root_ident(base),
                _ => None,
            }
        }
        let tname = root_ident(target);
        let mutable = tname
            .as_ref()
            .and_then(|n| {
                self.scopes
                    .iter()
                    .rev()
                    .find_map(|s| s.vars.get(n).map(|b| b.mutable))
            })
            .unwrap_or(false);
        if !mutable {
            self.err(
                d(
                    "Y-T0033",
                    Severity::Error,
                    "赋值目标必须声明为 `let mut`（可变点显式，19.2）",
                    span,
                )
                .with_suggestion(format!(
                    "改为 `let mut {} = ...`",
                    tname.clone().unwrap_or_default()
                )),
            );
        }
        let tt = self.check_expr(target, None);
        let vt = self.check_expr(value, Some(&tt));
        match op {
            ast::AssignOp::Assign => {
                if !self.assignable(&vt, &tt) {
                    let s = span;
                    self.err(self.mismatch(tt, vt, s));
                }
            }
            ast::AssignOp::AddAssign | ast::AssignOp::SubAssign => {
                let base = match &tt {
                    Ty::Opt(t) => (**t).clone(),
                    other => other.clone(),
                };
                let op2 = match op {
                    ast::AssignOp::AddAssign => BinOp::Add,
                    _ => BinOp::Sub,
                };
                self.check_arith(op2, &base, &vt, span);
            }
        }
        if let Some(n) = tname {
            for s in self.scopes.iter_mut() {
                s.facts.remove(&n);
            }
        }
        Ty::Unit
    }

    // ===== 调用 =====

    fn check_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        expected: Option<&Ty>,
        piped: Option<Ty>,
        span: Span,
    ) -> Ty {
        // Ok / Err 构造器（决策 54 词汇表）
        if let ExprKind::Ident(n) = &callee.kind {
            if n == "Ok" || n == "Err" {
                return self.check_ok_err(n, args, expected, span);
            }
            // 带载荷变体构造
            if let Some((owner, Some(payload))) = self.decls.variants.get(n).cloned() {
                // D-14：payload 可能含 owner 泛型形参（`Boxed(P2[T])`）——
                // 以 Generic 注入解析，payload 表达式检查后 unify 反推实参
                // （`P2<int>` → T=int → Box2<int>），与 apply_sig 同构。
                let owner_params: Vec<String> = match self.decls.types.get(&owner) {
                    Some(TypeKind::Tag { params, .. }) => params.clone(),
                    _ => Vec::new(),
                };
                let r = Resolver {
                    decls: &self.decls,
                    env: self.env,
                    subst: owner_params
                        .iter()
                        .cloned()
                        .map(|t| (t.clone(), Ty::Generic(t)))
                        .collect(),
                };
                let pt = r.resolve(&payload).unwrap_or_else(|e| {
                    self.err(e);
                    Ty::Unit
                });
                if args.len() != 1 {
                    self.err(d(
                        "Y-T0008",
                        Severity::Error,
                        format!("变体 `{n}` 恰好 1 个载荷，得到 {}", args.len()),
                        span,
                    ));
                    return Ty::named(&owner, owner_params.iter().map(|_| Ty::Unit).collect());
                }
                let at = self.check_expr(&args[0], Some(&pt));
                if at.same(&pt) {
                    // payload 精确匹配：owner 实参 = payload 里的具名实参
                    // （`Boxed(P2<int>{..})` → pt=P2[int]），非泛型载荷取 owner 声明序 Unit 占位
                    let vargs = match &pt {
                        Ty::Named { args, .. } if !args.is_empty() => args.clone(),
                        _ => owner_params.iter().map(|_| Ty::Unit).collect(),
                    };
                    return Ty::named(&owner, vargs);
                }
                if contains_generic(&pt) {
                    // 泛型载荷：unify 反推 owner 实参
                    let mut subst: HashMap<String, Ty> = HashMap::new();
                    self.unify(&pt, &at, &mut subst, span);
                    let vargs: Vec<Ty> = owner_params
                        .iter()
                        .map(|p| substitute(&Ty::Generic(p.clone()), &subst))
                        .collect();
                    if vargs.iter().any(contains_generic) {
                        self.err(
                            d(
                                "Y-T0018",
                                Severity::Error,
                                format!(
                                    "变体 `{n}` 的类型实参无法从载荷推断（得到 `{at}`，期望 `{pt}`）"
                                ),
                                span,
                            )
                            .with_suggestion("为变体绑定加类型标注，或让载荷类型可确定泛型"),
                        );
                    }
                    return Ty::named(&owner, vargs);
                }
                let s = span;
                self.err(self.mismatch(pt, at, s));
                return Ty::named(&owner, owner_params.iter().map(|_| Ty::Unit).collect());
            }
        }
        let sig: Option<Ty> = match &callee.kind {
            ExprKind::Ident(name) => {
                // 内建注册表（决策 77）：按实参数选层；内建名不可被用户遮蔽
                // （单一命名空间，19.2），故先于 lookup。
                let argc = args.len() + piped.is_some() as usize;
                if yoru_stdlib::is_builtin(name) {
                    if let Some(b) = yoru_stdlib::select_free(name, argc) {
                        return self.apply_builtin(b, args, expected, piped, span);
                    }
                    self.err(builtin_arity_err(name, argc, span));
                    for a in args {
                        self.check_expr(a, None);
                    }
                    return Ty::Unit;
                }
                match self.lookup(name, callee.span) {
                    Resolved::Ty(t) => Some(t),
                    Resolved::Module { module, .. } => {
                        self.err(d(
                            "Y-T0002",
                            Severity::Error,
                            format!("模块 `{module}` 不能直接调用"),
                            span,
                        ));
                        None
                    }
                }
            }
            ExprKind::Field { base, field } => {
                // 模块函数：math.sqrt 形式（决策 77：注册表属主）
                if let ExprKind::Ident(n) = &base.kind {
                    if let Resolved::Module { module, .. } = self.lookup(n, base.span) {
                        return match yoru_stdlib::module_fn(&module, field) {
                            Some(b) => self.apply_builtin(b, args, expected, piped, span),
                            None => {
                                self.err(d(
                                    "Y-T0007",
                                    Severity::Error,
                                    format!("模块 `{module}` 没有函数 `{field}`"),
                                    callee.span,
                                ));
                                for a in args {
                                    self.check_expr(a, None);
                                }
                                Ty::Unit
                            }
                        };
                    }
                }
                let bt = self.check_expr(base, None);
                let ft = self.check_field(bt, field, callee.span, base.span);
                Some(ft)
            }
            _ => Some(self.check_expr(callee, None)),
        };
        match sig {
            Some(s @ Ty::Fn(..)) | Some(s @ Ty::AsyncFn(..)) => {
                self.apply_sig(&s, args, expected, piped, span)
            }
            Some(other) => {
                self.err(d(
                    "Y-T0002",
                    Severity::Error,
                    format!("类型 `{other}` 不可调用"),
                    callee.span,
                ));
                for a in args {
                    self.check_expr(a, None);
                }
                Ty::Unit
            }
            None => {
                for a in args {
                    self.check_expr(a, None);
                }
                Ty::Unit
            }
        }
    }

    fn check_ok_err(
        &mut self,
        which: &str,
        args: &[Expr],
        expected: Option<&Ty>,
        span: Span,
    ) -> Ty {
        let (ok_ty, err_ty) = match expected {
            Some(Ty::Res(o, e)) => ((**o).clone(), (**e).clone()),
            _ => {
                self.err(
                    d(
                        "Y-T0018",
                        Severity::Error,
                        format!("`{which}` 的 Result 类型无法从上下文推断"),
                        span,
                    )
                    .with_suggestion("在 return 位置使用（函数返回类型提供上下文），或加类型标注"),
                );
                for a in args {
                    self.check_expr(a, None);
                }
                return Ty::Unit;
            }
        };
        let want = if which == "Ok" { ok_ty } else { err_ty };
        if args.len() != 1 {
            self.err(d(
                "Y-T0008",
                Severity::Error,
                format!("`{which}` 恰好 1 个参数，得到 {}", args.len()),
                span,
            ));
            return expected.cloned().unwrap_or(Ty::Unit);
        }
        let at = self.check_expr(&args[0], Some(&want));
        if !at.same(&want) && !self.assignable(&at, &want) {
            let s = span;
            self.err(self.mismatch(want, at, s));
        }
        expected.cloned().unwrap_or(Ty::Unit)
    }

    /// 签名应用：泛型 unify + 逐参检查 + 返回类型代换。
    /// 异步宿主函数（AsyncFn）调用产出 `Async(ret)`，仅可被 await 消费（14.5）。
    fn apply_sig(
        &mut self,
        sig: &Ty,
        args: &[Expr],
        expected: Option<&Ty>,
        piped: Option<Ty>,
        span: Span,
    ) -> Ty {
        let (is_async, formals, ret) = match sig {
            Ty::Fn(ps, r) => (false, ps.clone(), (**r).clone()),
            Ty::AsyncFn(ps, r) => (true, ps.clone(), (**r).clone()),
            _ => unreachable!(),
        };
        let mut subst: HashMap<String, Ty> = HashMap::new();
        let offset = if piped.is_some() { 1 } else { 0 };
        if let Some(p) = &piped {
            if formals.is_empty() {
                self.err(d(
                    "Y-T0008",
                    Severity::Error,
                    "管道送入的函数不接受参数",
                    span,
                ));
                return Ty::Unit;
            }
            self.unify(&formals[0], p, &mut subst, span);
        }
        let expect_argc = formals.len() - offset;
        if args.len() != expect_argc {
            self.err(
                d(
                    "Y-T0008",
                    Severity::Error,
                    format!(
                        "函数需要 {} 个参数，得到 {}（决策 48：无默认/命名参数）",
                        expect_argc,
                        args.len()
                    ),
                    span,
                )
                .with_suggestion("可选配置走 struct 参数"),
            );
            for a in args {
                self.check_expr(a, None);
            }
            return substitute(&ret, &subst);
        }
        for (i, arg) in args.iter().enumerate() {
            let formal_raw = formals[i + offset].clone();
            self.bind_arg(&formal_raw, arg, &mut subst);
        }
        let ret = substitute(&ret, &subst);
        // Y-T0018 兜底仅在泛型域外触发（D-13）：泛型 fn body 内调用函数值
        // （`f(x): U`，U 为形参类型变量）是泛型抽象的合法形态——自由类型
        // 变量上推到非泛型调用位再收敛；域内强报会阻断合法泛型代码。
        if self.fn_typarams.is_empty() && contains_generic(&ret) {
            self.err(
                d(
                    "Y-T0018",
                    Severity::Error,
                    format!("返回类型 `{ret}` 含未绑定泛型，无法推断"),
                    span,
                )
                .with_suggestion("让实参类型可确定泛型（如提供带类型标注的 lambda），或加标注"),
            );
        }
        let _ = expected;
        if is_async {
            Ty::Async(Box::new(ret))
        } else {
            ret
        }
    }

    /// 内建调用（决策 77）：注册表面 + 变参（print）/ 可选尾参（assert）+
    /// 软类型参数位（Num/Coll/Any，`same` 成员集判定）+ same_numeric（min/max
    /// 两参同具体类型）+ 返回位 Num 落地（abs/min/max 返回实参类型）。
    fn apply_builtin(
        &mut self,
        b: &yoru_stdlib::Builtin,
        args: &[Expr],
        expected: Option<&Ty>,
        piped: Option<Ty>,
        span: Span,
    ) -> Ty {
        let mut subst: HashMap<String, Ty> = HashMap::new();
        let offset = piped.is_some() as usize;
        let argc = args.len() + offset;
        let ret_ty = || crate::env::param_ty_pub(&b.ret);
        if !b.accepts_argc(argc) {
            self.err(builtin_arity_err(b.name, argc, span));
            for a in args {
                self.check_expr(a, None);
            }
            return substitute(&ret_ty(), &subst);
        }
        // 管道送入：变参内建按 Any 收（print）；否则绑定首参
        if let Some(p) = &piped {
            if !b.variadic {
                if let Some(first) = b.params.first() {
                    let formal = crate::env::param_ty_pub(first);
                    self.unify(&formal, p, &mut subst, span);
                }
            }
        }
        // Number 位实参的具体类型：abs 返回位落地 + min/max 同型校验
        let mut num_kinds: Vec<Ty> = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            let fi = i + offset;
            match b.params.get(fi) {
                // 变参附加位：Any，不检查
                None => {
                    self.check_expr(arg, None);
                }
                Some(yoru_stdlib::Param::Opt(inner)) => {
                    let formal = crate::env::param_ty_pub(&inner[0]);
                    let at = self.check_expr(arg, Some(&formal));
                    if !at.same(&formal) && !self.assignable(&at, &formal) {
                        self.err(self.mismatch(formal, at, arg.span));
                    }
                }
                Some(p) => {
                    // 先按已绑定泛型代换（管道 lhs 可能已绑定 T），再检查
                    let formal_raw = crate::env::param_ty_pub(p);
                    let at = self.bind_arg(&formal_raw, arg, &mut subst);
                    if matches!(formal_raw, Ty::Num) && matches!(at, Ty::Int | Ty::Float) {
                        num_kinds.push(at);
                    }
                }
            }
        }
        if b.same_numeric && num_kinds.len() == 2 && num_kinds[0] != num_kinds[1] {
            self.err(d(
                "Y-T0002",
                Severity::Error,
                format!(
                    "内建 `{}` 的两个参数须同为 int 或同为 float（决策 26：无隐式转换）",
                    b.name
                ),
                span,
            ));
        }
        let mut ret = substitute(&ret_ty(), &subst);
        // 同 apply_sig：泛型域内放行自由类型变量（D-13）。
        if self.fn_typarams.is_empty() && contains_generic(&ret) {
            self.err(
                d(
                    "Y-T0018",
                    Severity::Error,
                    format!("返回类型 `{ret}` 含未绑定泛型，无法推断"),
                    span,
                )
                .with_suggestion("让实参类型可确定泛型（如提供带类型标注的 lambda），或加标注"),
            );
        }
        if let Some(kind) = num_kinds.first() {
            ret = ret.resolve_num(kind);
        }
        let _ = expected;
        ret
    }

    /// 单参绑定：泛型 formal 与实参 unify；非泛型不匹配报错（apply_sig 与
    /// apply_builtin 共用，消除两处逐参逻辑重复）。
    /// 单参绑定：泛型 formal 与实参 unify；非泛型不匹配报错（apply_sig 与
    /// apply_builtin 共用，消除两处逐参逻辑重复）。返回实参类型。
    fn bind_arg(&mut self, formal_raw: &Ty, arg: &Expr, subst: &mut HashMap<String, Ty>) -> Ty {
        let formal = substitute(formal_raw, subst);
        let at = self.check_expr(arg, Some(&formal));
        if !at.same(&formal) && !self.assignable(&at, &formal) {
            if contains_generic(formal_raw) {
                self.unify(formal_raw, &at, subst, arg.span);
            } else {
                self.err(self.mismatch(formal.clone(), at.clone(), arg.span));
            }
        }
        at
    }

    /// 结构 unify：泛型绑定；命名类型按名义相等。
    fn unify(&mut self, pat: &Ty, act: &Ty, subst: &mut HashMap<String, Ty>, span: Span) {
        match (pat, act) {
            (Ty::Generic(n), a) => match subst.get(n) {
                Some(bound) => {
                    if !bound.same(a) {
                        self.err(d(
                            "Y-T0002",
                            Severity::Error,
                            format!("泛型 `{n}` 已绑定为 `{bound}`，与 `{a}` 冲突"),
                            span,
                        ));
                    }
                }
                None => {
                    subst.insert(n.clone(), a.clone());
                }
            },
            (Ty::Opt(p), Ty::Opt(a)) => self.unify(p, a, subst, span),
            (Ty::List(p), Ty::List(a)) => self.unify(p, a, subst, span),
            (Ty::Res(po, pe), Ty::Res(ao, ae)) => {
                self.unify(po, ao, subst, span);
                self.unify(pe, ae, subst, span);
            }
            (Ty::Map(pk, pv), Ty::Map(ak, av)) => {
                self.unify(pk, ak, subst, span);
                self.unify(pv, av, subst, span);
            }
            (Ty::Fn(ps, pr), Ty::Fn(as_, ar)) if ps.len() == as_.len() => {
                for (p, a) in ps.iter().zip(as_.iter()) {
                    self.unify(p, a, subst, span);
                }
                self.unify(pr, ar, subst, span);
            }
            (Ty::Named { name: pn, args: pa }, Ty::Named { name: an, args: aa })
                if pn == an && pa.len() == aa.len() =>
            {
                for (p, a) in pa.iter().zip(aa.iter()) {
                    self.unify(p, a, subst, span);
                }
            }
            _ => {}
        }
    }
}

fn body_expr_span(body: &ast::FnBody) -> Span {
    match body {
        ast::FnBody::Expr(e) => e.span,
        ast::FnBody::Block(b) => b.span,
    }
}

fn substitute(ty: &Ty, subst: &HashMap<String, Ty>) -> Ty {
    match ty {
        Ty::Generic(n) => subst.get(n).cloned().unwrap_or_else(|| ty.clone()),
        Ty::Named { name, args } => Ty::Named {
            name: name.clone(),
            args: args.iter().map(|a| substitute(a, subst)).collect(),
        },
        Ty::Opt(t) => Ty::opt(substitute(t, subst)),
        Ty::Res(o, e) => Ty::res(substitute(o, subst), substitute(e, subst)),
        Ty::List(t) => Ty::list(substitute(t, subst)),
        Ty::Map(k, v) => Ty::Map(
            Box::new(substitute(k, subst)),
            Box::new(substitute(v, subst)),
        ),
        Ty::Fn(ps, r) => Ty::Fn(
            ps.iter().map(|p| substitute(p, subst)).collect(),
            Box::new(substitute(r, subst)),
        ),
        Ty::AsyncFn(ps, r) => Ty::AsyncFn(
            ps.iter().map(|p| substitute(p, subst)).collect(),
            Box::new(substitute(r, subst)),
        ),
        Ty::Async(t) => Ty::Async(Box::new(substitute(t, subst))),
        other => other.clone(),
    }
}

fn contains_generic(ty: &Ty) -> bool {
    match ty {
        Ty::Generic(_) => true,
        Ty::Named { args, .. } => args.iter().any(contains_generic),
        Ty::Opt(t) | Ty::List(t) | Ty::Async(t) => contains_generic(t),
        Ty::Res(o, e) => contains_generic(o) || contains_generic(e),
        Ty::Map(k, v) => contains_generic(k) || contains_generic(v),
        Ty::Fn(ps, r) | Ty::AsyncFn(ps, r) => {
            ps.iter().any(contains_generic) || contains_generic(r)
        }
        _ => false,
    }
}

/// 内建元数错误（决策 77）：列全部重载层的签名面（range 两层可见）。
fn builtin_arity_err(name: &str, argc: usize, span: Span) -> Diagnostic {
    let faces: Vec<String> = yoru_stdlib::builtins_named(name)
        .iter()
        .map(|b| format!("{}", crate::env::builtin_face(b)))
        .collect();
    d(
        "Y-T0008",
        Severity::Error,
        format!("内建 `{name}` 不接受 {argc} 个参数（决策 48：无默认/命名参数）"),
        span,
    )
    .with_suggestion(format!("签名：{}", faces.join(" 或 ")))
}
