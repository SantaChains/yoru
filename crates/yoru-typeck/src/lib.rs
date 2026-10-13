//! yoru-typeck：Yoru 类型检查器（阶段 1，20 号路线图）。
//!
//! - 退出标准：签名必标（文法保证+复核）/nil 智能收窄（决策 40）/协议 impl 校验（21）/
//!   distinct（23）/as 唯一转换（26）/Result 忽略警告（34）/lint 框架；
//! - 诊断 JSON schema v1（code 一等字段）经快照测试锁定；
//! - 权威样例（19.5）typeck 零错误；每类类型错误有 fix-it 建议。

// 同 yoru-bytecode 裁决：typeck 公开签名以 AST（`Program`）/诊断
// （`Diagnostic`/`Span`）/AST 类型（`yoru_syntax::Type`）/注册表记录
// （`yoru_stdlib::Builtin`）为边界，管线消费者按构造依赖这些 crate。
#![allow(exported_private_dependencies)]

mod check;
mod env;
mod ty;

pub use env::{Env, HostSig};
pub use ty::Ty;

use yoru_syntax::{Diagnostic, ast};

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

    // ===== D-13：泛型函数 call-site 实例化 =====

    #[test]
    fn generic_fn_instantiation() {
        // D-13：`fn first[T](xs: [T]) -> T` 声明期 T 不得报未知类型；
        // 调用位按实参实例化（apply_sig 既有管线）。
        let ok = errors(
            "fn first[T](xs: [T]) -> T {\n    return xs[0]\n}\nfn main() -> int {\n    return first([1, 2, 3])\n}\n",
        );
        assert!(ok.is_empty(), "泛型 fn 声明与调用不应报错: {ok:#?}");
        // 跨类型实例化：同一泛型 fn 的 string 实例。
        let ok2 = errors(
            "fn first[T](xs: [T]) -> T {\n    return xs[0]\n}\nfn main() -> string {\n    return first([\"a\"])\n}\n",
        );
        assert!(ok2.is_empty(), "泛型 fn 第二实例不应报错: {ok2:#?}");
    }

    #[test]
    fn generic_fn_inference_via_lambda() {
        // T/U 双变量：U 经 lambda body 推断（bind_arg unify + check_lambda
        // contains_generic 跳过）；body 内函数值调用 f(x): U 为泛型域合法
        // 自由变量（apply_sig Y-T0018 兜底域内放行）。
        let ok = errors(
            "fn apply2[T, U](x: T, f: (T) -> U) -> U {\n    return f(x)\n}\nfn main() -> string {\n    return apply2(5, fn(n: int) = \"s\")\n}\n",
        );
        assert!(ok.is_empty(), "双变量泛型 + lambda 推断不应报错: {ok:#?}");
        // 泛型域内自由变量流动：body 内 f(x): U 传播到 return。
        let ok2 = errors(
            "fn apply1[T, U](f: (T) -> U, x: T) -> U {\n    let y = f(x)\n    return y\n}\n",
        );
        assert!(ok2.is_empty(), "泛型域内 f(x): U 不应报错: {ok2:#?}");
    }

    #[test]
    fn generic_fn_unbound_typaram_diag() {
        // U 只出现在返回位、调用位实参无法确定 → Y-T0018（apply_sig 既有
        // 诊断）；body 内 return x 另有 T≠U mismatch，此处断言调用位面。
        let bad = errors(
            "fn mk[T, U](x: T) -> U {\n    return x\n}\nfn main() -> int {\n    let _ = mk(5)\n    return 0\n}\n",
        );
        assert!(
            bad.iter().any(|d| d.code.as_deref() == Some("Y-T0018")),
            "未绑定泛型应 Y-T0018: {bad:#?}"
        );
    }

    // ===== 决策 26：int/float 严格 as =====

    #[test]
    fn opt_elem_list_lit_with_annotation() {
        // 决策 82：上下文期望 [T?] 时 nil 元素经 Ty::Nil 反向入口通过。
        let ok = errors(
            "fn main() -> int {\n    let xs: [int?] = [1, nil]\n    print(xs[0])\n    return 0\n}\n",
        );
        assert!(ok.is_empty(), "标注 [int?] 的 [1, nil] 不应报错: {ok:#?}");
        // 无标注仍拒绝（元素类型无法统一推断）。
        let bad = errors("fn main() -> int {\n    let ys = [1, nil]\n    return 0\n}\n");
        assert!(!bad.is_empty(), "无标注 [1, nil] 应报错");
    }

    #[test]
    fn map_for_iter_yields_kv() {
        // 决策 82 第二步：for-in-Map 产出 KV entry（.key/.value）；纯键
        // 通道迁移为 keys(m) 内建。
        let ok = errors(
            "fn main() -> int {\n    let mut m: [string: int] = [:]\n    m[\"a\"] = 1\n    let mut t = 0\n    for kv in m {\n        t = t + kv.value\n    }\n    for k in keys(m) {\n        t = t + 1\n    }\n    return t\n}\n",
        );
        assert!(ok.is_empty(), "KV entry + keys 通道不应报错: {ok:#?}");
        // KV 非法字段拒绝
        let bad = errors(
            "fn main() -> int {\n    let mut m: [string: int] = [:]\n    m[\"a\"] = 1\n    let mut t = 0\n    for kv in m {\n        t = t + kv.bogus\n    }\n    return t\n}\n",
        );
        assert!(
            bad.iter().any(|d| d.code.as_deref() == Some("Y-T0026")),
            "KV 非法字段应 Y-T0026（unit 级联可有）: {bad:#?}"
        );
        // 内建名保留：struct KV 声明拒绝
        let reserved = errors("struct KV {\n    x: int\n}\n");
        assert_eq!(codes(&reserved), vec!["Y-T0005"], "{reserved:#?}");
    }

    #[test]
    fn rune_int_as_roundtrip_allowed() {
        // 决策 81（27 号 A4 裁决方案①）：rune = distinct int 内建等价，
        // distinct 往返在决策 26 as 许可范围；lower CONVERT_RUNE / VM 码点
        // 域检查早已实现，此前仅 check_cast 漏列内建 Ty::Rune（Y-T0011 误拦）。
        let ok = errors(
            "fn f(r: rune) -> int {\n    return r as int\n}\nfn g(n: int) -> rune {\n    return n as rune\n}\n",
        );
        assert!(ok.is_empty(), "rune↔int as 不应报错: {ok:#?}");
        let bad = errors("fn f(r: rune) -> float {\n    return r as float\n}\n");
        assert_eq!(codes(&bad), vec!["Y-T0011"], "{bad:#?}");
    }

    #[test]
    fn struct_arith_fail_fast_even_with_impl() {
        // D-03 fail-fast（30 号定夺）：协议算术执行管线未接入前，**已 impl**
        // 的 struct 算术同样编译期拒绝——放行 = 虚假承诺（lower 发裸 Add、
        // VM Y-R9007 运行期崩），违 strict 健全。诊断区分已 impl / 未 impl
        // 两种情形，统一 Y-T0022。随泛型批次接入协议调用后此测试换面。
        let src = concat!(
            "struct V { x: float, y: float }\n",
            "impl Add for V {\n",
            "    fn add(a: V, b: V) -> V { return V { x: a.x + b.x, y: a.y + b.y } }\n",
            "}\n",
            "fn f(a: V, b: V) -> float {\n",
            "    let c = a + b\n",
            "    return c.x\n",
            "}\n",
        );
        let bad = errors(src);
        assert_eq!(codes(&bad), vec!["Y-T0022"], "{bad:#?}");
        assert!(
            bad[0].message.contains("已 impl"),
            "已 impl 情形诊断须明示管线未接入: {}",
            bad[0].message
        );
        // 未 impl：原语义保持（决策 21）
        let src2 = "struct P { x: float, y: float }\nfn f(a: P, b: P) -> float {\n    let c = a + b\n    return c.x\n}\n";
        let bad2 = errors(src2);
        assert_eq!(codes(&bad2), vec!["Y-T0022"], "{bad2:#?}");
        assert!(
            bad2[0].message.contains("未 impl"),
            "未 impl 情形诊断: {}",
            bad2[0].message
        );
    }

    #[test]
    fn unwrap_on_narrowed_ident_allowed() {
        // D-04（26 号登记）：nil 收窄（决策 40）后 `o!` 是恒等解包，
        // 曾被 Y-T0004 误拦且诊断自相矛盾（帮助文本自己建议先做 nil 检查）。
        let ds = errors(
            "fn f(o: int?) -> int {\n    if o != nil {\n        let v = o!\n        return v + 1\n    }\n    return 0\n}\n",
        );
        assert!(ds.is_empty(), "收窄分支内 `!` 不应报错: {ds:#?}");
    }

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
        assert!(
            warnings(src)
                .iter()
                .all(|d| d.code.as_deref() != Some("W-T0001"))
        );
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
        assert!(
            errors(src)
                .iter()
                .any(|d| d.code.as_deref() == Some("Y-T0022"))
        );
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
        assert!(
            warnings(src)
                .iter()
                .any(|d| d.code.as_deref() == Some("W-T0002"))
        );
    }

    #[test]
    fn unused_import_warns() {
        let src = "import math from \"std\"\nfn f() -> int {\n    return 1\n}\n";
        assert!(
            warnings(src)
                .iter()
                .any(|d| d.code.as_deref() == Some("W-T0003"))
        );
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
        // 边界文本契约（诊断 JSON schema v1）：parse 回验，语义与 Value 快照等价
        let json: serde_json::Value = serde_json::from_str(&errs[0].to_json()).expect("valid json");
        let expected = serde_json::json!({
            "severity": "error",
            "message": "int 与 float 不可混算（决策 26）：`int` 与 `float`",
            "span": { "start": json["span"]["start"], "end": json["span"]["end"] },
            "code": "Y-T0003",
            "suggestions": ["显式转换：`x as float`"],
        });
        assert_eq!(json, expected, "schema v1 快照不符");
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
        let env = Env::core().with_host_fns(hosts.iter().map(|(n, s)| (n.to_string(), s.clone())));
        check(&program, &env)
    }

    #[test]
    fn host_fn_registered_typechecks() {
        let sig = HostSig {
            params: vec![Ty::Int],
            ret: Ty::Int,
        };
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
        let sig = HostSig {
            params: vec![Ty::Int],
            ret: Ty::Int,
        };
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
                    env.modules["std"]
                        .get(m)
                        .is_some_and(|mm| mm.contains_key(b.name)),
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
        assert!(
            errors("fn f() -> int {\n    return abs(0 - 3) + min(1, 2) + max(4, 5)\n}").is_empty()
        );
        assert!(errors("fn f() -> float {\n    return min(1.0, 2.0)\n}").is_empty());
        // 决策 26：int/float 不隐式混合
        assert!(!errors("fn f() {\n    let x = min(1, 2.0)\n}").is_empty());
        // abs 返回位落地为实参类型（float 入 → float 出）
        assert!(errors("fn f() -> float {\n    return abs(3.5)\n}").is_empty());
        // D3 行为锁定：int 入 → int 出，可绑定 int（Num 不向上漂移）
        assert!(errors("fn f() -> int {\n    let y = abs(0 - 3)\n    return y\n}").is_empty());
    }

    #[test]
    fn math_module_faces() {
        // std 模块面需显式 import（19.2 单一命名空间；未用 import 另有 W-T0003）
        assert!(
            errors("import math from \"std\"\n\nfn f() -> float {\n    return math.sqrt(2.0)\n}")
                .is_empty()
        );
        assert!(errors("import math from \"std\"\n\nfn f() -> int {\n    return math.floor(2.7) + math.ceil(2.1)\n}").is_empty());
        // 决策 26：sqrt 不收 int
        assert!(
            !errors("import math from \"std\"\n\nfn f() -> float {\n    return math.sqrt(2)\n}")
                .is_empty()
        );
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

    // ===== 决策 78 / D1：模块限定内建名作值（值位归一）=====

    #[test]
    fn module_qualified_builtin_as_value() {
        // 回归：此前 `map(xs, math.floor)` 级联 4 条误诊（模块不能作为值 →
        // unit 无字段 → 泛型未绑定 → 类型不匹配），根因是 typeck 值位缺面
        assert!(
            errors(concat!(
                "import math from \"std\"\n\n",
                "fn f(xs: [float]) -> [int] {\n    return map(xs, math.floor)\n}\n",
            ))
            .is_empty()
        );
        assert!(
            errors(concat!(
                "import string from \"std\"\n\n",
                "fn g(ss: [string]) -> [[rune]] {\n    return map(ss, string.to_runes)\n}\n",
            ))
            .is_empty()
        );
        // 与裸名作值同构：同一内建函数面
        assert!(
            errors(concat!(
                "import math from \"std\"\n\n",
                "fn f(xs: [float]) -> [int] {\n    return map(xs, floor)\n}\n",
            ))
            .is_empty()
        );
    }

    #[test]
    fn std_module_field_miss_in_value_position_is_single_error() {
        // 值位字段缺失 = 调用位同一诊断（Y-T0007 根因首发），
        // 误导性级联（「模块不能作为值」「unit 无字段」）灭绝
        let errs = errors(
            "import math from \"std\"\n\nfn f(xs: [float]) -> [int] {\n    return map(xs, math.nope)\n}\n",
        );
        assert_eq!(
            errs.first().and_then(|d| d.code.as_deref()),
            Some("Y-T0007"),
            "{errs:#?}"
        );
        assert!(
            !errs
                .iter()
                .any(|d| d.message.contains("不能作为值") || d.message.contains("无字段")),
            "误导性级联复现: {errs:#?}"
        );
        // 宿主模块（非注册表）值访问维持原边界：模块不能作为值
        assert!(
            !errors("import game from \"host\"\n\nfn f() {\n    let x = game.thing\n}\n")
                .is_empty()
        );
    }

    #[test]
    fn module_field_probe_emits_single_undefined() {
        // 决策 78 配套：Field 分支探针走 lookup_opt（无诊断副作用），
        // 未定义模块名只经 check_expr 单次发射——修复前同 span 双 Y-T0001。
        // （合法派生诊断仍可出现，只锁「未定义名称」不重复。）
        let errs = errors("fn f(xs: [float]) -> [int] {\n    return map(xs, math.floor)\n}\n");
        let undefined: Vec<_> = errs
            .iter()
            .filter(|d| d.code.as_deref() == Some("Y-T0001"))
            .collect();
        assert_eq!(undefined.len(), 1, "同 span 双发射回归: {errs:#?}");
    }

    #[test]
    fn duplicate_diagnostics_suppressed() {
        // 级联抑制（rustc 同款）：精确重复（同码+同 span+同 message）只留首条。
        // 探针实证：`[any]` 作返回类型 + 局部标注时曾双发射「未知类型 any」。
        let errs = errors("fn f() -> [any] {\n    let fs: [any] = []\n    return fs\n}\n");
        let n = errs
            .iter()
            .filter(|d| d.message.contains("未知类型 `any`"))
            .count();
        assert_eq!(n, 1, "精确重复诊断应被抑制: {errs:#?}");
    }

    #[test]
    fn soft_types_never_user_writable() {
        // G3 反面锚（GDScript 渐进类型半成品教训，23-T2 §7）：Num/Coll/Any
        // 仅存于内建注册表面（same() 成员集判定），用户标注一律拒绝——
        // 「内建面专用类型外溢之日，即半成品类型系统之始」。
        for t in ["Num", "Coll", "Any", "any"] {
            let src = format!("fn f(x: {t}) -> int {{\n    return 0\n}}\n");
            assert!(!errors(&src).is_empty(), "`{t}` 必须不可用户书写");
        }
    }
}
