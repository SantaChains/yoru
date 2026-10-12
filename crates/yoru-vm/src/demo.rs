//! demo 宿主（`--host=demo`）：为权威样例/演示模块提供 import 面与 Ctx 对象。
//!
//! 能力即参数（决策 19）：VM 默认零能力，一切宿主能力经 Instance.imports 显式注入。
//! 本模块只是演示装配，不是沙箱边界。

use std::cell::RefCell;
use std::rc::Rc;

use crate::value::{HostFn, Value};
use crate::{Instance, Vm};

fn module(pairs: Vec<(&str, HostFn)>) -> Value {
    let m: Vec<(Value, Value)> = pairs
        .into_iter()
        .map(|(k, f)| (Value::Str(Rc::from(k)), Value::HostFn(f)))
        .collect();
    Value::Map(Rc::new(RefCell::new(m)))
}

/// 按名字给出 demo 宿主的 import 值。
///
/// **决策 77**：std 模块内建（math/string/json）已由 VM 原生注册表实现，
/// 且 lower 把模块限定调用归一为 bare 名分发——宿主不再需要为 std 模块
/// 提供 import 面（此前 demo 三张平行模块表正是 typeck/VM 漂移的病灶之一）。
/// 未知名仍给空模块（脚本 GETFIELD 得 nil，fail-open 归宿主）。
pub fn demo_import(_name: &str) -> Value {
    module(vec![])
}

/// demo Ctx 对象（ctx.log(...) 写入 VM 输出缓冲）。
pub fn demo_ctx() -> Value {
    module(vec![(
        "log",
        Rc::new(|vm: &mut Vm, args: &[Value]| {
            vm.emit_line(args);
            Ok(Value::Nil)
        }) as HostFn,
    )])
}

/// 为实例安装 demo 宿主 import 面（模块里 import 什么就绑什么）。
pub fn install_demo_host(inst: &mut Instance) {
    let names: Vec<String> = inst
        .module()
        .imports
        .iter()
        .filter_map(|&s| inst.module().strings.get(s as usize).cloned())
        .collect();
    for name in names {
        let _ = inst.set_import(&name, demo_import(&name));
    }
}

