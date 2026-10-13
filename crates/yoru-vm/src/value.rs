//! Yoru 运行时值模型（阶段 3，22 号决策 69：堆归 `Vm` 所有，实例严格隔离）。
//!
//! 语义锚点：
//! - 值语义 + RC 堆（决策 6/13）：容器为 `Rc<RefCell<_>>`；**refcount==1 时原地变更，
//!   否则先拷贝再改（COW）**——无观测别名是可证明的运行期不变量；
//! - 闭包按值快照（决策 31）：upvalue 捕获在闭包创建时拍快照，无共享可变单元；
//! - Map 插入序（决策 36）：线性 Vec 承载，T1 以正确性优先；
//! - 深相等（决策 16）：容器逐元素递归比较；
//! - 宿主不透明值（Json 等，决策 28）：`Opaque` 变体，VM 不解构。

use std::cell::RefCell;
use std::rc::Rc;

use crate::Vm;
use crate::error::VmError;

/// 宿主函数（内建/宿主注册统一形态）。
pub type HostFn = Rc<dyn Fn(&mut Vm, &[Value]) -> Result<Value, VmError>>;

/// struct 运行时对象：`struct_id` 指向模块 struct 表；字段按表序对齐全局字段名序号。
#[derive(Debug, Clone)]
pub struct StructObj {
    pub struct_id: u32,
    /// (全局字段名序号, 值)——NEWSTRUCT 时由模块声明表对齐
    pub fields: Vec<(u32, Value)>,
}

/// 闭包：原型 + 按值快照的 upvalue（决策 31）。
#[derive(Debug, Clone)]
pub struct ClosureObj {
    pub proto: u32,
    pub upvals: Vec<Value>,
}

/// 变体值：tag/variant 序号 + 可选载荷。
#[derive(Debug, Clone)]
pub struct VariantVal {
    pub tag: u32,
    pub variant: u32,
    pub payload: Option<Rc<Value>>,
}

/// for-in 游标（内部值，脚本不可绑定；ForPrep 建立，ForNext 步进）。
#[derive(Debug, Clone)]
pub enum Cursor {
    /// 列表按值快照迭代（决策 31：迭代中改原列表不影响游标）
    List { items: Vec<Value>, next: usize },
    /// 半开区间 `next..end`（决策 42）
    Range { next: i64, end: i64 },
    /// 字符串按码点迭代（决策 46：禁 s[i]，经迭代/切片获取）
    Str { chars: Vec<char>, next: usize },
    /// Map 按键值对快照迭代（决策 82 第二步：产出 KV entry struct）
    Map {
        entries: Vec<(Value, Value)>,
        next: usize,
    },
}

/// Map 堆形态：保序 KV（COW 经 Rc 引用计数判定）。
pub type MapBox = Rc<RefCell<Vec<(Value, Value)>>>;

/// Yoru 运行时值。
#[derive(Clone)]
pub enum Value {
    Nil,
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(Rc<str>),
    List(Rc<RefCell<Vec<Value>>>),
    /// 插入序键值对（决策 36）；同时充当宿主模块对象（名字→值）
    Map(MapBox),
    Struct(Rc<RefCell<StructObj>>),
    /// 半开区间 [start, end)（决策 42）
    Range(i64, i64),
    Variant(Rc<VariantVal>),
    Closure(Rc<ClosureObj>),
    HostFn(HostFn),
    /// for-in 游标（内部）
    Cursor(Rc<RefCell<Cursor>>),
    /// 宿主不透明值（Json 等；VM 不解构，仅透传与显示）
    Opaque(Rc<dyn std::any::Any>),
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.deep_eq(other)
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Nil => write!(f, "nil"),
            Value::Int(v) => write!(f, "int({v})"),
            Value::Float(v) => write!(f, "float({v})"),
            Value::Bool(v) => write!(f, "bool({v})"),
            Value::Str(s) => write!(f, "str({s:?})"),
            Value::List(_) => write!(f, "list"),
            Value::Map(_) => write!(f, "map"),
            Value::Struct(o) => write!(f, "struct#{}", o.borrow().struct_id),
            Value::Range(a, b) => write!(f, "range({a}..{b})"),
            Value::Variant(v) => write!(f, "variant({}, {})", v.tag, v.variant),
            Value::Closure(c) => write!(f, "closure#{}", c.proto),
            Value::HostFn(_) => write!(f, "host_fn"),
            Value::Cursor(_) => write!(f, "cursor"),
            Value::Opaque(_) => write!(f, "opaque"),
        }
    }
}

impl Value {
    /// 循环引用防爆栈上限：超深截断（列表可自含，T2 硬化）。
    const MAX_RECURSE_DEPTH: u32 = 100;

    /// 深相等（决策 16）。闭包/宿主函数/不透明值按同一性（Rc 指针）比较。
    /// 循环结构：指针短路判等；两条不同循环链比较超深后保守返回 false。
    pub fn deep_eq(&self, other: &Value) -> bool {
        self.deep_eq_d(other, 0)
    }

    fn deep_eq_d(&self, other: &Value, depth: u32) -> bool {
        if depth > Self::MAX_RECURSE_DEPTH {
            return false;
        }
        match (self, other) {
            (Value::Nil, Value::Nil) => true,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Range(a, b), Value::Range(c, d)) => a == c && b == d,
            (Value::List(a), Value::List(b)) => {
                if Rc::ptr_eq(a, b) {
                    return true;
                }
                let (a, b) = (a.borrow(), b.borrow());
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|(x, y)| x.deep_eq_d(y, depth + 1))
            }
            (Value::Map(a), Value::Map(b)) => {
                if Rc::ptr_eq(a, b) {
                    return true;
                }
                let (a, b) = (a.borrow(), b.borrow());
                // 序不敏感：同数量，且 a 的每对键值在 b 中存在
                a.len() == b.len()
                    && a.iter().all(|(k, v)| {
                        b.iter().any(|(k2, v2)| {
                            k.deep_eq_d(k2, depth + 1) && v.deep_eq_d(v2, depth + 1)
                        })
                    })
            }
            (Value::Struct(a), Value::Struct(b)) => {
                if Rc::ptr_eq(a, b) {
                    return true;
                }
                let (a, b) = (a.borrow(), b.borrow());
                a.struct_id == b.struct_id
                    && a.fields.len() == b.fields.len()
                    && a.fields
                        .iter()
                        .zip(b.fields.iter())
                        .all(|((f1, v1), (f2, v2))| f1 == f2 && v1.deep_eq_d(v2, depth + 1))
            }
            (Value::Variant(a), Value::Variant(b)) => {
                a.tag == b.tag
                    && a.variant == b.variant
                    && match (&a.payload, &b.payload) {
                        (None, None) => true,
                        (Some(x), Some(y)) => x.deep_eq_d(y, depth + 1),
                        _ => false,
                    }
            }
            (Value::Closure(a), Value::Closure(b)) => Rc::ptr_eq(a, b),
            (Value::HostFn(a), Value::HostFn(b)) => Rc::ptr_eq(a, b),
            (Value::Opaque(a), Value::Opaque(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// 插值/打印显示形式（19.1：插值 = 拼串唯一形式的值侧）。
    /// 循环结构超深后以 `...` 截断（T2 硬化：防自含列表爆栈）。
    pub fn display(&self) -> String {
        self.display_d(0)
    }

    /// 带模块上下文的观测格式化（27 号 A3）：Variant 顶层反查 tag 名，
    /// 载荷递归。resolver(tag_id, variant_id) → Some(格式化名) 或 None
    /// （fallback 到 `<variant N>` 原样）。仅用于 print/错误消息/CLI 等
    /// 观测面——值布局与 display() 本体不变；嵌套容器内层走 display_d
    /// （错误身份主要在顶层值，容器内嵌 Variant 场景属诊断增强，暂不递归）。
    pub fn display_with<F>(&self, resolve: &mut F) -> String
    where
        F: FnMut(u32, u32) -> Option<String>,
    {
        match self {
            Value::Variant(v) => {
                let name =
                    resolve(v.tag, v.variant).unwrap_or_else(|| format!("<variant {}>", v.variant));
                match &v.payload {
                    Some(p) => format!("{name}({})", p.display()),
                    None => name,
                }
            }
            other => other.display(),
        }
    }

    fn display_d(&self, depth: u32) -> String {
        if depth > Self::MAX_RECURSE_DEPTH {
            return "...".into();
        }
        match self {
            Value::Nil => "nil".into(),
            Value::Int(v) => v.to_string(),
            Value::Float(v) => {
                if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
                    format!("{v:.1}") // 保留 float 观感：1 → "1.0"
                } else {
                    format!("{v}")
                }
            }
            Value::Bool(v) => v.to_string(),
            Value::Str(s) => s.to_string(),
            Value::Range(a, b) => format!("{a}..{b}"),
            Value::List(l) => {
                let l = l.borrow();
                let items: Vec<String> = l.iter().map(|v| v.display_d(depth + 1)).collect();
                format!("[{}]", items.join(", "))
            }
            Value::Map(m) => {
                let m = m.borrow();
                let items: Vec<String> = m
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k.display_d(depth + 1), v.display_d(depth + 1)))
                    .collect();
                format!("{{{}}}", items.join(", "))
            }
            Value::Struct(s) => {
                let s = s.borrow();
                let items: Vec<String> = s
                    .fields
                    .iter()
                    .map(|(_, v)| v.display_d(depth + 1))
                    .collect();
                format!("{{{}}}", items.join(", "))
            }
            Value::Variant(v) => match &v.payload {
                Some(p) => format!("<variant {}>{}", v.variant, p.display()),
                None => format!("<variant {}>", v.variant),
            },
            Value::Closure(_) => "<fn>".into(),
            Value::HostFn(_) => "<host_fn>".into(),
            Value::Cursor(_) => "<cursor>".into(),
            Value::Opaque(_) => "<host>".into(),
        }
    }
}
