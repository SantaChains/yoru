//! V0 长跑入口：`cargo run --profile fuzz -p yoru-fuzz -- --iters N --seed S`
//!
//! 行为（libFuzzer 惯例微调）：
//! - panic 按 msg 分组归档到 `fuzz-artifacts/`（每组首例 + 全量计数），
//!   继续跑完统计全貌而非首崩即停（批量发现优先）；
//! - 结束时打印分阶段统计，存在 panic 则 exit 1；
//! - 确定性：`(seed, case_id)` 二元组可精确重放任何 case。

use yoru_fuzz::{Rng, Stage, fuzz_case, seeds};

fn main() {
    let mut iters: u64 = 10_000;
    let mut seed: u64 = 0xC0FFEE;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--iters" => {
                iters = args.next().and_then(|v| v.parse().ok()).unwrap_or(iters);
            }
            "--seed" => {
                seed = args
                    .next()
                    .and_then(|v| {
                        u64::from_str_radix(v.trim_start_matches("0x"), 16)
                            .or_else(|_| v.parse())
                            .ok()
                    })
                    .unwrap_or(seed);
            }
            other => {
                eprintln!("unknown arg: {other}\nusage: yoru-fuzz [--iters N] [--seed HEX]");
                std::process::exit(2);
            }
        }
    }

    // 静默 panic hook：catch_unwind 已捕获，默认 hook 的回溯打印纯噪声
    std::panic::set_hook(Box::new(|_| {}));

    let corpus = seeds();
    let mut stats: [u64; 5] = [0; 5];
    let mut skipped: u64 = 0;
    let mut diverged: u64 = 0;
    let mut panicked: Vec<(String, String, u64)> = Vec::new(); // (stage, msg首行, 计数)

    for case_id in 0..iters {
        let src = corpus[(case_id as usize) % corpus.len()];
        let mut rng = Rng::new(seed ^ case_id.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        match fuzz_case(src, &mut rng) {
            yoru_fuzz::Outcome::Reached(stage) => stats[stage_index(stage)] += 1,
            yoru_fuzz::Outcome::Skipped => skipped += 1,
            yoru_fuzz::Outcome::Diverged { src: annotated } => {
                // 双面分歧（V1 素材）：逐例归档，首例去重按内容哈希
                diverged += 1;
                let _ = std::fs::create_dir("fuzz-artifacts");
                let path = format!("fuzz-artifacts/diverg_case{case_id}.yoru");
                let _ = std::fs::write(&path, &annotated);
                eprintln!("DIVERGED @ case {case_id}: archived {path}");
            }
            yoru_fuzz::Outcome::Panicked { stage, msg } => {
                let key = (
                    stage.name().to_string(),
                    msg.lines().next().unwrap_or("").to_string(),
                );
                if let Some(e) = panicked
                    .iter_mut()
                    .find(|(s, m, _)| *s == key.0 && *m == key.1)
                {
                    e.2 += 1;
                } else {
                    let path = format!("fuzz-artifacts/crash_case{case_id}_{}.yoru", stage.name());
                    let _ = std::fs::create_dir("fuzz-artifacts");
                    let _ = std::fs::write(
                        &path,
                        format!("// seed=0x{seed:X} case={case_id} stage={}\n", stage.name()),
                    );
                    panicked.push((key.0, key.1, 1));
                    eprintln!("PANIC @ {} case {case_id}: archived {path}", stage.name());
                }
            }
        }
        if (case_id + 1) % 10_000 == 0 {
            eprintln!("... {} cases", case_id + 1);
        }
    }

    let total: u64 = stats.iter().sum::<u64>() + skipped;
    eprintln!("\n=== V0 fuzz report ===");
    eprintln!("iters={iters} seed=0x{seed:X} skipped={skipped} diverged={diverged}");
    for (i, n) in stats.iter().enumerate() {
        let name = [
            Stage::Lex,
            Stage::Parse,
            Stage::Typeck,
            Stage::Lower,
            Stage::Run,
        ][i]
            .name();
        // Typeck/Lower 仅作 panic 发生点标记：lower 拒已改走 Diverged，
        // reached 恒零——不打印死输出行。
        if matches!(name, "typeck" | "lower") {
            continue;
        }
        eprintln!(
            "reached {name}: {n} ({:.1}%)",
            *n as f64 * 100.0 / total.max(1) as f64
        );
    }
    if panicked.is_empty() {
        eprintln!("panic-free ✓");
    } else {
        eprintln!("\nPANICS ({} groups):", panicked.len());
        for (stage, msg, n) in &panicked {
            eprintln!("  [{stage}] x{n}: {msg}");
        }
        std::process::exit(1);
    }
}

fn stage_index(s: Stage) -> usize {
    match s {
        Stage::Lex => 0,
        Stage::Parse => 1,
        Stage::Typeck => 2,
        Stage::Lower => 3,
        Stage::Run => 4,
    }
}
