//! 内建注册表（阶段 3 最小切片）：以 `Const(Str(名))` 为 callee 的调用约定分派。
//!
//! 覆盖 yoru-stdlib 注册表全名单（决策 77）：len/map/filter/fold/sort/sort_floats/keys/
//! values/abs/min/max/to_runes/parse_int/parse_float/print/range/assert + 模块内建
//! sqrt/floor/ceil/upper/parse/stringify（此前 math.floor/ceil 有 typeck 面而无
//! 运行时实现、string.upper 仅 demo 宿主有——注册表统一收口后 VM 全责实现）。
//! print 写入 Vm 输出缓冲（宿主经 take_output 取走，决策 70：print 走宿主通道）。
//! **对账**：`registry_dispatch_reconciled` 测试断言注册表每个名字都有分派臂。

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

use crate::Vm;
use crate::error::{RuntimeError, VmError};
use crate::value::Value;

impl Vm {
    pub(crate) fn dispatch_builtin(
        &mut self,
        name: &str,
        args: &[Value],
    ) -> Result<Value, VmError> {
        match name {
            "len" => {
                let v = arg(args, 0, name)?;
                let n = match &v {
                    Value::Str(s) => s.chars().count() as i64, // 码点计数（决策 46）
                    Value::List(l) => l.borrow().len() as i64,
                    Value::Map(m) => m.borrow().len() as i64,
                    other => {
                        return Err(self.builtin_err(
                            name,
                            format!("len expects str/list/map, got {}", other.display()),
                        ));
                    }
                };
                Ok(Value::Int(n))
            }
            "abs" => {
                let v = arg(args, 0, name)?;
                match v {
                    Value::Int(a) => {
                        Ok(Value::Int(a.checked_abs().ok_or_else(|| {
                            self.builtin_err(name, "integer overflow")
                        })?))
                    }
                    Value::Float(a) => Ok(Value::Float(a.abs())),
                    other => Err(self
                        .builtin_err(name, format!("abs expects number, got {}", other.display()))),
                }
            }
            "min" | "max" => {
                let a = arg(args, 0, name)?.clone();
                let b = arg(args, 1, name)?.clone();
                let ord = match (&a, &b) {
                    (Value::Int(x), Value::Int(y)) => x.cmp(y),
                    (Value::Float(x), Value::Float(y)) => x
                        .partial_cmp(y)
                        .ok_or_else(|| self.builtin_err(name, "NaN comparison unordered"))?,
                    _ => {
                        return Err(self.builtin_err(name, "min/max expects two numbers"));
                    }
                };
                let take_first = if name == "min" {
                    ord != Ordering::Greater
                } else {
                    ord != Ordering::Less
                };
                Ok(if take_first { a } else { b })
            }
            "parse_int" => {
                let v = arg(args, 0, name)?;
                let Value::Str(s) = &v else {
                    return Err(self.builtin_err(name, "parse_int expects string"));
                };
                Ok(mk_option(s.trim().parse::<i64>().ok().map(Value::Int)))
            }
            "parse_float" => {
                let v = arg(args, 0, name)?;
                let Value::Str(s) = &v else {
                    return Err(self.builtin_err(name, "parse_float expects string"));
                };
                Ok(mk_option(s.trim().parse::<f64>().ok().map(Value::Float)))
            }
            "to_runes" => {
                let v = arg(args, 0, name)?;
                let Value::Str(s) = &v else {
                    return Err(self.builtin_err(name, "to_runes expects string"));
                };
                let items: Vec<Value> = s.chars().map(|c| Value::Int(c as i64)).collect();
                self.alloc(items.len() as i64 * 16 + 32)?;
                Ok(Value::List(Rc::new(RefCell::new(items))))
            }
            "print" => {
                self.emit_line(args);
                Ok(Value::Nil)
            }
            "range" => match args.len() {
                1 => match &arg(args, 0, name)? {
                    Value::Int(n) => Ok(Value::Range(0, *n)),
                    other => Err(self
                        .builtin_err(name, format!("range expects int, got {}", other.display()))),
                },
                2 => {
                    let a = arg(args, 0, name)?.clone();
                    let b = arg(args, 1, name)?.clone();
                    match (a, b) {
                        (Value::Int(a), Value::Int(b)) => Ok(Value::Range(a, b)),
                        _ => Err(self.builtin_err(name, "range expects int bounds")),
                    }
                }
                n => Err(self.builtin_err(name, format!("range takes 1-2 args, got {n}"))),
            },
            "keys" | "values" => {
                let v = arg(args, 0, name)?;
                let Value::Map(m) = &v else {
                    return Err(self.builtin_err(name, format!("{name} expects map")));
                };
                let items: Vec<Value> = m
                    .borrow()
                    .iter()
                    .map(|(k, val)| {
                        if name == "keys" {
                            k.clone()
                        } else {
                            val.clone()
                        }
                    })
                    .collect();
                self.alloc(items.len() as i64 * 16 + 32)?;
                Ok(Value::List(Rc::new(RefCell::new(items))))
            }
            "map" | "filter" => {
                let coll = arg(args, 0, name)?.resolve_iterable()?;
                let f = arg(args, 1, name)?;
                let items = coll.expand();
                let mut out: Vec<Value> = Vec::with_capacity(items.len());
                for it in items {
                    let r = self.call_value(f.clone(), vec![it.clone()])?;
                    if name == "map" {
                        out.push(r);
                    } else {
                        let Value::Bool(keep) = r else {
                            return Err(self.builtin_err(name, "filter predicate must return bool"));
                        };
                        if keep {
                            out.push(it);
                        }
                    }
                }
                self.alloc(out.len() as i64 * 16 + 32)?;
                Ok(Value::List(Rc::new(RefCell::new(out))))
            }
            "fold" => {
                let coll = arg(args, 0, name)?.resolve_iterable()?;
                let mut acc = arg(args, 1, name)?.clone();
                let f = arg(args, 2, name)?.clone();
                for it in coll.expand() {
                    acc = self.call_value(f.clone(), vec![acc, it])?;
                }
                Ok(acc)
            }
            "sort" => {
                let v = arg(args, 0, name)?;
                let Value::List(l) = &v else {
                    return Err(self.builtin_err(name, "sort expects list"));
                };
                let mut items = l.borrow().clone();
                // 决策 47：float 不进全序——混入 float 直接拒绝（sort_floats 兜底）
                let all_int = items.iter().all(|x| matches!(x, Value::Int(_)));
                let all_str = items.iter().all(|x| matches!(x, Value::Str(_)));
                if all_int {
                    items.sort_by_key(|x| match x {
                        Value::Int(i) => *i,
                        _ => unreachable!(),
                    });
                } else if all_str {
                    items.sort_by(|a, b| match (a, b) {
                        (Value::Str(x), Value::Str(y)) => x.cmp(y),
                        _ => unreachable!(),
                    });
                } else {
                    return Err(self.builtin_err(
                        name,
                        "sort expects homogeneous int or str list (floats: sort_floats)",
                    ));
                }
                self.alloc(items.len() as i64 * 16 + 32)?;
                Ok(Value::List(Rc::new(RefCell::new(items))))
            }
            "sort_floats" => {
                let v = arg(args, 0, name)?;
                let Value::List(l) = &v else {
                    return Err(self.builtin_err(name, "sort_floats expects list"));
                };
                let mut items = l.borrow().clone();
                let mut any_nan = false;
                items.sort_by(|a, b| match (a, b) {
                    (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or_else(|| {
                        any_nan = true;
                        Ordering::Equal
                    }),
                    _ => Ordering::Equal,
                });
                if any_nan {
                    return Err(self.builtin_err(name, "NaN is unordered (decision 47)"));
                }
                self.alloc(items.len() as i64 * 16 + 32)?;
                Ok(Value::List(Rc::new(RefCell::new(items))))
            }
            "assert" => {
                // strip 产物（无调试段）：assert = no-op（决策 70）
                if self
                    .module
                    .as_ref()
                    .map(|m| m.debug.is_none())
                    .unwrap_or(true)
                {
                    return Ok(Value::Nil);
                }
                let v = arg(args, 0, name)?;
                let Value::Bool(ok) = &v else {
                    return Err(self.builtin_err(name, "assert expects bool"));
                };
                if !*ok {
                    let msg = match args.get(1) {
                        Some(m) => m.display(),
                        None => "assertion failed".to_string(),
                    };
                    let (proto, pc) = (self.cur_proto(), self.cur_pc());
                    return Err(VmError::Runtime(RuntimeError {
                        code: "Y-R9005",
                        message: msg,
                        proto,
                        pc,
                        src_off: self.debug().src_off(proto, pc),
                    }));
                }
                Ok(Value::Nil)
            }
            // ---- 模块内建（决策 77：此前 typeck 有面/VM 缺员或仅 demo 宿主有）----
            "sqrt" => {
                let v = arg(args, 0, name)?;
                let Value::Float(f) = &v else {
                    return Err(
                        self.builtin_err(name, format!("sqrt expects float, got {}", v.display()))
                    );
                };
                Ok(Value::Float(f.sqrt()))
            }
            "floor" | "ceil" => {
                let v = arg(args, 0, name)?;
                let Value::Float(f) = &v else {
                    return Err(self
                        .builtin_err(name, format!("{name} expects float, got {}", v.display())));
                };
                if !f.is_finite() {
                    return Err(self.builtin_err(name, format!("{name} expects finite float")));
                }
                let r = if name == "floor" { f.floor() } else { f.ceil() };
                const LIMIT: f64 = 9223372036854775808.0; // 2^63
                if (-LIMIT..LIMIT).contains(&r) {
                    Ok(Value::Int(r as i64))
                } else {
                    Err(self.builtin_err(name, "result out of int range"))
                }
            }
            "upper" => {
                let v = arg(args, 0, name)?;
                let Value::Str(s) = &v else {
                    return Err(self
                        .builtin_err(name, format!("upper expects string, got {}", v.display())));
                };
                Ok(Value::Str(Rc::from(s.to_uppercase().as_str())))
            }
            "parse" => {
                // T1：Json 为宿主不透明值（决策 28）——存解析文本，stringify 透传
                let v = arg(args, 0, name)?;
                let Value::Str(s) = &v else {
                    return Err(self
                        .builtin_err(name, format!("parse expects string, got {}", v.display())));
                };
                self.alloc(s.len() as i64)?;
                Ok(Value::Opaque(Rc::new(s.to_string())))
            }
            "stringify" => {
                let v = arg(args, 0, name)?;
                match &v {
                    Value::Opaque(o) => match o.downcast_ref::<String>() {
                        Some(s) => Ok(Value::Str(Rc::from(s.as_str()))),
                        None => Err(self.builtin_err(name, "stringify expects Json value")),
                    },
                    other => Err(self.builtin_err(
                        name,
                        format!("stringify expects Json, got {}", other.display()),
                    )),
                }
            }
            // 非内建名 → 宿主注册表（阶段 4，决策 76）：内建与宿主共用
            // `Const(Str(名))` callee 约定，分派在此收束；未注册 = Y-H9000。
            other => self.call_host(other, args),
        }
    }

    pub(crate) fn builtin_err(&self, name: &str, msg: impl Into<String>) -> VmError {
        let (proto, pc) = if !self.frames.is_empty() {
            (self.cur_proto(), self.cur_pc())
        } else {
            (usize::MAX, 0)
        };
        let src_off = self.debug().src_off(proto, pc);
        VmError::Runtime(RuntimeError {
            code: "Y-R9021",
            message: format!("{name}: {}", msg.into()),
            proto,
            pc,
            src_off,
        })
    }
}

/// Option 值构造（parse_* 内建）。
/// 运行期表示统一（D-05/D-20，26 号登记）：`T?` = `Nil | T`——None 为 Nil，
/// Some 为裸值。旧表示（Option 走 Variant 包装）使 nil 收窄（纯静态层）后的
/// 值在运行期仍是 Variant：算术透传侥幸正确，但 display/Ok 构造/返回值泄漏
/// `<variant N>`（D-20 Critical 静默错值）。spec 19 无 Some/None 模式匹配承诺，
/// 表示统一无观测破坏面。
pub fn mk_option(v: Option<Value>) -> Value {
    match v {
        Some(v) => v,
        None => Value::Nil,
    }
}

fn arg<'a>(args: &'a [Value], i: usize, name: &str) -> Result<&'a Value, VmError> {
    args.get(i).ok_or_else(|| {
        VmError::Runtime(RuntimeError {
            code: "Y-R9021",
            message: format!("{name}: missing argument {i}"),
            proto: usize::MAX,
            pc: 0,
            src_off: None,
        })
    })
}

enum Iterable {
    List(Vec<Value>),
    Range(i64, i64),
}

impl Iterable {
    /// 物化迭代序列（决策 31：迭代按值快照；T1 范围直接展开）。
    fn expand(self) -> Vec<Value> {
        match self {
            Iterable::List(items) => items,
            Iterable::Range(a, b) => (a..b).map(Value::Int).collect(),
        }
    }
}

impl Value {
    /// 内建迭代入口：List 快照 / Range 展开（决策 31：迭代按值快照）。
    fn resolve_iterable(&self) -> Result<Iterable, VmError> {
        match self {
            Value::List(l) => Ok(Iterable::List(l.borrow().clone())),
            Value::Range(a, b) => Ok(Iterable::Range(*a, *b)),
            other => Err(VmError::Runtime(RuntimeError {
                code: "Y-R9016",
                message: format!("{} is not iterable", other.display()),
                proto: usize::MAX,
                pc: 0,
                src_off: None,
            })),
        }
    }
}
