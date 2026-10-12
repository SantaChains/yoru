//! Yoru × Rust 嵌入示例（阶段 4 宿主桥，12.2 契约 / 决策 76/77/78）。
//!
//! 运行：`cargo run -p yoru-cli --example embed_host`
//!
//! 演示完整的嵌入管线——三处配对，缺一不可：
//! 1. **typeck 签名面**：`Env::core().with_host_fns(...)`（编译期闸口，
//!    未注入名字 = Y-T0001）；
//! 2. **lower 宿主名单**：`lower_with_hosts(&program, &names)`（lower 有
//!    独立名字解析 Y-T0913，只知道名单才放行 callee 名）；
//! 3. **VM 运行期闭包**：`Vm::register_host_fn(name, closure)`（fail-closed
//!    兜底：有签名无闭包 = 运行期 Y-H9000）。
//!
//! 双向交互：脚本调 Rust（宿主闭包，可 move 捕获宿主状态）+ Rust 调脚本
//! （`vm.run(inst, Some("fn"), args)` 传值收值）。

use yoru_bytecode::Lbc0Module;
use yoru_typeck::{Env, HostSig, Ty};
use yoru_vm::{Instance, Value, Vm};

/// 源码 → 字节码的完整管线（typeck 签名面 + lower 名单 + 结构验证）。
fn compile(src: &str, env: &Env, host_fns: &[&str]) -> Lbc0Module {
    let (diags, ok) = yoru_typeck::check_source(src, env);
    assert!(ok, "typeck failed: {diags:?}");
    let program = yoru_syntax::parse_source(src).expect("parse");
    let names: Vec<String> = host_fns.iter().map(|s| s.to_string()).collect();
    let module = yoru_bytecode::lower_with_hosts(&program, &names).expect("lower");
    yoru_bytecode::verify_module(&module).expect("verify");
    module
}

fn main() {
    // ---------- 1. 编译期签名面：宿主能力显式声明 ----------
    let env = Env::core().with_host_fns([
        (
            "roll".into(),
            HostSig {
                params: vec![Ty::Int],
                ret: Ty::Int,
            },
        ),
        (
            "tick".into(),
            HostSig {
                params: vec![],
                ret: Ty::Int,
            },
        ),
        (
            "echo".into(),
            HostSig {
                params: vec![Ty::Str],
                ret: Ty::Str,
            },
        ),
    ]);

    // ---------- 2. 运行期注入：纯 Rust 闭包，move 捕获宿主状态 ----------
    // 嵌入方视角：闭包直接捕获业务状态（计数器、连接池、RNG……），
    // 无需任何包装礼节（Rc::new / as HostFn 均由 API 内部完成）。
    let ticks = std::cell::RefCell::new(0i64);
    let mut vm = Vm::new();
    vm.register_host_fn("roll", |_vm, args| {
        // 确定性伪随机（示例不引 RNG 依赖）；真实宿主可接 rand / 游戏引擎
        let Value::Int(seed) = args[0] else {
            return Err(yoru_vm::VmError::Raised(Value::Str(
                "roll: expect int".into(),
            )));
        };
        Ok(Value::Int(
            seed.wrapping_mul(1103515245).wrapping_add(12345) % 100,
        ))
    });
    vm.register_host_fn("tick", move |_vm, _args| {
        let mut t = ticks.borrow_mut();
        *t += 1;
        Ok(Value::Int(*t))
    });
    vm.register_host_fn("echo", |_vm, args| {
        let Value::Str(s) = &args[0] else {
            return Err(yoru_vm::VmError::Raised(Value::Str(
                "echo: expect str".into(),
            )));
        };
        Ok(Value::Str(format!("<{s}>").into()))
    });

    let src = r#"
import math from "std"

pub fn main() -> int {
    let a = roll(42)
    let b = roll(a)
    print("echo:", echo("host"))
    print("rolls:", a, b, "sum:", a + b)
    return a + b
}

fn scale(x: float) -> float {
    return x * math.sqrt(4.0)
}
"#;

    // ---------- 3. 脚本 → Rust：跑入口函数，取回返回值与 print 输出 ----------
    let ret = vm
        .run(
            Instance::new(compile(src, &env, &["roll", "tick", "echo"])),
            None,
            vec![],
        )
        .expect("script run");
    let out = String::from_utf8(vm.take_output()).expect("utf8");
    println!("--- script output ---\n{out}--- main returned: {ret:?}");

    // ---------- 4. Rust → 脚本：反向调用脚本函数，传参收值 ----------
    // 宿主侧高频模式：把 Yoru 当可热重载的规则/公式引擎用。
    let scaled = vm
        .run(
            Instance::new(compile(src, &env, &["roll", "tick", "echo"])),
            Some("scale"),
            vec![Value::Float(3.0)],
        )
        .expect("call script fn from rust");
    println!("scale(3.0) = {scaled:?}");

    // ---------- 5. 双闸口安全语义 ----------
    // 编译期：未注入签名的名字 → Y-T0001
    let (diags, ok) = yoru_typeck::check_source("fn f() { ghost() }\n", &env);
    println!(
        "unregistered call typeck: ok={ok}, codes={:?}",
        diags
            .iter()
            .filter_map(|d| d.code.clone())
            .collect::<Vec<_>>()
    );
    // 运行期：有签名无闭包 → Y-H9000 fail-closed
    let env2 = Env::core().with_host_fns([(
        "half".into(),
        HostSig {
            params: vec![Ty::Int],
            ret: Ty::Int,
        },
    )]);
    let m = compile("fn f() -> int { return half(4) }\n", &env2, &["half"]);
    match vm.run(Instance::new(m), Some("f"), vec![]) {
        Err(e) => println!("signature-without-impl runtime: correctly failed -> {e}"),
        Ok(v) => panic!("must fail closed, got {v:?}"),
    }
    println!("embed example done");
}
