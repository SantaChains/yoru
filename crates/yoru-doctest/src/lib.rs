//! yoru-doctest：文档示例机械化验证（28 号 §2.3，C8 规格漂移防线）。
//!
//! 文档示例坏了 CI 红，而非 26 号式事后人工登记——本 crate 把「文档里的
//! Yoru 代码」变成可执行契约：提取 → 编译 → 运行 → 断言，一套设施吃两种语料：
//!
//! 1. **markdown 围栏块**：README / 设计文档中的 ```` ```yoru ```` 围栏；
//! 2. **`///` 文档注释内嵌块**（spec 19.1 既定：「`///` = doctest 源」，
//!    决策 83 词法一等保留后零成本可提取）：`.yoru` 源内 rustdoc 风格
//!    ```` ``` ```` 围栏，剥注释前缀后走同一提取器（单一实现，无第二套语义）。
//!
//! # 断言尾注约定（块内代码尾部，逐行识别）
//!
//! - `// => <期望行>`：期望 print 输出。连续多行 = 多行期望，与
//!   `take_output()` 逐位比对（`print(x)` 产 `"x\n"`）。
//! - `// error: <码>`：check 块——只 parse + typeck，断言 error 级诊断码
//!   集合**精确等于**所列码集（写全防漂移；不执行、不 lower）。
//! - 无尾注：全链契约——typeck 无错 + lower + verify + `run(main)` Ok。
//!
//! # 块形态约定
//!
//! 可执行块（无尾注 / `// =>`）必须是**完整编译单元**（含 `fn main`）——
//! 与 examples 套件同口径，VM `entry=None` 找 `main` 找不到即 Y-R9000。
//! check 块（`// error:`）可为片段。诊断码面暂限 parse + typeck
//! （Y-S / Y-T）；lower 码（Y-B 系）断言待泛型批次稳定后扩展。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use yoru_syntax::{Severity, TokenKind};
use yoru_typeck::Env;

/// 一个提取出的 doctest 块。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// 来源文件（报告用，原样回显）。
    pub file: String,
    /// 块在文件中的 1 起始行号（围栏开行）。
    pub line: usize,
    /// 剥除尾注后的可执行源码。
    pub code: String,
    /// `// =>` 期望输出行（有序）。
    pub expects: Vec<String>,
    /// `// error:` 断言诊断码（有序，比对时取集合）。
    pub errors: Vec<String>,
}

impl Block {
    /// 报告定位标签：`文件:行号`。
    pub fn label(&self) -> String {
        format!("{}:{}", self.file, self.line)
    }

    /// 断言形态：check 块（error 断言）不可执行，其余可执行。
    fn is_check_only(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// 单块裁决。`Fail` 携带人类可读原因（首因，不堆砌）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail(String),
}

/// 一批 doctest 的汇总报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub passed: usize,
    pub failed: usize,
    /// 失败明细（定位标签 + 原因），按执行序。
    pub failures: Vec<(String, String)>,
}

impl Report {
    fn merge(&mut self, block: &Block, verdict: Verdict) {
        match verdict {
            Verdict::Pass => self.passed += 1,
            Verdict::Fail(reason) => {
                self.failed += 1;
                self.failures.push((block.label(), reason));
            }
        }
    }
}

// ─────────────────────────── 提取 ───────────────────────────

/// markdown 源中提取全部 ```` ```yoru ```` 块。其他语言标记的围栏跳过。
pub fn extract_markdown(file: &str, src: &str) -> Vec<Block> {
    extract_fenced(file, src, 0)
}

/// `.yoru` 源中提取 `///` 文档注释内嵌的 ```` ```yoru ```` 块。
///
/// 连续的 DocComment 行剥 `///`（及一个可选空格）拼为 markdown 文本，
/// 再走围栏提取器——文档注释与 markdown 共用同一块语法与断言语义。
pub fn extract_doc_comments(file: &str, src: &str) -> Vec<Block> {
    let (tokens, _) = yoru_syntax::tokenize(src);
    let line_starts: Vec<usize> = line_starts(src);
    let mut md = String::new();
    let mut prev_end_line: Option<usize> = None;
    for t in &tokens {
        let TokenKind::DocComment(text) = &t.kind else {
            continue;
        };
        let line = line_of(&line_starts, t.span.start);
        // 仅连续段参与拼装——隔了代码的注释是另一篇文档。
        if prev_end_line.is_some_and(|p| line > p + 1) && !md.is_empty() {
            md.push_str("\n\n");
        }
        md.push_str(text.strip_prefix("/").unwrap_or(text));
        md.push('\n');
        prev_end_line = Some(line);
    }
    if md.is_empty() {
        return Vec::new();
    }
    extract_fenced(file, &md, 0)
}

/// 围栏提取核心：扫 ```` ```lang ```` … ```` ``` ````，lang == "yoru" 才收。
///
/// `base_line` 为传入文本相对源文件的行偏移（doc 注释剥前缀不改变相对
/// 结构，markdown 直接传 0）——报告行号按传入文本计。
fn extract_fenced(file: &str, src: &str, base_line: usize) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut lines = src.lines().peekable();
    let mut line_no = 0usize;
    while let Some(line) = lines.next() {
        line_no += 1;
        let trimmed = line.trim_start();
        if !trimmed.starts_with("```") {
            continue;
        }
        let lang = trimmed[3..].trim();
        if lang != "yoru" {
            // 非目标围栏：跳到其闭合行，防止围栏内 ``` 扰动扫描。
            for l in lines.by_ref() {
                line_no += 1;
                if l.trim_start().starts_with("```") {
                    break;
                }
            }
            continue;
        }
        let open_line = line_no + base_line;
        let mut code = String::new();
        let mut expects = Vec::new();
        let mut errors = Vec::new();
        let mut closed = false;
        for body in lines.by_ref() {
            line_no += 1;
            if body.trim_start().starts_with("```") {
                closed = true;
                break;
            }
            let t = body.trim_start();
            if let Some(rest) = t.strip_prefix("// =>") {
                expects.push(rest.trim().to_string());
            } else if let Some(rest) = t.strip_prefix("// error:") {
                let code_str = rest.trim();
                if !code_str.is_empty() {
                    errors.push(code_str.to_string());
                }
            } else if t.starts_with("//") {
                // 普通注释：doctest 块内不参与断言，保留进源码（可执行块
                // 可能需要行注释说明；parser 对注释透明）。
                code.push_str(body);
                code.push('\n');
            } else {
                code.push_str(body);
                code.push('\n');
            }
        }
        if closed && !(code.trim().is_empty() && expects.is_empty() && errors.is_empty()) {
            blocks.push(Block {
                file: file.to_string(),
                line: open_line,
                code,
                expects,
                errors,
            });
        }
    }
    blocks
}

fn line_starts(src: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

fn line_of(starts: &[usize], offset: usize) -> usize {
    // 分界查找：starts 有序，返回 <= offset 的最大索引（0 起始行号 → +1）。
    let mut lo = 0usize;
    let mut hi = starts.len();
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        if starts[mid] <= offset {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo + 1
}

// ─────────────────────────── 执行 ───────────────────────────

/// 执行单个块。错误路径全部带定位上下文返回，不 panic。
pub fn check_block(block: &Block) -> Verdict {
    if block.is_check_only() {
        return check_error_block(block);
    }
    check_run_block(block)
}

/// check 块：error 级诊断码集合 == 断言集合（精确，写全防漂移）。
/// 走 `check_source` 单一入口（与可执行块同构复用）——此前手写
/// `parse_source + check_program` 管道存在接口差异：parse_source 词法
/// 错误提前返回只报**第一个**，check_source 累积全部诊断。坏块断言面
/// 应与 CLI `yoru check` 逐位一致（同一诊断管线，同一结果）。
fn check_error_block(block: &Block) -> Verdict {
    let env = Env::core();
    let diags = yoru_typeck::check_source(&block.code, &env).0;
    let got: BTreeSet<String> = diags
        .iter()
        .filter(|d| matches!(d.severity, Severity::Error))
        .filter_map(|d| d.code.clone())
        .collect();
    let want: BTreeSet<String> = block.errors.iter().cloned().collect();
    if got == want {
        return Verdict::Pass;
    }
    let fmt = |s: &BTreeSet<String>| {
        if s.is_empty() {
            "(none)".to_string()
        } else {
            s.iter().cloned().collect::<Vec<_>>().join(", ")
        }
    };
    Verdict::Fail(format!(
        "diagnostic codes mismatch: want [{}], got [{}]",
        fmt(&want),
        fmt(&got)
    ))
}

/// 可执行块：typeck 无错 → lower → verify → run(main) → 输出逐位比对。
fn check_run_block(block: &Block) -> Verdict {
    let env = Env::core();
    let (diags, ok) = yoru_typeck::check_source(&block.code, &env);
    if !ok {
        let codes: Vec<String> = diags
            .iter()
            .filter(|d| matches!(d.severity, Severity::Error))
            .map(|d| d.code.clone().unwrap_or_else(|| "Y-????".into()))
            .collect();
        return Verdict::Fail(format!("typecheck failed: [{}]", codes.join(", ")));
    }
    let program = match yoru_syntax::parse_source(&block.code) {
        Ok(p) => p,
        Err(ds) => {
            return Verdict::Fail(format!(
                "parse failed after typecheck ok (internal): {} diagnostics",
                ds.len()
            ));
        }
    };
    let module = match yoru_bytecode::lower(&program) {
        Ok(m) => m,
        Err(ds) => {
            let codes: Vec<String> = ds
                .iter()
                .map(|d| d.code.clone().unwrap_or_else(|| "Y-B????".into()))
                .collect();
            return Verdict::Fail(format!("lowering failed: [{}]", codes.join(", ")));
        }
    };
    if let Err(errs) = yoru_bytecode::verify_module(&module) {
        return Verdict::Fail(format!(
            "structural verification failed (Y-I): {:?}",
            errs.first()
        ));
    }
    let inst = yoru_vm::Instance::new(module);
    let mut vm = yoru_vm::Vm::new();
    let result = vm.run(inst, None, vec![]);
    if let Err(e) = result {
        return Verdict::Fail(format!("runtime error: {e}"));
    }
    if block.expects.is_empty() {
        return Verdict::Pass;
    }
    // 逐位比对：期望行以 \n 连接、末尾补 \n——与 print 的行输出同构。
    let want = format!("{}\n", block.expects.join("\n"));
    let got = vm.take_output();
    if got == want.as_bytes() {
        Verdict::Pass
    } else {
        Verdict::Fail(format!(
            "output mismatch: want {:?}, got {:?}",
            want,
            String::from_utf8_lossy(&got)
        ))
    }
}

// ─────────────────────────── 入口 ───────────────────────────

/// 批量校验路径。`.md` 走围栏提取，`.yoru` 走文档注释提取；
/// 目录递归展开（按扩展名分流）。返回汇总报告。
pub fn check_paths(paths: &[PathBuf]) -> Result<Report, String> {
    let mut report = Report {
        passed: 0,
        failed: 0,
        failures: Vec::new(),
    };
    for path in collect_files(paths)? {
        let display = path.display().to_string();
        let src =
            std::fs::read_to_string(&path).map_err(|e| format!("cannot read {display}: {e}"))?;
        let blocks = if path.extension().is_some_and(|e| e == "yoru") {
            extract_doc_comments(&display, &src)
        } else {
            extract_markdown(&display, &src)
        };
        for block in &blocks {
            let verdict = check_block(block);
            report.merge(block, verdict);
        }
    }
    Ok(report)
}

/// 展开路径列表：目录递归（.md / .yoru），文件直收，其余扩展名跳过。
fn collect_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    fn push_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| format!("cannot read dir {}: {e}", dir.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                push_dir(&p, out)?;
            } else if p.extension().is_some_and(|e| e == "md" || e == "yoru") {
                out.push(p);
            }
        }
        Ok(())
    }
    for p in paths {
        if p.is_dir() {
            push_dir(p, &mut out)?;
        } else {
            out.push(p.clone());
        }
    }
    Ok(out)
}
