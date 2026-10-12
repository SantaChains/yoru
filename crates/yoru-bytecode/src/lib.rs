//! yoru-bytecode：LBC0 字节码层（阶段 2，20 号路线图）。
//!
//! - [`module`]：LBC0 模块格式（04 号 §4.5 定稿 + 阶段 2 实现细化）+ 序列化/CRC32；
//! - [`instr`]：opcode 表 + u32 指令编码（保留区 opcode = 拒绝）；
//! - [`verify`]：结构化验证器（决策 9 五项 + 分支深度一致性；validate-before-use）；
//! - [`disasm`]：反汇编器（round-trip 测试与调试输出）；
//! - [`lower`]：AST → LBC0 翻译（顶层两遍，决策 11；Lua 式 upvalue 捕获链）。
//!
//! 阶段 2 退出标准（20 号）：任意字节输入给验证器不 panic（fuzz 直靶）；
//! 合法程序 编码 → 验证 → 反汇编往返一致。

// 编译管线公开面固有暴露 `yoru_syntax` 的 `ast::Program`/`Diagnostic`
// （`lower` 的输入即 AST、错误即诊断）与 `yoru_stdlib` 的 `BUILTINS` 镜像
// 相关类型——管线消费者按构造依赖这些 crate，无版本错配面（workspace 内
// semver 同步移动）。故对本灯做 crate 级 allow：这不是疏漏，是「编译器
// 管线的公开签名以 AST/诊断为边界」的显式裁决（2026-10-12 外部消费者
// 构建实证：workspace 内 clippy 对该灯结构性失明，仅下游构建暴露）。
#![allow(exported_private_dependencies)]

pub mod disasm;
pub mod instr;
pub mod lower;
pub mod module;
pub mod verify;

pub use instr::{Instr, Opcode};
pub use lower::{lower, lower_with_hosts};
pub use module::{Const, Lbc0Module, Proto, StructInfo, TagInfo, UpvalueDesc, VERSION};
pub use verify::{VerifyError, verify_module};

#[cfg(test)]
mod tests {
    use super::*;
    use yoru_syntax::parse_source;

    const AUTHORITATIVE: &str = include_str!("../../yoru-syntax/examples/authoritative.yoru");

    fn lower_src(src: &str) -> Lbc0Module {
        let program = parse_source(src).expect("parse");
        lower(&program).expect("lower")
    }

    // ---- 退出标准 1：合法程序 编码 → 验证 → 反汇编往返一致 ----

    #[test]
    fn authoritative_lower_verify() {
        let m = lower_src(AUTHORITATIVE);
        verify_module(&m).expect("structural verification");
        assert!(!m.protos.is_empty());
    }

    #[test]
    fn serialize_deserialize_roundtrip() {
        let m = lower_src(AUTHORITATIVE);
        verify_module(&m).expect("verify before serialize");
        let bytes = m.serialize();
        let back = Lbc0Module::deserialize(&bytes).expect("deserialize");
        // 结构等价：serialize(back) == serialize(m)（canonical 字节级往返）
        assert_eq!(m.serialize(), back.serialize());
        // 验证器对往返结果仍然通过
        verify_module(&back).expect("verify after roundtrip");
    }

    #[test]
    fn disasm_roundtrip_stable() {
        let m = lower_src(AUTHORITATIVE);
        let d1 = disasm::disassemble(&m);
        let bytes = m.serialize();
        let back = Lbc0Module::deserialize(&bytes).expect("deserialize");
        let d2 = disasm::disassemble(&back);
        assert_eq!(d1, d2, "disassembly must be identical across roundtrip");
        assert!(d1.contains("proto #0"));
    }

    #[test]
    fn small_program_lowering() {
        let src = r#"
fn add(a: int, b: int) -> int {
    return a + b
}
pub fn main() -> int {
    return add(1, 2)
}
"#;
        let m = lower_src(src);
        verify_module(&m).expect("verify");
        // main 调 add：应有 NewClosure(add) + CALL
        let main = &m.protos[m.protos.len() - 1];
        assert!(main.code.iter().any(|i| i.op == Opcode::NewClosure));
        assert!(main.code.iter().any(|i| i.op == Opcode::Call));
    }

    #[test]
    fn control_flow_and_match_lowering() {
        let src = r#"
tag E = A | B(int) | C
pub fn pick(e: E) -> int {
    return match e {
        A -> 0
        B(n) -> n
        _ -> 2
    }
}
pub fn loops(n: int) -> int {
    let mut total = 0
    let mut i = 0
    while i < n {
        if i % 2 == 0 {
            i = i + 1
            continue
        }
        total = total + i
        i = i + 1
        if total > 100 {
            break
        }
    }
    for x in 0..n {
        total = total + x
    }
    return total
}
"#;
        let m = lower_src(src);
        verify_module(&m).expect("verify");
    }

    #[test]
    fn lambda_capture_upvalue() {
        let src = r#"
pub fn make(base: int) -> int {
    let f = fn(x: int) = x + base
    return 0
}
"#;
        let m = lower_src(src);
        verify_module(&m).expect("verify");
        // lambda 原型应捕获 base（upvalue）
        let lambda = m
            .protos
            .iter()
            .find(|p| !p.upvalues.is_empty())
            .expect("upvalue proto");
        assert!(lambda.upvalues.iter().any(|u| u.is_local));
    }

    #[test]
    fn interpolation_concat() {
        let src = "pub fn f(x: int) -> string {\n    return \"v=${x}\"\n}\n";
        let m = lower_src(src);
        verify_module(&m).expect("verify");
        let p = &m.protos[m.protos.len() - 1];
        assert!(p.code.iter().any(|i| i.op == Opcode::ToString));
        assert!(p.code.iter().any(|i| i.op == Opcode::Add));
    }

    #[test]
    fn try_dual_propagation() {
        let src = r#"
tag E = A
pub fn g(t: int) -> Result<int, E> {
    return Ok(try h(t))
}
fn h(x: int) -> Result<int, E> {
    return Ok(x)
}
"#;
        let m = lower_src(src);
        verify_module(&m).expect("verify");
        let main = m
            .protos
            .iter()
            .find(|p| p.code.iter().any(|i| i.op == Opcode::IsErr))
            .expect("try lowered");
        assert!(main.code.iter().any(|i| i.op == Opcode::IsNone));
    }

    // ---- 退出标准 2：任意字节输入给验证器/加载器不 panic（确定性伪 fuzz）----

    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut rng = XorShift(0x243F_6A88_85A3_08D3);
        for _ in 0..3000 {
            let len = (rng.next() % 512) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| (rng.next() & 0xFF) as u8).collect();
            // 只要求不 panic；结果 Err/Ok 均可
            if let Ok(m) = Lbc0Module::deserialize(&bytes) {
                let _ = verify_module(&m);
                let _ = disasm::disassemble(&m);
            }
        }
    }

    #[test]
    fn mutated_valid_modules_never_panic() {
        let m = lower_src(AUTHORITATIVE);
        let valid = m.serialize();
        let mut rng = XorShift(0x1319_8A2E_0370_7344);
        for _ in 0..3000 {
            let mut bytes = valid.clone();
            let flips = 1 + (rng.next() % 8);
            for _ in 0..flips {
                let pos = (rng.next() as usize) % bytes.len();
                bytes[pos] ^= (rng.next() & 0xFF) as u8;
            }
            // CRC 破坏 → Err；伪造一致 → 验证器兜底；均不得 panic
            let _ = Lbc0Module::deserialize(&bytes).map(|mm| {
                let _ = verify_module(&mm);
                let _ = disasm::disassemble(&mm);
            });
        }
    }

    // ---- 验证器负面用例（决策 9 逐项）----

    fn verified_minimal() -> Lbc0Module {
        let src = "fn f() -> int {\n    return 1\n}\n";
        lower_src(src)
    }

    #[test]
    fn bad_opcode_rejected() {
        let mut m = verified_minimal();
        // 未分配 opcode 0x67（0x66 已分配给 SetIndexLocal）：直接构造编码字
        m.protos[0].code[0] = Instr::decode(0x67, 0).unwrap_or(Instr::new(Opcode::Nil));
        // decode 必失败时上面的 fallback 不可用——用原始字直接验证负路径
        assert!(Instr::decode(0x67, 0).is_err());
        // 对模块级验证：改走 deserialize 路径更真实，但此处直接检查 verify 拒绝
        // （用占位 Nil 替换后仍应通过——故只断言 decode 负路径）
        let _ = m;
    }

    #[test]
    fn jump_out_of_bounds_rejected() {
        let mut m = verified_minimal();
        m.protos[0]
            .code
            .insert(0, Instr::with_arg(Opcode::Jmp, 0x7F_FFFF));
        assert!(verify_module(&m).is_err());
    }

    #[test]
    fn max_stack_mismatch_rejected() {
        let mut m = verified_minimal();
        m.protos[0].max_stack = 99;
        assert!(verify_module(&m).is_err());
    }

    #[test]
    fn const_index_oob_rejected() {
        let mut m = verified_minimal();
        m.protos[0].code[0] = Instr::with_arg(Opcode::Const, 999);
        assert!(verify_module(&m).is_err());
    }

    #[test]
    fn stack_underflow_rejected() {
        let mut m = verified_minimal();
        m.protos[0].code = vec![Instr::new(Opcode::Return)];
        m.protos[0].max_stack = 0;
        m.protos[0].const_refs.clear();
        assert!(verify_module(&m).is_err());
    }

    #[test]
    fn branch_depth_mismatch_rejected() {
        // JMPIFNOT 跳到压值点，fallthrough 也到同点但深度不同 → 汇合冲突必拒
        let m = Lbc0Module {
            version: VERSION,
            flags: 0,
            imports: vec![],
            tags: vec![],
            structs: vec![],
            field_names: vec![],
            consts: vec![],
            protos: vec![Proto {
                params: 0,
                locals: 0,
                max_stack: 1,
                code: vec![
                    Instr::new(Opcode::True),             // depth 1
                    Instr::with_arg(Opcode::JmpIfNot, 1), // pop → 0；跳 pc2（depth0）
                    Instr::new(Opcode::Nil),              // fallthrough 从 depth0 进入压 1
                    Instr::new(Opcode::Return),
                ],
                upvalues: vec![],
                const_refs: vec![],
            }],
            strings: vec![],
            debug: None,
        };
        // JMPIFNOT 目标 pc2 depth=0；fallthrough pc2 depth=0 → 一致！
        // 修正：让跳转目标是 pc3（Return 处 depth0），fallthrough 到 pc3 depth1 → 冲突
        let m2 = Lbc0Module {
            version: VERSION,
            flags: 0,
            imports: vec![],
            tags: vec![],
            structs: vec![],
            field_names: vec![],
            consts: vec![],
            protos: vec![Proto {
                params: 0,
                locals: 0,
                max_stack: 1,
                code: vec![
                    Instr::new(Opcode::True),             // depth 1
                    Instr::with_arg(Opcode::JmpIfNot, 2), // pop → 0；跳 pc3（depth0）
                    Instr::new(Opcode::Nil),              // fallthrough pc2：depth0 → 压 1
                    Instr::new(Opcode::Return), // pc3：从跳转来 depth0，从 fallthrough 来 depth1 → 冲突
                ],
                upvalues: vec![],
                const_refs: vec![],
            }],
            strings: vec![],
            debug: None,
        };
        assert!(verify_module(&m).is_ok() || verify_module(&m).is_err()); // 不 panic
        assert!(
            verify_module(&m2).is_err(),
            "branch depth mismatch must be rejected"
        );
    }

    #[test]
    fn unreachable_code_rejected() {
        let m = Lbc0Module {
            version: VERSION,
            flags: 0,
            imports: vec![],
            tags: vec![],
            structs: vec![],
            field_names: vec![],
            consts: vec![],
            protos: vec![Proto {
                params: 0,
                locals: 0,
                max_stack: 1,
                code: vec![
                    Instr::new(Opcode::Nil),
                    Instr::new(Opcode::Return),
                    Instr::new(Opcode::Nil), // 不可达
                    Instr::new(Opcode::Return),
                ],
                upvalues: vec![],
                const_refs: vec![],
            }],
            strings: vec![],
            debug: None,
        };
        assert!(verify_module(&m).is_err());
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use yoru_syntax::parse_source;

    /// 决策 77 对账：lower 的 BUILTIN_FNS 镜像 == 注册表 bare 名集合。
    /// 名单漂移（漏注册/幽灵名）在此处失败。
    #[test]
    fn builtin_fns_mirror_locked() {
        let mut mirror: Vec<&str> = BUILTIN_FNS.to_vec();
        mirror.sort_unstable();
        let mut reg = yoru_stdlib::builtin_names();
        reg.sort_unstable();
        assert_eq!(mirror, reg, "BUILTIN_FNS 与注册表漂移");
    }

    /// 决策 77：模块限定调用在 lower 归一为 bare 内建名——
    /// `string.to_runes(s)` 发射 Const(Str("to_runes"))，不再有 LoadImport/GetField。
    #[test]
    fn module_qualified_call_normalized() {
        let src = "import string from \"std\"\n\nfn f(s: string) -> int {\n    return len(string.to_runes(s))\n}\n";
        let program = parse_source(src).expect("parse");
        let m = lower(&program).expect("lower");
        let names: Vec<&str> = m
            .consts
            .iter()
            .filter_map(|c| match c {
                Const::Str(si) => m.strings.get(*si as usize).map(String::as_str),
                _ => None,
            })
            .collect();
        assert!(
            names.contains(&"to_runes"),
            "模块限定调用应归一为 bare 名: {names:?}"
        );
        // 限定调用不产生 import 读取（归一后 import 槽仅留档）
        assert!(
            !m.protos
                .iter()
                .flat_map(|p| &p.code)
                .any(|i| i.op == Opcode::LoadImport),
            "归一后不应有 LoadImport（此前 LoadImport(Nil)+GetField 运行期断链）"
        );
    }

    /// 决策 77：import 别名（`import json as j`）的限定调用同样归一。
    #[test]
    fn aliased_module_call_normalized() {
        let src = "import json as j from \"std\"\n\nfn f(s: string) -> string {\n    return stringify(j.parse(s))\n}\n";
        let program = parse_source(src).expect("parse");
        let m = lower(&program).expect("lower");
        let names: Vec<&str> = m
            .consts
            .iter()
            .filter_map(|c| match c {
                Const::Str(si) => m.strings.get(*si as usize).map(String::as_str),
                _ => None,
            })
            .collect();
        assert!(
            names.contains(&"parse") && names.contains(&"stringify"),
            "{names:?}"
        );
    }
}
