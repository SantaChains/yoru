//! yoru-typeck：Yoru 类型检查器（阶段 1，20 号路线图）。
//!
//! - 退出标准：签名必标（文法保证+复核）/nil 智能收窄（决策 40）/协议 impl 校验（21）/
//!   distinct（23）/as 唯一转换（26）/Result 忽略警告（34）/lint 框架；
//! - 诊断 JSON schema v1（code 一等字段）经快照测试锁定；
//! - 权威样例（19.5）typeck 零错误；每类类型错误有 fix-it 建议。

mod check;
mod env;
mod ty;

pub use env::{Env, HostSig};
pub use ty::Ty;

use yoru_syntax::{ast, Diagnostic};

/// 解析 + 类型检查一站式入口（CLI 用）。
/// 类型检查入口。checker 对 AST 的递归与大栈线程配合（同 parse_source 注记）。
pub fn check_source(src: &str, env: &Env) -> (Vec<Diagnostic>, bool) {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(s, || check_source_inner(src, env))
            .expect("spawn typecheck thread")
            .join()
            .unwrap_or_else(|p| std::panic::resume_unwind(p))
    })
}

fn check_source_inner(src: &str, env: &Env) -> (Vec<Diagnostic>, bool) {
    let (tokens, mut diags) = yoru_syntax::tokenize(src);
    match yoru_syntax::parser::parse(&tokens) {
        Ok(program) => {
            let tdiags = check::check(&program, env);
            diags.extend(tdiags);
            let has_err = diags.iter().any(|d| d.is_error());
            (diags, !has_err)
        }
        Err(mut more) => {
            diags.append(&mut more);
            (diags, false)
        }
    }
}

/// 仅对已解析 AST 做类型检查（测试与宿主嵌入用）。
pub fn check_program(program: &ast::Program, env: &Env) -> Vec<Diagnostic> {
    check::check(program, env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::check;
    use yoru_syntax::parser;

    // 辅助函数提升为 pub(super)：供同层 registry_face_tests 对账模块复用
    pub(super) fn ck(src: &str) -> Vec<Diagnostic> {
        let (tokens, _ld) = yoru_syntax::tokenize(src);
        let program = parser::parse(&tokens).expect("parse");
        check(&program, &Env::core())
    }

    pub(super) fn errors(src: &str) -> Vec<Diagnostic> {
        ck(src).into_iter().filter(|d| d.is_error()).collect()
    }

    pub(super) fn warnings(src: &str) -> Vec<Diagnostic> {
        ck(src).into_iter().filter(|d| !d.is_error()).collect()
    }

    pub(super) fn codes(ds: &[Diagnostic]) -> Vec<String> {
        ds.iter().filter_map(|d| d.code.clone()).collect()
    }

    // ===== 权威样例 =====

    #[test]
    fn authoritative_sample_zero_errors() {
        let src = include_str!("../../yoru-syntax/examples/authoritative.yoru");
        let (tokens, ld) = yoru_syntax::tokenize(src);
        assert!(ld.is_empty(), "lex diagnostics: {ld:?}");
        let program = parser::parse(&tokens).expect("parse authoritative");
        let env = Env::core().with_demo_host();
        let diags = check(&program, &env);
        let errs: Vec<_> = diags.iter().filter(|d| d.is_error()).collect();
        assert!(errs.is_empty(), "typeck errors: {errs:#?}");
    }

    // ===== 决策 26：int/float 严格 as =====

    #[test]
    fn int_float_mixing_has_as_fixit() {
        let src = "fn f(a: int) -> float {\n    return a * 0.5\n}\n";
        let errs = errors(src);
        assert_eq!(codes(&errs), vec!["Y-T0003"], "{errs:#?}");
        assert!(errs[0].suggestions.iter().any(|s| s.contains("as float")));
    }

    #[test]
    fn explicit_as_passes() {
        let src = "fn f(a: int) -> float {\n    return a as float * 0.5\n}\n";
        assert!(errors(src).is_empty(), "{:#?}", errors(src));
    }

    // ===== 决策 27/40：T? 与智能收窄 =====

    #[test]
    fn option_widening_and_narrowing() {
        let src = concat!(
            "struct Item { size: int }\n",
            "fn pick(best: Item?) -> int {\n",
            "    if best != nil {\n",
            "        let b = best\n", // 收窄：b: Item
            "        return b.size\n",
            "    }\n",
            "    return 0\n",
            "}\n",
        );
        assert!(errors(src).is_empty(), "{:#?}", errors(src));
    }

    #[test]
    fn narrowing_does_not_leak_out_of_branch() {
        let src = concat!(
            "struct Item { size: int }\n",
            "fn pick(best: Item?) -> int {\n",
            "    if best != nil {\n",
            "        return 1\n",
            "    }\n",
            "    return best.size\n", // 分支外无收窄：Item? 无字段
            "}\n",
        );
        let errs = errors(src);
        assert!(
            errs.iter().any(|d| d.code.as_deref() == Some("Y-T0004")),
            "{errs:#?}"
        );
    }

    #[test]
    fn unwrap_on_non_option_rejected() {
        let src = "fn f(x: int) -> int {\n    return x!\n}\n";
        assert!(codes(&errors(src)).contains(&"Y-T0004".to_string()));
    }

    // ===== 决策 34：Result 忽略警告 =====

    #[test]
    fn discarded_result_warns() {
        let src = concat!(
            "tag E = Boom\n",
            "fn g() -> Result[int, E] {\n    return Ok(1)\n}\n",
            "fn f() {\n    g()\n}\n",
        );
        let ws = warnings(src);
        assert!(
            ws.iter().any(|d| d.code.as_deref() == Some("W-T0001")),
            "{ws:#?}"
        );
    }

    #[test]
    fn explicit_let_underscore_discard_is_silent() {
        let src = concat!(
            "tag E = Boom\n",
            "fn g() -> Result[int, E] {\n    return Ok(1)\n}\n",
            "fn f() {\n    let _ = g()\n}\n",
        );
        assert!(warnings(src).iter().all(|d| d.code.as_deref() != Some("W-T0001")));
    }

    // ===== 决策 21：协议 impl 校验 =====

    #[test]
    fn impl_missing_method_rejected() {
        let src = concat!(
            "struct V { x: int }\n",
            "impl Add for V {\n",
            "    fn add(a: V, b: V) -> V {\n        return V { x: a.x + b.x }\n    }\n",
            "}\n",
        );
        assert!(errors(src).is_empty(), "{:#?}", errors(src));
        let bad = concat!(
            "struct V { x: int }\n",
            "impl Add for V {\n",
            "    fn plus(a: V, b: V) -> V {\n        return V { x: a.x + b.x }\n    }\n",
            "}\n",
        );
        let errs = errors(bad);
        assert!(
            errs.iter().any(|d| d.code.as_deref() == Some("Y-T0009")),
            "{errs:#?}"
        );
    }

    #[test]
    fn operator_without_impl_rejected() {
        let src = concat!(
            "struct V { x: int }\n",
            "fn f(a: V, b: V) -> V {\n    return a + b\n}\n",
        );
        assert!(errors(src).iter().any(|d| d.code.as_deref() == Some("Y-T0022")));
    }

    // ===== 决策 23：distinct =====

    #[test]
    fn distinct_not_interchangeable() {
        let src = concat!(
            "type Id = distinct int\n",
            "fn f(a: Id, b: int) -> bool {\n    return a == b\n}\n",
        );
        assert_eq!(codes(&errors(src)), vec!["Y-T0010"]);
    }

    #[test]
    fn distinct_cast_roundtrip() {
        let src = concat!(
            "type Id = distinct int\n",
            "fn f(x: int) -> Id {\n    return x as Id\n}\n",
        );
        assert!(errors(src).is_empty(), "{:#?}", errors(src));
    }

    // ===== 决策 46/55/42：字符串/Map/切片 =====

    #[test]
    fn string_index_forbidden() {
        let src = "fn f(s: string) -> rune {\n    return s[0]\n}\n";
        assert_eq!(codes(&errors(src)), vec!["Y-T0025"]);
    }

    #[test]
    fn map_index_returns_option() {
        let src = "fn f(m: [string: int]) -> int? {\n    return m[\"k\"]\n}\n";
        assert!(errors(src).is_empty(), "{:#?}", errors(src));
    }

    #[test]
    fn slice_half_open() {
        let src = "fn f(xs: [int]) -> [int] {\n    return xs[0..2]\n}\n";
        assert!(errors(src).is_empty(), "{:#?}", errors(src));
    }

    // ===== 决策 52：空字面量 =====

    #[test]
    fn empty_list_requires_annotation() {
        let src = "fn f() -> [int] {\n    let xs = []\n    return xs\n}\n";
        assert!(codes(&errors(src)).contains(&"Y-T0013".to_string()));
    }

    // ===== 决策 45：float 不进全序 =====

    #[test]
    fn int_float_compare_rejected() {
        let src = "fn f(a: int, b: float) -> bool {\n    return a < b\n}\n";
        assert!(codes(&errors(src)).contains(&"Y-T0002".to_string()));
    }

    // ===== 决策 47：break/continue =====

    #[test]
    fn continue_outside_loop_rejected() {
        let src = "fn f() {\n    continue\n}\n";
        assert_eq!(codes(&errors(src)), vec!["Y-T0031"]);
    }

    // ===== 决策 48：arity =====

    #[test]
    fn arity_mismatch() {
        let src = concat!(
            "fn g(a: int) -> int {\n    return a\n}\n",
            "fn f() -> int {\n    return g(1, 2)\n}\n",
        );
        assert_eq!(codes(&errors(src)), vec!["Y-T0008"]);
    }

    // ===== 条件 bool =====

    #[test]
    fn truthiness_rejected() {
        let src =
            "fn f(xs: [int]) -> int {\n    if xs {\n        return 1\n    }\n    return 0\n}\n";
        assert_eq!(codes(&errors(src)), vec!["Y-T0005"]);
    }

    // ===== 决策 43：未使用警告 =====

    #[test]
    fn unused_variable_warns() {
        let src = "fn f() -> int {\n    let x = 1\n    return 2\n}\n";
        assert!(warnings(src).iter().any(|d| d.code.as_deref() == Some("W-T0002")));
    }

    #[test]
    fn unused_import_warns() {
        let src = "import math from \"std\"\nfn f() -> int {\n    return 1\n}\n";
        assert!(warnings(src).iter().any(|d| d.code.as_deref() == Some("W-T0003")));
    }

    // ===== 泛型调用推断 =====

    #[test]
    fn generic_builtin_call() {
        let src = concat!(
            "struct Item { size: int }\n",
            "fn f(xs: [Item]) -> [int] {\n",
            "    return map(xs, fn(it) = it.size)\n",
            "}\n",
        );
        assert!(errors(src).is_empty(), "{:#?}", errors(src));
    }

    // ===== JSON schema v1 快照 =====

    #[test]
    fn diagnostic_json_schema_snapshot() {
        let src = "fn f(a: int) -> float {\n    return a * 0.5\n}\n";
        let errs = errors(src);
        assert_eq!(errs.len(), 1);
        let json = &errs[0].to_json();
        let expected = serde_json::json!({
            "severity": "error",
            "message": "int 与 float 不可混算（决策 26）：`int` 与 `float`",
            "span": { "start": json["span"]["start"], "end": json["span"]["end"] },
            "code": "Y-T0003",
            "suggestions": ["显式转换：`x as float`"],
        });
        assert_eq!(json, &expected, "schema v1 快照不符");
    }

    // ===== 顶层缺失 return =====

    #[test]
    fn missing_return_rejected() {
        let src = "fn f() -> int {\n    let x = 1\n}\n";
        assert!(codes(&errors(src)).contains(&"Y-T0016".to_string()));
    }

    // ===== 阶段 4：宿主签名面（with_host_fns，12.2 契约）=====

    fn ck_with_hosts(src: &str, hosts: &[(&str, HostSig)]) -> Vec<Diagnostic> {
        let (tokens, _ld) = yoru_syntax::tokenize(src);
        let program = parser::parse(&tokens).expect("parse");
        let env = Env::core()
            .with_host_fns(hosts.iter().map(|(n, s)| (n.to_string(), s.clone())));
        check(&program, &env)
    }

    #[test]
    fn host_fn_registered_typechecks() {
        let sig = HostSig { params: vec![Ty::Int], ret: Ty::Int };
        let ds = ck_with_hosts(
            "pub fn main() -> int {\n    return double(21)\n}\n",
            &[("double", sig)],
        );
        assert!(ds.iter().filter(|d| d.is_error()).count() == 0, "{ds:#?}");
    }

    #[test]
    fn host_fn_unregistered_is_compile_error() {
        // 退出标准：能力未注入时调用 = 编译期错误（Y-T0001，带 did-you-mean 面）
        let ds = errors("pub fn main() -> int {\n    return ghost(1)\n}\n");
        assert!(codes(&ds).contains(&"Y-T0001".to_string()), "{ds:#?}");
    }

    #[test]
    fn host_fn_call_site_signature_checked() {
        // 签名契约在 typeck 校验：参数个数/类型不符 = 编译期错误
        let sig = HostSig { params: vec![Ty::Int], ret: Ty::Int };
        let bad_arity = ck_with_hosts(
            "pub fn main() -> int {\n    return double()\n}\n",
            &[("double", sig.clone())],
        );
        assert!(!bad_arity.is_empty(), "arity mismatch must be rejected");
        let bad_type = ck_with_hosts(
            "pub fn main() -> int {\n    return double(\"x\")\n}\n",
            &[("double", sig)],
        );
        assert!(!bad_type.is_empty(), "arg type mismatch must be rejected");
    }
}

// ===== 决策 77：注册表内建面 =====

#[cfg(test)]
mod registry_face_tests {
    use super::tests::*;
    use super::*;

    /// 注册表 ↔ Env 对账：每个 bare 名有 free 面，每个模块函数有模块面。
    #[test]
    fn env_faces_cover_registry() {
        let env = Env::core();
        for b in yoru_stdlib::BUILTINS {
            assert!(env.free_fns.contains_key(b.name), "free 面缺失: {}", b.name);
            if let Some(m) = b.module {
                assert!(
                    env.modules["std"].get(m).is_some_and(|mm| mm.contains_key(b.name)),
                    "模块面缺失: {m}.{}",
                    b.name
                );
            }
        }
    }

    #[test]
    fn print_variadic_any_values() {
        assert!(errors("fn f() {\n    print(1, \"a\", true, [1, 2])\n}").is_empty());
        assert!(errors("fn f() {\n    print()\n}").is_empty());
    }

    #[test]
    fn assert_optional_message() {
        assert!(errors("fn f(x: bool) {\n    assert(x)\n}").is_empty());
        assert!(errors("fn f(x: bool) {\n    assert(x, \"msg\")\n}").is_empty());
        assert!(!errors("fn f(x: string) {\n    assert(x)\n}").is_empty());
        assert!(!errors("fn f(x: bool) {\n    assert(x, 1)\n}").is_empty());
    }

    #[test]
    fn len_collection_face() {
        assert!(errors("fn f(s: string, xs: [int], m: [string: int]) -> int {\n    return len(s) + len(xs) + len(m)\n}").is_empty());
        assert!(!errors("fn f(x: int) -> int {\n    return len(x)\n}").is_empty());
    }

    #[test]
    fn numeric_builtins_same_kind() {
        assert!(errors("fn f() -> int {\n    return abs(0 - 3) + min(1, 2) + max(4, 5)\n}").is_empty());
        assert!(errors("fn f() -> float {\n    return min(1.0, 2.0)\n}").is_empty());
        // 决策 26：int/float 不隐式混合
        assert!(!errors("fn f() {\n    let x = min(1, 2.0)\n}").is_empty());
        // abs 返回位落地为实参类型（float 入 → float 出）
        assert!(errors("fn f() -> float {\n    return abs(3.5)\n}").is_empty());
    }

    #[test]
    fn math_module_faces() {
        // std 模块面需显式 import（19.2 单一命名空间；未用 import 另有 W-T0003）
        assert!(errors("import math from \"std\"\n\nfn f() -> float {\n    return math.sqrt(2.0)\n}").is_empty());
        assert!(errors("import math from \"std\"\n\nfn f() -> int {\n    return math.floor(2.7) + math.ceil(2.1)\n}").is_empty());
        // 决策 26：sqrt 不收 int
        assert!(!errors("import math from \"std\"\n\nfn f() -> float {\n    return math.sqrt(2)\n}").is_empty());
    }

    #[test]
    fn string_upper_registered() {
        // 回归：此前 string.upper 被 Y-T0007 误拦（typeck 面缺员）
        assert!(errors("import string from \"std\"\n\nfn f(s: string) -> string {\n    return string.upper(s)\n}").is_empty());
    }

    #[test]
    fn range_arity_overloads() {
        assert!(errors("fn f() -> int {\n    let mut n = 0\n    for i in range(3) {\n        n = n + i\n    }\n    return n\n}").is_empty());
        assert!(!errors("fn f() {\n    let r = range(1, 2, 3)\n}").is_empty());
    }

    #[test]
    fn parse_option_faces() {
        assert!(errors("fn f(s: string) -> int? {\n    return parse_int(s)\n}").is_empty());
        assert!(!errors("fn f(s: int) -> int? {\n    return parse_int(s)\n}").is_empty());
    }
}
