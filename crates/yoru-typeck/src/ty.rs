//! 类型表示：Yoru typeck 的语义类型（阶段 1，20 号路线图）。
//!
//! 设计要点：
//! - 命名类型（struct/tag/alias/distinct）按**名义相等**（`Named` 的 name+args）；
//!   distinct 与其基类型名字不同 → 结构上不可互换（决策 23）。
//! - 递归类型（`Tree[T]` 含自身）不在 `Ty` 里展开——字段/变体查询经 decl 表按需
//!   解析（`env.rs`），`Ty` 只携带名字与实参，天然支持递归。
//! - `Generic(s)` 仅存在于泛型签名与调用期 unify 的模式位，不出现在检查结果中。

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Int,
    Float,
    Bool,
    Str,
    Rune,
    Unit,
    Json,
    /// nil 字面量（可赋给任意 `T?`，决策 27 的反向入口）
    Nil,
    /// 用户命名类型：struct / tag / alias / distinct（kind 查 decl 表）
    Named { name: String, args: Vec<Ty> },
    Opt(Box<Ty>),
    Res(Box<Ty>, Box<Ty>),
    List(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    Fn(Vec<Ty>, Box<Ty>),
    /// 宿主异步函数：调用产出 `Async(ret)`，`await` 解包（14.5）
    AsyncFn(Vec<Ty>, Box<Ty>),
    /// 宿主异步调用的结果类型，仅可被 `await` 消费
    Async(Box<Ty>),
    /// 泛型参数（模式位）
    Generic(String),
    /// `a..b` 区间（仅合法出现在切片下标与 for）
    Range,
    /// 内建面软类型：int | float（abs/min/max；决策 77 注册表生成，用户不可书写）
    Num,
    /// 内建面软类型：str | list | map（len；决策 77）
    Coll,
    /// 内建面软类型：任意值（print 变参；决策 77）
    Any,
}

impl Ty {
    pub fn opt(inner: Ty) -> Ty {
        Ty::Opt(Box::new(inner))
    }
    pub fn res(ok: Ty, err: Ty) -> Ty {
        Ty::Res(Box::new(ok), Box::new(err))
    }
    pub fn list(elem: Ty) -> Ty {
        Ty::List(Box::new(elem))
    }
    pub fn named(name: &str, args: Vec<Ty>) -> Ty {
        Ty::Named { name: name.into(), args }
    }

    /// 结构相等（泛型参数位不参与——unify 前置）。
    /// 内建面软类型（Num/Coll/Any）按成员集判定——只出现在注册表生成的
    /// 内建签名里，不会进入用户可书写类型（决策 77）。
    pub fn same(&self, other: &Ty) -> bool {
        match (self, other) {
            (Ty::Num, Ty::Int) | (Ty::Num, Ty::Float) | (Ty::Num, Ty::Num) => true,
            (Ty::Int, Ty::Num) | (Ty::Float, Ty::Num) => true,
            (Ty::Coll, Ty::Str) | (Ty::Coll, Ty::List(_)) | (Ty::Coll, Ty::Map(..)) => true,
            (Ty::Str, Ty::Coll) | (Ty::List(_), Ty::Coll) | (Ty::Map(..), Ty::Coll) => true,
            (Ty::Any, _) | (_, Ty::Any) => true,
            (Ty::Named { name: a, args: xa }, Ty::Named { name: b, args: xb }) => {
                a == b && xa.len() == xb.len() && xa.iter().zip(xb).all(|(p, q)| p.same(q))
            }
            (Ty::Opt(a), Ty::Opt(b)) => a.same(b),
            (Ty::Res(ao, ae), Ty::Res(bo, be)) => ao.same(bo) && ae.same(be),
            (Ty::List(a), Ty::List(b)) => a.same(b),
            (Ty::Map(ak, av), Ty::Map(bk, bv)) => ak.same(bk) && av.same(bv),
            (Ty::Fn(ap, ar), Ty::Fn(bp, br)) => {
                ap.len() == bp.len()
                    && ar.same(br)
                    && ap.iter().zip(bp).all(|(p, q)| p.same(q))
            }
            _ => self == other,
        }
    }

    /// 内建面软类型 `Num` 落地为具体类型（abs/min/max 的返回位：实参是 int
    /// 就返回 int，是 float 就返回 float——决策 77 注册表调用路径专用）。
    pub fn resolve_num(&self, kind: &Ty) -> Ty {
        match self {
            Ty::Num => kind.clone(),
            Ty::Opt(t) => Ty::opt(t.resolve_num(kind)),
            Ty::List(t) => Ty::list(t.resolve_num(kind)),
            Ty::Fn(ps, r) => Ty::Fn(
                ps.iter().map(|p| p.resolve_num(kind)).collect(),
                Box::new(r.resolve_num(kind)),
            ),
            other => other.clone(),
        }
    }
}

// 手写 Display（错误消息 expected/actual 用）
impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Int => write!(f, "int"),
            Ty::Float => write!(f, "float"),
            Ty::Bool => write!(f, "bool"),
            Ty::Str => write!(f, "string"),
            Ty::Rune => write!(f, "rune"),
            Ty::Unit => write!(f, "unit"),
            Ty::Json => write!(f, "Json"),
            Ty::Nil => write!(f, "nil"),
            Ty::Range => write!(f, "range"),
            Ty::Named { name, args } => {
                write!(f, "{name}")?;
                if !args.is_empty() {
                    let parts: Vec<String> = args.iter().map(|a| a.to_string()).collect();
                    write!(f, "[{}]", parts.join(", "))?;
                }
                Ok(())
            }
            Ty::Opt(t) => write!(f, "{t}?"),
            Ty::Num => write!(f, "number"),
            Ty::Coll => write!(f, "str | list | map"),
            Ty::Any => write!(f, "any"),
            Ty::Res(o, e) => write!(f, "Result[{o}, {e}]"),
            Ty::List(t) => write!(f, "[{t}]"),
            Ty::Map(k, v) => write!(f, "[{k}: {v}]"),
            Ty::Fn(ps, r) => {
                let parts: Vec<String> = ps.iter().map(|a| a.to_string()).collect();
                write!(f, "fn({}) -> {}", parts.join(", "), r)
            }
            Ty::AsyncFn(ps, r) => {
                let parts: Vec<String> = ps.iter().map(|a| a.to_string()).collect();
                write!(f, "async fn({}) -> {}", parts.join(", "), r)
            }
            Ty::Async(t) => write!(f, "Async<{t}>"),
            Ty::Generic(s) => write!(f, "{s}"),
        }
    }
}
