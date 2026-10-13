//! yoru CLI：`yoru check` / `yoru build` / `yoru run` / `yoru fmt` / `yoru doctest`。
//!
//! 阶段 1-3（20 号路线图）+ F2（fmt）+ D 线（doctest）；clig.dev 退出码契约
//! （决策 65）：0 = 无错误（可有警告）、1 = 执行错误（含 IO/编译/运行/断言）、
//! 2 = 用法错误（unknown flag / 缺参数）。
//!
//! flag 语法统一约定：布尔 flag `--name`；带值 flag 一律 `--name=value`
//! （不支持空格形式，help 即文档）；`fmt` 额外保留 gofmt 系短形式 `-w`/`-l`。
//! 各子命令解析骨架同构（flag 臂 → 位置参数臂 → unknown 拒绝臂），
//! 语义各异不强 DRY（间接层损害可读性，2026-10-13 审计定夺）。

use std::process::ExitCode;
use yoru_syntax::Diagnostic;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("check") => {
            let mut path = None;
            let mut host_demo = false;
            for a in &args[2..] {
                match a.as_str() {
                    "--host=demo" | "--host" => host_demo = true,
                    s if !s.starts_with('-') => path = Some(s.to_string()),
                    s => {
                        eprintln!("error: unknown flag `{s}`");
                        return ExitCode::from(2);
                    }
                }
            }
            match path {
                Some(p) => check(&p, host_demo),
                None => {
                    eprintln!("usage: yoru check [--host=demo] <file.yoru>");
                    ExitCode::from(2)
                }
            }
        }
        Some("build") => {
            let mut path = None;
            let mut host_demo = false;
            let mut out: Option<String> = None;
            for a in &args[2..] {
                match a.as_str() {
                    "--host=demo" | "--host" => host_demo = true,
                    s if s.starts_with("--out=") => out = Some(s["--out=".len()..].to_string()),
                    s if !s.starts_with('-') => path = Some(s.to_string()),
                    s => {
                        eprintln!("error: unknown flag `{s}`");
                        return ExitCode::from(2);
                    }
                }
            }
            match path {
                Some(p) => build(&p, host_demo, out.as_deref()),
                None => {
                    eprintln!("usage: yoru build [--host=demo] [--out=<file.yoruc>] <file.yoru>");
                    ExitCode::from(2)
                }
            }
        }
        Some("run") => {
            let mut path = None;
            let mut host_demo = false;
            let mut entry: Option<String> = None;
            let mut show_stats = false;
            for a in &args[2..] {
                match a.as_str() {
                    "--host=demo" | "--host" => host_demo = true,
                    "--stats" => show_stats = true,
                    s if s.starts_with("--entry=") => {
                        entry = Some(s["--entry=".len()..].to_string())
                    }
                    s if !s.starts_with('-') => path = Some(s.to_string()),
                    s => {
                        eprintln!("error: unknown flag `{s}`");
                        return ExitCode::from(2);
                    }
                }
            }
            match path {
                Some(p) => run(&p, host_demo, entry.as_deref(), show_stats),
                None => {
                    eprintln!("usage: yoru run [--host=demo] [--entry=F] [--stats] <file.yoru>");
                    ExitCode::from(2)
                }
            }
        }
        Some("fmt") => {
            // gofmt 风格（F2）：默认 stdout；-w 原地写入；--check 只校验。
            // clig.dev 契约：0 = 已格式化、1 = 有差异/格式化失败、2 = 用法错误。
            let mut write_in_place = false;
            let mut check_only = false;
            let mut files: Vec<String> = Vec::new();
            for a in &args[2..] {
                match a.as_str() {
                    "-w" | "--write" => write_in_place = true,
                    "--check" | "-l" => check_only = true,
                    s if !s.starts_with('-') => files.push(s.to_string()),
                    s => {
                        eprintln!("error: unknown flag `{s}`");
                        return ExitCode::from(2);
                    }
                }
            }
            if files.is_empty() || (write_in_place && check_only) {
                eprintln!("usage: yoru fmt [--check|-l | -w] <file.yoru>...");
                ExitCode::from(2)
            } else {
                fmt(&files, write_in_place, check_only)
            }
        }
        Some("doctest") => {
            // 同构解析骨架（与 check/build/run 一致）：目前无 flag，但
            // `-` 开头的参数必须拒绝为用法错误——否则 flag 拼写错误会被
            // 静默当路径处理（"cannot read --foo"，接口不一致实证修复）。
            let mut paths: Vec<std::path::PathBuf> = Vec::new();
            for a in &args[2..] {
                match a.as_str() {
                    s if !s.starts_with('-') => paths.push(std::path::PathBuf::from(s)),
                    s => {
                        eprintln!("error: unknown flag `{s}`");
                        return ExitCode::from(2);
                    }
                }
            }
            if paths.is_empty() {
                eprintln!("usage: yoru doctest <file.md|.yoru|dir>...");
                ExitCode::from(2)
            } else {
                doctest(&paths)
            }
        }
        Some("-h") | Some("--help") | Some("help") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("--version") | Some("-V") => {
            println!("yoru 0.1.0 (frontend+typeck+bytecode+vm, spec 19 v2, LBC0 1.0)");
            ExitCode::SUCCESS
        }
        _ => {
            print_help();
            ExitCode::from(2)
        }
    }
}

/// 顶层 usage（`-h`/`--help` 与未知子命令共用同一份——单一事实源）。
/// help 成功请求 exit 0、未知命令 exit 2（clig.dev）。
fn print_help() {
    eprintln!(
        "yoru — AI-native embedded language\n\nusage:\n  yoru check [--host=demo] <file.yoru>                 parse + typecheck, report diagnostics\n  yoru build [--host=demo] [--out=F] <file.yoru>      compile to LBC0 (.yoruc)\n  yoru run [--host=demo] [--entry=F] [--stats] <file.yoru>  compile + execute via T1 VM\n  yoru fmt [--check|-l | -w] <file.yoru>...           format source (stdout | in-place | check)\n  yoru doctest <file.md|.yoru|dir>...                 extract and verify embedded code blocks\n  yoru -h | --help                                    show this help\n  yoru --version"
    );
}

/// 格式化（F2）。语义不变性由 yoru-fmt 的 D3 双断言 + 幂等验收协议在
/// crate 层保证；CLI 层薄封装：逐文件 format，stdout / -w / --check 三形态。
fn fmt(files: &[String], write_in_place: bool, check_only: bool) -> ExitCode {
    let mut changed = 0usize;
    let mut failed = 0usize;
    for path in files {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot read {path}: {e}");
                failed += 1;
                continue;
            }
        };
        let out = match yoru_fmt::format(&src) {
            Ok(o) => o,
            Err(diags) => {
                for d in &diags {
                    print_diagnostic(&src, d);
                }
                eprintln!(
                    "error: {path}: not formatted (parse error) — fmt only handles valid files"
                );
                failed += 1;
                continue;
            }
        };
        let differs = out != src;
        if check_only {
            if differs {
                println!("{path}: needs formatting");
                changed += 1;
            }
        } else if write_in_place {
            if differs {
                if let Err(e) = std::fs::write(path, &out) {
                    eprintln!("error: cannot write {path}: {e}");
                    failed += 1;
                    continue;
                }
                changed += 1;
            }
        } else {
            print!("{out}");
        }
    }
    if check_only {
        // 部分文件有差异 → 1（CI 可直接拦截）；读取/解析失败 → 1
        if changed > 0 || failed > 0 {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    } else if failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn check(path: &str, host_demo: bool) -> ExitCode {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            // IO/执行错误 = 1（与 fmt/doctest 对齐）；用法错误才 = 2
            eprintln!("error: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let env = if host_demo {
        yoru_typeck::Env::core().with_demo_host()
    } else {
        yoru_typeck::Env::core()
    };
    let (diags, ok) = yoru_typeck::check_source(&src, &env);
    for d in &diags {
        print_diagnostic(&src, d);
    }
    let errors = diags.iter().filter(|d| d.is_error()).count();
    let warnings = diags.len() - errors;
    if ok {
        if diags.is_empty() {
            println!("{path}: ok (0 diagnostics)");
        } else {
            println!("{path}: ok ({warnings} warning(s))");
        }
        ExitCode::SUCCESS
    } else {
        println!("{path}: {errors} error(s), {} total", diags.len());
        ExitCode::FAILURE
    }
}

fn build(path: &str, host_demo: bool, out: Option<&str>) -> ExitCode {
    let module = match compile_to_module(path, host_demo) {
        Ok(m) => m,
        Err(code) => return code,
    };
    let bytes = module.serialize();
    let out_path = out.map(String::from).unwrap_or_else(|| {
        let p = std::path::Path::new(path);
        p.with_extension("yoruc").to_string_lossy().into_owned()
    });
    match std::fs::write(&out_path, &bytes) {
        Ok(()) => {
            println!(
                "{path}: wrote {out_path} ({} bytes, {} protos, {} consts)",
                bytes.len(),
                module.protos.len(),
                module.consts.len()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: cannot write {out_path}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(path: &str, host_demo: bool, entry: Option<&str>, show_stats: bool) -> ExitCode {
    let module = match compile_to_module(path, host_demo) {
        Ok(m) => m,
        Err(code) => return code,
    };
    let mut inst = yoru_vm::Instance::new(module);
    if host_demo {
        yoru_vm::demo::install_demo_host(&mut inst);
    }
    let mut vm = yoru_vm::Vm::new();
    if show_stats {
        vm.enable_op_histogram();
    }
    // edition 2024 析构时序：run 的返回值临时量先绑定再 match——显式固定
    // 「结果临时量先于 vm/inst 析构」的次序（Value 无副作用 Drop，绑定使
    // 次序与 edition 无关，消除 tail-expr-drop-order 跨 edition 语义差）。
    let result = vm.run(inst, entry, vec![]);
    match result {
        Ok(v) => {
            let out = String::from_utf8_lossy(&vm.take_output()).into_owned();
            if !out.is_empty() {
                print!("{out}");
            }
            let display = vm.format_value(&v);
            if display != "nil" {
                println!("= {display}");
            }
            if show_stats {
                print_op_histogram(&mut vm);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            // Raised 值经模块 tag 表反查身份（27 号 A3）；其余错误走
            // VmError Display（无模块上下文的 fallback 格式）。
            if let yoru_vm::VmError::Raised(v) = &e {
                eprintln!("runtime error: raised: {}", vm.format_value(v));
            } else {
                eprintln!("runtime error: {e}");
            }
            ExitCode::FAILURE
        }
    }
}

/// 源码 → typeck → lowering → 结构验证。任何失败打印诊断并返回对应退出码。
fn compile_to_module(path: &str, host_demo: bool) -> Result<yoru_bytecode::Lbc0Module, ExitCode> {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            // IO/执行错误 = 1（与 fmt/doctest 对齐）；用法错误才 = 2
            eprintln!("error: cannot read {path}: {e}");
            return Err(ExitCode::FAILURE);
        }
    };
    let env = if host_demo {
        yoru_typeck::Env::core().with_demo_host()
    } else {
        yoru_typeck::Env::core()
    };
    let (diags, ok) = yoru_typeck::check_source(&src, &env);
    for d in &diags {
        print_diagnostic(&src, d);
    }
    if !ok {
        let errors = diags.iter().filter(|d| d.is_error()).count();
        println!("{path}: {errors} error(s), compilation aborted");
        return Err(ExitCode::FAILURE);
    }
    // typeck 通过 → lowering（第二遍，决策 11）
    let program = match yoru_syntax::parse_source(&src) {
        Ok(p) => p,
        Err(ds) => {
            for d in &ds {
                print_diagnostic(&src, d);
            }
            return Err(ExitCode::FAILURE);
        }
    };
    let module = match yoru_bytecode::lower(&program) {
        Ok(m) => m,
        Err(ds) => {
            for d in &ds {
                print_diagnostic(&src, d);
            }
            println!("{path}: bytecode lowering failed");
            return Err(ExitCode::FAILURE);
        }
    };
    // validate-before-use：编译器自身产物也必须过验证器（决策 9 的编译侧自检）
    if let Err(errs) = yoru_bytecode::verify_module(&module) {
        for e in &errs {
            eprintln!("internal: structural verification failed: {e}");
        }
        eprintln!("this is a Yoru bug (Y-I): please report with a minimal repro");
        return Err(ExitCode::FAILURE);
    }
    Ok(module)
}

/// 人类可读诊断：`severity [line:col] [code] message`（JSON 输出留 T5）。
/// 输出流契约（与 rustc/gcc 一致）：**诊断详情 → stderr**（日志类，clig.dev），
/// 结果摘要（`{path}: ok` / doctest 报告）与程序 print 输出 → stdout。
fn print_diagnostic(src: &str, d: &Diagnostic) {
    let (line, col) = match d.span {
        Some(span) => line_col(src, span.start),
        None => (0, 0),
    };
    let sev = match d.severity {
        yoru_syntax::Severity::Error => "error",
        yoru_syntax::Severity::Warning => "warning",
        yoru_syntax::Severity::Info => "info",
    };
    let code = d.code.as_deref().unwrap_or("-");
    eprintln!("{sev} [{line}:{col}] [{code}] {}", d.message);
    for s in &d.suggestions {
        eprintln!("  help: {s}");
    }
}

fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(src.len());
    let before = &src[..offset];
    let line = before.bytes().filter(|b| *b == b'\n').count() + 1;
    let col = before
        .rsplit('\n')
        .next()
        .map_or(1, |s| s.chars().count() + 1);
    (line, col)
}

/// doctest（D 线，28 号 §2.3）：提取 markdown ` ```yoru ` 围栏块与 `.yoru`
/// `///` 文档注释内嵌块，逐块执行断言。提取与执行语义全在 yoru-doctest
/// crate（lib 层可测试复用），CLI 层薄封装：报告 + clig.dev 退出码。
fn doctest(paths: &[std::path::PathBuf]) -> ExitCode {
    match yoru_doctest::check_paths(paths) {
        Ok(report) => {
            if report.failed == 0 {
                println!("doctest: {} passed", report.passed);
                ExitCode::SUCCESS
            } else {
                for (label, reason) in &report.failures {
                    println!("FAIL {label}: {reason}");
                }
                println!(
                    "doctest: {} passed, {} failed",
                    report.passed, report.failed
                );
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// P0a① opcode 直方图报告（--stats）：top 指令占比——超指令选型（25 号
/// §A）以真实负载分布为据，不凭感觉。名称反查用 Opcode Debug 分解。
fn print_op_histogram(vm: &mut yoru_vm::Vm) {
    let Some(hist) = vm.take_op_histogram() else {
        return;
    };
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return;
    }
    let mut ranked: Vec<(u8, u64)> = hist
        .iter()
        .enumerate()
        .filter(|&(_, &n)| n > 0)
        .map(|(i, &n)| (i as u8, n))
        .collect();
    ranked.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
    println!("-- opcode histogram (total {total}) --");
    for (op, n) in ranked.iter().take(12) {
        let name = yoru_bytecode::Opcode::from_byte(*op)
            .map(|o| format!("{o:?}"))
            .unwrap_or_else(|| format!("0x{op:02X}"));
        println!(
            "  {:>12}  {:>9}  {:>5.1}%",
            name,
            n,
            *n as f64 / total as f64 * 100.0
        );
    }
}
