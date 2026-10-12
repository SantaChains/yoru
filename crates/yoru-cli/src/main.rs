//! yoru CLI：`yoru check` / `yoru build` / `yoru run` —— 前端 + typeck + LBC0 + VM。
//!
//! 阶段 1-3（20 号路线图）；clig.dev 退出码契约（决策 65）：
//! 0 = 无错误（可有警告）、1 = 有错误、2 = 用法错误。

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
            for a in &args[2..] {
                match a.as_str() {
                    "--host=demo" | "--host" => host_demo = true,
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
                Some(p) => run(&p, host_demo, entry.as_deref()),
                None => {
                    eprintln!("usage: yoru run [--host=demo] [--entry=F] <file.yoru>");
                    ExitCode::from(2)
                }
            }
        }
        Some("--version") | Some("-V") => {
            println!("yoru 0.1.0 (frontend+typeck+bytecode+vm, spec 19 v2, LBC0 1.0)");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!(
                "yoru — AI-native embedded language\n\nusage:\n  yoru check [--host=demo] <file.yoru>                 parse + typecheck, report diagnostics\n  yoru build [--host=demo] [--out=F] <file.yoru>      compile to LBC0 (.yoruc)\n  yoru run [--host=demo] [--entry=F] <file.yoru>      compile + execute via T1 VM\n  yoru --version"
            );
            ExitCode::from(2)
        }
    }
}

fn check(path: &str, host_demo: bool) -> ExitCode {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read {path}: {e}");
            return ExitCode::from(2);
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
            ExitCode::from(2)
        }
    }
}

fn run(path: &str, host_demo: bool, entry: Option<&str>) -> ExitCode {
    let module = match compile_to_module(path, host_demo) {
        Ok(m) => m,
        Err(code) => return code,
    };
    let mut inst = yoru_vm::Instance::new(module);
    if host_demo {
        yoru_vm::demo::install_demo_host(&mut inst);
    }
    let mut vm = yoru_vm::Vm::new();
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
            let display = v.display();
            if display != "nil" {
                println!("= {display}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("runtime error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// 源码 → typeck → lowering → 结构验证。任何失败打印诊断并返回对应退出码。
fn compile_to_module(path: &str, host_demo: bool) -> Result<yoru_bytecode::Lbc0Module, ExitCode> {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read {path}: {e}");
            return Err(ExitCode::from(2));
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

/// 人类可读输出：`severity [line:col] [code] message`（JSON 输出留 T5）。
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
    println!("{sev} [{line}:{col}] [{code}] {}", d.message);
    for s in &d.suggestions {
        println!("  help: {s}");
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
