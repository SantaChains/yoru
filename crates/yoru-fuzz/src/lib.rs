//! V0 crash-fuzz harness（30 号 §6.1）：全链 panic-free 契约的机械化验证。
//!
//! 契约（决策 74）：任何输入经 `lex → parse → typeck → lower → run(有限步)`
//! 五阶段，要么产出诊断/结构化错误，要么正常完成——**不允许 panic**。
//!
//! 运行期有限步由 VM 资源三限承接（`Limits::max_instructions` /
//! `max_call_depth` / `max_heap_bytes`），harness 零 VM 扩展。
//!
//! 平台边界（30 号 §6.1 预注）：libFuzzer 不支持 Windows——本 crate 为
//! 自研确定性 harness（`fuzz_case` 即 fuzz target 函数），未来 Linux/macOS
//! CI 侧可薄包装为 cargo-fuzz target（`fuzz_case(bytes)` 签名不变）。
//!
//! 诚实边界：栈溢出是 abort 类，catch_unwind 不可捕获——缓解 =
//! gen.rs 深度守卫（MAX_NEST）+ typeck/lower 内部 16MB 栈线程；
//! cargo-fuzz CI 侧由 ASan 兜底。

use std::panic::{catch_unwind, AssertUnwindSafe};

use yoru_vm::Limits;

pub mod corpus;
pub use corpus::{mutate, seeds, Rng};

#[cfg(test)]
mod tests;

/// 管线阶段（Outcome 记录最远到达点 / panic 发生点）。
/// 注意语义是「最远成功穿过」：Lex = parse 失败止步；Parse = parse 成功
/// 但 typeck 失败；Typeck = typeck 成功但 lower 失败；Run = 全链完成。
/// Lower 仅作 panic 发生点标记（lower 成功必然进入 run）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// lex（tokenize；gen 的种子 tokenize 亦走此面）
    Lex,
    Parse,
    Typeck,
    Lower,
    Run,
}

impl Stage {
    pub fn name(self) -> &'static str {
        match self {
            Stage::Lex => "lex",
            Stage::Parse => "parse",
            Stage::Typeck => "typeck",
            Stage::Lower => "lower",
            Stage::Run => "run",
        }
    }
}

/// 单 case 结果。
#[derive(Debug)]
pub enum Outcome {
    /// 全链无 panic 完成。载荷 = 最远到达阶段（Lex = parse 失败止步）。
    Reached(Stage),
    /// 深度守卫/体量守卫拒绝，未进入管线（非缺陷，不计数）。
    Skipped,
    /// **契约命中**：panic 发生在某阶段。
    Panicked { stage: Stage, msg: String },
}

fn panic_msg(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// fuzz target 函数：一个 case 的全链执行。
/// 输入为种子源码 + 确定性 RNG（变异在内部完成），结果可复现。
pub fn fuzz_case(seed_src: &str, rng: &mut Rng) -> Outcome {
    // ---- 阶段 1：变异生成（含种子 tokenize——lex 契约的第一输入通道）----
    let src = match catch_unwind(AssertUnwindSafe(|| mutate(seed_src, rng))) {
        Ok(Some(s)) => s,
        Ok(None) => return Outcome::Skipped,
        Err(p) => return Outcome::Panicked { stage: Stage::Lex, msg: panic_msg(p) },
    };

    // ---- 阶段 2：parse ----
    let program = match catch_unwind(AssertUnwindSafe(|| yoru_syntax::parse_source(&src))) {
        Ok(Ok(program)) => program,
        Ok(Err(_)) => return Outcome::Reached(Stage::Lex),
        Err(p) => return Outcome::Panicked { stage: Stage::Parse, msg: panic_msg(p) },
    };

    // ---- 阶段 3：typeck（内部 16MB 栈线程，panic 经 resume_unwind 传出）----
    let env = yoru_typeck::Env::core();
    let checked = match catch_unwind(AssertUnwindSafe(|| yoru_typeck::check_source(&src, &env))) {
        Ok((_, ok)) => ok,
        Err(p) => return Outcome::Panicked { stage: Stage::Typeck, msg: panic_msg(p) },
    };

    // ---- 阶段 4：lower（typeck 通过才值得降级；同内部线程）----
    if checked {
        let module = match catch_unwind(AssertUnwindSafe(|| yoru_bytecode::lower(&program))) {
            Ok(Ok(m)) => m,
            Ok(Err(_)) => return Outcome::Reached(Stage::Typeck),
            Err(p) => return Outcome::Panicked { stage: Stage::Lower, msg: panic_msg(p) },
        };

        // ---- 阶段 5：run（有限步 = 资源三限；预算收紧以保吞吐）----
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let inst = yoru_vm::Instance::new(module);
            let mut vm = yoru_vm::Vm::new();
            vm.set_limits(Limits {
                max_instructions: 100_000,
                max_call_depth: 64,
                max_heap_bytes: 16 * 1024 * 1024,
                check_interval: 1_000,
            });
            // print 输出写入 vm.output 缓冲——丢弃即可（预算限制了总量）
            let _ = vm.run(inst, None, vec![]);
            let _ = vm.take_output();
        }));
        if let Err(p) = outcome {
            return Outcome::Panicked { stage: Stage::Run, msg: panic_msg(p) };
        }
        return Outcome::Reached(Stage::Run);
    }
    Outcome::Reached(Stage::Parse)
}
