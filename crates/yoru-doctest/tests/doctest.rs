//! yoru-doctest 集成测试：提取器（markdown / doc 注释）、执行裁决、
//! examples 套件 typeck↔lower 对拍（V 线 V1 最小形态，30 号 §6）。

use yoru_doctest::{Block, check_block, extract_doc_comments, extract_markdown};

fn block(code: &str, expects: &[&str], errors: &[&str]) -> Block {
    Block {
        file: "test.md".into(),
        line: 1,
        code: code.to_string(),
        expects: expects.iter().map(|s| s.to_string()).collect(),
        errors: errors.iter().map(|s| s.to_string()).collect(),
    }
}

// ── 提取：markdown ──

#[test]
fn extract_skips_non_yoru_fences() {
    let md = "```text\nnot yoru\n```\n\n```yoru\nfn main() -> int {\n    print(1)\n}\n```\n";
    let blocks = extract_markdown("t.md", md);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].line, 5);
    assert!(blocks[0].code.contains("print(1)"));
}

#[test]
fn extract_separates_assertions_from_code() {
    let md = "```yoru\nfn main() -> int {\n    print(\"a\")\n    print(\"b\")\n}\n// => a\n// => b\n```\n";
    let b = &extract_markdown("t.md", md)[0];
    assert_eq!(b.expects, vec!["a", "b"]);
    assert!(b.errors.is_empty());
    assert!(!b.code.contains("=>"));
}

#[test]
fn extract_error_assertions() {
    let md = "```yoru\nfn broken() {\n    let x: int = true\n}\n// error: Y-T0002\n```\n";
    let b = &extract_markdown("t.md", md)[0];
    assert_eq!(b.errors, vec!["Y-T0002"]);
    assert!(b.expects.is_empty());
}

#[test]
fn extract_ignores_unclosed_fence() {
    let md = "```yoru\nfn main() -> int {\n";
    assert!(extract_markdown("t.md", md).is_empty());
}

// ── 提取：/// 文档注释 ──

#[test]
fn extract_doc_comment_fences() {
    let src = concat!(
        "/// 加倍示例。\n",
        "///\n",
        "/// ```yoru\n",
        "/// fn main() -> int {\n",
        "///     print(84)\n",
        "/// }\n",
        "/// // => 84\n",
        "/// ```\n",
        "fn double(x: int) -> int {\n",
        "    return x * 2\n",
        "}\n",
    );
    let blocks = extract_doc_comments("d.yoru", src);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].expects, vec!["84"]);
    assert!(blocks[0].code.contains("print(84)"));
}

#[test]
fn doc_comment_without_fence_yields_nothing() {
    let src = "/// 普通文档注释，无内嵌代码。\nfn f() {}\n";
    assert!(extract_doc_comments("d.yoru", src).is_empty());
}

// ── 执行裁决 ──

#[test]
fn run_block_output_matches() {
    let b = block(
        "fn main() -> int {\n    print(1 + 1)\n    return 0\n}\n",
        &["2"],
        &[],
    );
    assert_eq!(check_block(&b), yoru_doctest::Verdict::Pass);
}

#[test]
fn run_block_output_mismatch_fails() {
    let b = block(
        "fn main() -> int {\n    print(1 + 1)\n    return 0\n}\n",
        &["3"],
        &[],
    );
    assert!(matches!(check_block(&b), yoru_doctest::Verdict::Fail(_)));
}

#[test]
fn run_block_multiline_output() {
    let b = block(
        "fn main() -> int {\n    print(\"a\")\n    print(\"b\")\n    return 0\n}\n",
        &["a", "b"],
        &[],
    );
    assert_eq!(check_block(&b), yoru_doctest::Verdict::Pass);
}

#[test]
fn run_block_no_assertion_only_needs_success() {
    let b = block("fn main() -> int {\n    return 0\n}\n", &[], &[]);
    assert_eq!(check_block(&b), yoru_doctest::Verdict::Pass);
}

#[test]
fn run_block_runtime_error_fails() {
    let b = block("fn main() -> int {\n    throw \"boom\"\n}\n", &[], &[]);
    assert!(matches!(check_block(&b), yoru_doctest::Verdict::Fail(_)));
}

#[test]
fn check_block_error_code_matches() {
    // 码值经 yoru check 实证（probe：int 绑定 string 值 → Y-T0002）。
    let b = block(
        "fn broken() {\n    let x: int = \"s\"\n}\n",
        &[],
        &["Y-T0002"],
    );
    // 码值以 typeck 实际产码为准——若实现码不同，此断言会红（C8 防线本义）。
    let v = check_block(&b);
    assert_eq!(v, yoru_doctest::Verdict::Pass, "actual: {v:?}");
}

#[test]
fn check_block_expecting_error_but_compiles_fails() {
    let b = block("fn fine() -> int {\n    return 1\n}\n", &[], &["Y-T0002"]);
    assert!(matches!(check_block(&b), yoru_doctest::Verdict::Fail(_)));
}

#[test]
fn check_block_wrong_code_fails() {
    let b = block(
        "fn broken() {\n    let x: int = \"s\"\n}\n",
        &[],
        &["Y-T9999"],
    );
    assert!(matches!(check_block(&b), yoru_doctest::Verdict::Fail(_)));
}

// ── V1 最小对拍（30 号 §6：typeck/lower 分歧机械化信号） ──

#[test]
fn examples_typeck_lower_agree() {
    // examples 11 篇：typeck ok ⟺ lower ok——双面分歧（V0 抓到 10 例
    // typeck 过但 lower 拒的同族信号）在既有语料上必须为零。
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/math");
    let mut checked = 0;
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("examples/math exists")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "yoru"))
        .collect();
    entries.sort();
    for path in entries {
        let src = std::fs::read_to_string(&path).expect("read example");
        let env = yoru_typeck::Env::core().with_demo_host();
        let (diags, ok) = yoru_typeck::check_source(&src, &env);
        let lower_ok = match yoru_syntax::parse_source(&src) {
            Ok(p) => yoru_bytecode::lower(&p).is_ok(),
            Err(_) => false,
        };
        assert_eq!(
            ok, lower_ok,
            "{path:?}: typeck/lower verdict disagree (V1 signal)"
        );
        let _ = diags;
        checked += 1;
    }
    assert_eq!(checked, 11, "examples/math 全量 11 篇必须都在场");
}

// ── 仓库内播种语料自验 ──

#[test]
fn repo_readme_doctests_pass() {
    // CI 同款入口在本地全绿——README 的 doctest 块即契约。
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let report =
        yoru_doctest::check_paths(&[std::path::PathBuf::from(format!("{root}/README.md"))])
            .expect("readme readable");
    assert_eq!(
        report.failed, 0,
        "README doctest 失败: {:?}",
        report.failures
    );
    assert!(report.passed >= 1, "README 至少 1 块在场");
}
