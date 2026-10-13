//! V0 smoke：cargo test 有界验证（30 号 §6.1「验收 = panic 零命中」）。
//! 长跑用 bin（`--profile fuzz`），此处 debug 模式小规模常驻回归。

use crate::{fuzz_case, seeds, Outcome, Rng};

/// 固定种子确定性 smoke：种子轮转 × 800 case，零 panic 契约断言。
#[test]
fn v0_panic_free_smoke() {
    // 注意：不设静默 panic hook——assert/expect 消息依赖默认 hook 输出；
    // fuzz_case 内部已 catch_unwind，业务 panic 不外泄。
    let corpus = seeds();
    let base_seed = 0x5EED_2026_1013u64;
    let mut failures = Vec::new();
    let mut reached: std::collections::HashMap<&'static str, u64> = Default::default();
    for case_id in 0..800u64 {
        let src = corpus[(case_id as usize) % corpus.len()];
        let mut rng = Rng::new(base_seed ^ case_id.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        match fuzz_case(src, &mut rng) {
            Outcome::Reached(stage) => *reached.entry(stage.name()).or_insert(0) += 1,
            Outcome::Skipped => {}
            Outcome::Panicked { stage, msg } => failures.push((case_id, stage.name(), msg)),
        }
    }
    // 覆盖健康度：lex/parse/run 三关键点必须非零（parse = typeck 失败路径，
    // run = 全链；typeck 阶段 = lower 失败——实证其恒零：typeck 过则 lower
    // 从不失败，故不作断言）。全零说明变异器或 harness 失效。
    for stage in ["lex", "parse", "run"] {
        let n = reached.get(stage).copied().unwrap_or(0);
        assert!(n > 0, "stage {stage} never reached — harness coverage broken: {reached:?}");
    }
    if !failures.is_empty() {
        let report = failures
            .iter()
            .map(|(id, stage, msg)| format!("case {id} @ {stage}: {msg}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        std::fs::create_dir_all("fuzz-artifacts")
            .and_then(|_| std::fs::write("fuzz-artifacts/smoke_failures.txt", &report))
            .expect("archive smoke failures (check cwd)");
        panic!(
            "V0 panic-free contract violated ({} cases) — see fuzz-artifacts/smoke_failures.txt",
            failures.len()
        );
    }
}

/// 确定性回归：同 (seed, case_id) 重放结果逐字节一致。
#[test]
fn v0_deterministic_replay() {
    // 同进程测试共享 panic hook——一律不设静默 hook（会吞其他测试的
    // assert 消息）；fuzz_case 业务 panic 已被内部 catch_unwind 捕获。
    let corpus = seeds();
    for case_id in 0..40u64 {
        let src = corpus[(case_id as usize) % corpus.len()];
        let mk = || Rng::new(0xABCD ^ case_id.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let a = format!("{:?}", fuzz_case(src, &mut mk()));
        let b = format!("{:?}", fuzz_case(src, &mut mk()));
        assert_eq!(a, b, "non-deterministic outcome at case {case_id}");
    }
}
