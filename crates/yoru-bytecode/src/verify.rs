//! LBC0 结构化验证器（决策 9，13 号 §13.3）：validate-before-use。
//!
//! 通过前不得进入执行。五项检查 + 分支深度一致性：
//! 1. opcode 值域合法 + 操作数形态匹配（A/sA 与 opcode 组一致）；
//! 2. 全部跳转偏移落在原型指令流边界内；
//! 3. 编译期最大栈深声明与指令流静态重算一致（含分支汇合点深度一致、全程非负）；
//! 4. upvalue/常量池/原型/import/struct/字段索引全部在界内；
//! 5. CRC32 在反序列化层先行（module.rs），验证器在其后。
//!
//! 同时是 10 号 §10.2 第四层 fuzz 的直接靶子：对任意字节输入绝不 panic。

use crate::instr::{Instr, Opcode};
use crate::module::{Const, Lbc0Module};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    BadOpcode {
        pc: usize,
        opcode: u32,
    },
    JumpOutOfRange {
        pc: usize,
        target: i32,
        code_len: usize,
    },
    StackMismatch {
        pc: usize,
        expected: usize,
        got: usize,
    },
    StackUnderflow {
        pc: usize,
    },
    MaxStackMismatch {
        declared: u16,
        computed: usize,
    },
    ConstIndexOutOfBounds {
        pc: usize,
        index: u32,
        len: usize,
    },
    ConstKindMismatch {
        pc: usize,
        index: u32,
        want: &'static str,
    },
    ProtoIndexOutOfBounds {
        pc: usize,
        index: u32,
        len: usize,
    },
    UpvalueIndexOutOfBounds {
        pc: usize,
        index: u32,
        len: usize,
    },
    ImportIndexOutOfBounds {
        pc: usize,
        index: u32,
        len: usize,
    },
    StructIndexOutOfBounds {
        pc: usize,
        index: u32,
        len: usize,
    },
    FieldIndexOutOfBounds {
        pc: usize,
        index: u32,
        len: usize,
    },
    LocalIndexOutOfBounds {
        pc: usize,
        index: u32,
        locals: u16,
    },
    UnreachableInstruction {
        pc: usize,
    },
    MissingTerminator {
        proto: usize,
    },
    FallthroughPastEnd {
        pc: usize,
    },
    EmptyCode {
        proto: usize,
    },
    TrailingConstRefs {
        proto: usize,
        extra: Vec<u32>,
    },
    MissingConstRefs {
        proto: usize,
        missing: Vec<u32>,
    },
    BadConstRef {
        proto: usize,
        ref_idx: u32,
    },
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::BadOpcode { pc, opcode } => {
                write!(f, "pc {pc}: reserved/unknown opcode 0x{opcode:02X}")
            }
            VerifyError::JumpOutOfRange {
                pc,
                target,
                code_len,
            } => {
                write!(f, "pc {pc}: jump target {target} outside 0..{code_len}")
            }
            VerifyError::StackMismatch { pc, expected, got } => {
                write!(f, "pc {pc}: branch merge stack depth {got} != {expected}")
            }
            VerifyError::StackUnderflow { pc } => write!(f, "pc {pc}: stack underflow"),
            VerifyError::MaxStackMismatch { declared, computed } => {
                write!(f, "declared max_stack {declared} != computed {computed}")
            }
            VerifyError::ConstIndexOutOfBounds { pc, index, len } => {
                write!(f, "pc {pc}: const index {index} outside 0..{len}")
            }
            VerifyError::ConstKindMismatch { pc, index, want } => {
                write!(f, "pc {pc}: const[{index}] is not a {want}")
            }
            VerifyError::ProtoIndexOutOfBounds { pc, index, len } => {
                write!(f, "pc {pc}: proto index {index} outside 0..{len}")
            }
            VerifyError::UpvalueIndexOutOfBounds { pc, index, len } => {
                write!(f, "pc {pc}: upvalue index {index} outside 0..{len}")
            }
            VerifyError::ImportIndexOutOfBounds { pc, index, len } => {
                write!(f, "pc {pc}: import index {index} outside 0..{len}")
            }
            VerifyError::StructIndexOutOfBounds { pc, index, len } => {
                write!(f, "pc {pc}: struct index {index} outside 0..{len}")
            }
            VerifyError::FieldIndexOutOfBounds { pc, index, len } => {
                write!(f, "pc {pc}: field index {index} outside 0..{len}")
            }
            VerifyError::LocalIndexOutOfBounds { pc, index, locals } => {
                write!(f, "pc {pc}: local slot {index} outside 0..{locals}")
            }
            VerifyError::UnreachableInstruction { pc } => {
                write!(f, "pc {pc}: unreachable instruction")
            }
            VerifyError::MissingTerminator { proto } => {
                write!(f, "proto #{proto}: missing RETURN/RAISE terminator")
            }
            VerifyError::FallthroughPastEnd { pc } => {
                write!(f, "pc {pc}: fallthrough past end of code")
            }
            VerifyError::EmptyCode { proto } => write!(f, "proto #{proto}: empty code"),
            VerifyError::TrailingConstRefs { proto, extra } => {
                write!(
                    f,
                    "proto #{proto}: const_refs not referenced by code: {extra:?}"
                )
            }
            VerifyError::MissingConstRefs { proto, missing } => {
                write!(
                    f,
                    "proto #{proto}: code references missing from const_refs: {missing:?}"
                )
            }
            VerifyError::BadConstRef { proto, ref_idx } => {
                write!(
                    f,
                    "proto #{proto}: const_refs entry {ref_idx} outside const pool"
                )
            }
        }
    }
}

/// 变体载荷判定（TagNew 栈效应依赖）：该变体是否带载荷。
fn variant_has_payload(m: &Lbc0Module, c: &Const) -> Option<bool> {
    match c {
        Const::Variant { tag, variant } => {
            let t = m.tags.get(*tag as usize)?;
            Some(*t.variants.get(*variant as usize)?)
        }
        _ => None,
    }
}

/// 指令栈效应 `(pops, pushes)`。控制流（JMP/JMPIF/JMPIFNOT/RETURN/RAISE）不在本表。
/// 操作数越界返回 None（由调用方转成结构化错误）。
fn effect(op: Opcode, arg: u32, m: &Lbc0Module) -> Option<(usize, usize)> {
    Some(match op {
        Opcode::Const
        | Opcode::Nil
        | Opcode::True
        | Opcode::False
        | Opcode::NewClosure
        | Opcode::LoadLocal
        | Opcode::LoadUpvalue
        | Opcode::LoadImport => (0, 1),
        Opcode::NewList => (arg as usize, 1),
        Opcode::NewMap => (arg as usize * 2, 1),
        Opcode::NewStruct => {
            let st = m.structs.get(arg as usize)?;
            (st.fields.len(), 1)
        }
        Opcode::StoreLocal | Opcode::StoreUpvalue | Opcode::Pop => (1, 0),
        Opcode::Add
        | Opcode::Sub
        | Opcode::Mul
        | Opcode::Div
        | Opcode::Mod
        | Opcode::Band
        | Opcode::Bor
        | Opcode::Bxor
        | Opcode::Shl
        | Opcode::Shr
        | Opcode::Ushr
        | Opcode::Eq
        | Opcode::Ne
        | Opcode::Lt
        | Opcode::Le
        | Opcode::Gt
        | Opcode::Ge => (2, 1),
        Opcode::Neg
        | Opcode::Not
        | Opcode::IsNone
        | Opcode::Unwrap
        | Opcode::ToString
        | Opcode::IsErr
        | Opcode::Await
        | Opcode::Convert
        | Opcode::IsTag
        | Opcode::Payload
        | Opcode::Suspend
        | Opcode::Resume => (1, 1),
        Opcode::GetField => (1, 1), // 弹基址压字段
        Opcode::GetIndex => (2, 1), // 弹 (索引, 容器) 压值
        // 弹 (值, 索引, 容器) 压容器 —— 值语义原地变更（决策 13：refcount==1 时无观测别名）
        Opcode::SetIndex => (3, 1),
        // 弹 (值, 索引) 变异局部槽 —— 无压出（SetIndexLocal 快路径，T2b）
        Opcode::SetIndexLocal => (2, 0),
        // 双局部槽下标读 —— 无弹只压（GetIndexLL，决策 80 族对称推广）
        Opcode::GetIndexLL => (0, 1),
        // 融合条件跳转：弹 (b, a) 双操作数（P0a③「Cmp+JumpIf」）
        Opcode::JmpIfNotLt | Opcode::JmpIfNotLe | Opcode::JmpIfNotGt | Opcode::JmpIfNotGe => (2, 0),
        Opcode::SetField => (2, 1),
        Opcode::Call | Opcode::CallMethod | Opcode::TailCall | Opcode::CallHost => {
            (arg as usize + 1, 1) // 弹 callee+args 压结果
        }
        Opcode::TagNew => {
            // 弹载荷（若变体有载荷）压变体值
            (
                if variant_has_payload(m, m.consts.get(arg as usize)?)? {
                    1
                } else {
                    0
                },
                1,
            )
        }
        Opcode::NewRange => (2, 1),
        Opcode::Slice => (3, 1),
        Opcode::ForPrep => (1, 1), // 弹 iterable 压迭代游标
        Opcode::ForNext => (0, 1), // 弹下个值→局部槽，压 bool；游标留栈（Lua 式）
        Opcode::Return | Opcode::Raise => (1, 0),
        Opcode::Jmp | Opcode::JmpIf | Opcode::JmpIfNot => (0, 0),
    })
}

/// 摘要解释器：对单个原型做栈深静态重算。
/// 要求：分支汇合点深度一致、全程非负、无 fallthrough 越界。
/// 返回 (每 pc 深度表, 计算最大深度)。
fn analyze(code: &[Instr], m: &Lbc0Module) -> Result<(Vec<Option<usize>>, usize), VerifyError> {
    let n = code.len();
    let mut depth_at: Vec<Option<usize>> = vec![None; n];
    let mut max_depth = 0usize;
    let mut work = vec![(0usize, 0usize)];

    while let Some((pc, depth)) = work.pop() {
        if pc >= n {
            return Err(VerifyError::FallthroughPastEnd { pc });
        }
        if let Some(prev) = depth_at[pc] {
            if prev != depth {
                return Err(VerifyError::StackMismatch {
                    pc,
                    expected: prev,
                    got: depth,
                });
            }
            continue; // 已访问且深度一致
        }
        depth_at[pc] = Some(depth);

        let instr = code[pc];
        let op = instr.op;

        // 控制流指令单独展开后继
        match op {
            Opcode::Jmp => {
                let target = pc as i64 + instr.sarg() as i64;
                if target < 0 || target as usize >= n {
                    return Err(VerifyError::JumpOutOfRange {
                        pc,
                        target: instr.sarg(),
                        code_len: n,
                    });
                }
                work.push((target as usize, depth));
                continue;
            }
            Opcode::JmpIf | Opcode::JmpIfNot => {
                let target = pc as i64 + instr.sarg() as i64;
                if target < 0 || target as usize >= n {
                    return Err(VerifyError::JumpOutOfRange {
                        pc,
                        target: instr.sarg(),
                        code_len: n,
                    });
                }
                if depth == 0 {
                    return Err(VerifyError::StackUnderflow { pc });
                }
                let after = depth - 1; // 条件已被弹出：两条后继路径都在弹栈后深度
                work.push((target as usize, after));
                max_depth = max_depth.max(after);
                work.push((pc + 1, after));
                continue;
            }
            Opcode::JmpIfNotLt | Opcode::JmpIfNotLe | Opcode::JmpIfNotGt | Opcode::JmpIfNotGe => {
                let target = pc as i64 + instr.sarg() as i64;
                if target < 0 || target as usize >= n {
                    return Err(VerifyError::JumpOutOfRange {
                        pc,
                        target: instr.sarg(),
                        code_len: n,
                    });
                }
                if depth < 2 {
                    return Err(VerifyError::StackUnderflow { pc });
                }
                let after = depth - 2; // 双比较操作数已弹出
                work.push((target as usize, after));
                max_depth = max_depth.max(after);
                work.push((pc + 1, after));
                continue;
            }
            Opcode::Return | Opcode::Raise => {
                if depth == 0 {
                    return Err(VerifyError::StackUnderflow { pc });
                }
                max_depth = max_depth.max(depth);
                continue; // 终结指令，无后继
            }
            _ => {}
        }

        // 操作数界检查（决策 9 第 4 项；局部槽/upvalue 界在 verify_proto 内）
        match op {
            Opcode::Const | Opcode::IsTag | Opcode::Payload | Opcode::TagNew => {
                if instr.arg as usize >= m.consts.len() {
                    return Err(VerifyError::ConstIndexOutOfBounds {
                        pc,
                        index: instr.arg,
                        len: m.consts.len(),
                    });
                }
                // ISTAG/PAYLOAD/TagNew 的操作数必须是变体常量（决策 9 第 1 项的形态匹配）
                if matches!(op, Opcode::IsTag | Opcode::Payload | Opcode::TagNew)
                    && variant_has_payload(m, &m.consts[instr.arg as usize]).is_none()
                {
                    return Err(VerifyError::ConstKindMismatch {
                        pc,
                        index: instr.arg,
                        want: "variant",
                    });
                }
            }
            Opcode::NewClosure => {
                if instr.arg as usize >= m.protos.len() {
                    return Err(VerifyError::ProtoIndexOutOfBounds {
                        pc,
                        index: instr.arg,
                        len: m.protos.len(),
                    });
                }
            }
            Opcode::LoadImport => {
                if instr.arg as usize >= m.imports.len() {
                    return Err(VerifyError::ImportIndexOutOfBounds {
                        pc,
                        index: instr.arg,
                        len: m.imports.len(),
                    });
                }
            }
            Opcode::GetField | Opcode::SetField => {
                if instr.arg as usize >= m.field_names.len() {
                    return Err(VerifyError::FieldIndexOutOfBounds {
                        pc,
                        index: instr.arg,
                        len: m.field_names.len(),
                    });
                }
            }
            Opcode::NewStruct if instr.arg as usize >= m.structs.len() => {
                return Err(VerifyError::StructIndexOutOfBounds {
                    pc,
                    index: instr.arg,
                    len: m.structs.len(),
                });
            }
            _ => {}
        }

        let (pops, pushes) = effect(op, instr.arg, m).ok_or(match op {
            Opcode::NewStruct => VerifyError::StructIndexOutOfBounds {
                pc,
                index: instr.arg,
                len: m.structs.len(),
            },
            Opcode::TagNew => VerifyError::ConstIndexOutOfBounds {
                pc,
                index: instr.arg,
                len: m.consts.len(),
            },
            _ => VerifyError::BadOpcode {
                pc,
                opcode: op as u32,
            },
        })?;

        let after = depth
            .checked_sub(pops)
            .ok_or(VerifyError::StackUnderflow { pc })?
            + pushes;
        max_depth = max_depth.max(after);
        work.push((pc + 1, after));
    }

    Ok((depth_at, max_depth))
}

/// 全模块结构验证（决策 9）。通过后方可进入执行/持久化。
pub fn verify_module(m: &Lbc0Module) -> Result<(), Vec<VerifyError>> {
    let mut errs = Vec::new();
    for (pi, p) in m.protos.iter().enumerate() {
        if let Err(e) = verify_proto(m, pi, p) {
            errs.push(e);
        }
    }
    // 常量池级检查：Proto/Variant/Str 引用界（deserialize 已查一遍，此处复核防绕过）
    for c in &m.consts {
        match c {
            Const::Proto(i) => {
                if *i as usize >= m.protos.len() {
                    errs.push(VerifyError::ProtoIndexOutOfBounds {
                        pc: usize::MAX,
                        index: *i,
                        len: m.protos.len(),
                    });
                }
            }
            Const::Variant { tag, variant } => match m.tags.get(*tag as usize) {
                None => errs.push(VerifyError::ConstIndexOutOfBounds {
                    pc: usize::MAX,
                    index: *tag,
                    len: m.tags.len(),
                }),
                Some(t) => {
                    if *variant as usize >= t.variants.len() {
                        errs.push(VerifyError::ConstIndexOutOfBounds {
                            pc: usize::MAX,
                            index: *variant,
                            len: t.variants.len(),
                        });
                    }
                }
            },
            Const::Str(i) => {
                if *i as usize >= m.strings.len() {
                    errs.push(VerifyError::ConstIndexOutOfBounds {
                        pc: usize::MAX,
                        index: *i,
                        len: m.strings.len(),
                    });
                }
            }
            Const::Int(_) | Const::Float(_) | Const::Bool(_) => {}
        }
    }
    if errs.is_empty() { Ok(()) } else { Err(errs) }
}

/// 公共分析入口：对指定原型做栈深静态重算，返回最大栈深。
/// lowering 收尾与验证器共用同一算法（决策 9 第 3 项：声明 == 计算）。
pub fn compute_max_stack(m: &Lbc0Module, proto_idx: usize) -> Result<usize, VerifyError> {
    let p = m
        .protos
        .get(proto_idx)
        .ok_or(VerifyError::EmptyCode { proto: proto_idx })?;
    if p.code.is_empty() {
        return Ok(0);
    }
    let (_, max) = analyze(&p.code, m)?;
    Ok(max)
}

fn verify_proto(m: &Lbc0Module, pi: usize, p: &crate::module::Proto) -> Result<(), VerifyError> {
    if p.code.is_empty() {
        return Err(VerifyError::EmptyCode { proto: pi });
    }
    let last = p.code.last().unwrap();
    if !matches!(last.op, Opcode::Return | Opcode::Raise) {
        return Err(VerifyError::MissingTerminator { proto: pi });
    }

    // 局部槽 / upvalue 界检查
    for (pc, i) in p.code.iter().enumerate() {
        if matches!(
            i.op,
            Opcode::LoadLocal | Opcode::StoreLocal | Opcode::ForPrep | Opcode::ForNext
        ) && i.arg >= p.locals as u32
        {
            return Err(VerifyError::LocalIndexOutOfBounds {
                pc,
                index: i.arg,
                locals: p.locals,
            });
        }
        if matches!(i.op, Opcode::LoadUpvalue | Opcode::StoreUpvalue)
            && i.arg as usize >= p.upvalues.len()
        {
            return Err(VerifyError::UpvalueIndexOutOfBounds {
                pc,
                index: i.arg,
                len: p.upvalues.len(),
            });
        }
    }

    // 栈深静态重算（决策 9 第 3 项）：声明 == 计算
    let (depth_at, computed) = analyze(&p.code, m)?;
    if computed != p.max_stack as usize {
        return Err(VerifyError::MaxStackMismatch {
            declared: p.max_stack,
            computed,
        });
    }

    // 可达性：所有指令必须可达（canonical form；lowering 不产死码）
    for (pc, d) in depth_at.iter().enumerate() {
        if d.is_none() {
            return Err(VerifyError::UnreachableInstruction { pc });
        }
    }

    // 常量引用表交叉核对（决策 9 第 4 项的表级部分）
    let mut used: Vec<u32> = p
        .code
        .iter()
        .filter(|i| {
            matches!(
                i.op,
                Opcode::Const | Opcode::IsTag | Opcode::Payload | Opcode::TagNew
            )
        })
        .map(|i| i.arg)
        .collect();
    used.sort_unstable();
    used.dedup();
    for r in &p.const_refs {
        if *r as usize >= m.consts.len() {
            return Err(VerifyError::BadConstRef {
                proto: pi,
                ref_idx: *r,
            });
        }
    }
    let mut refs = p.const_refs.clone();
    refs.sort_unstable();
    refs.dedup();
    let missing: Vec<u32> = used.iter().filter(|u| !refs.contains(u)).copied().collect();
    if !missing.is_empty() {
        return Err(VerifyError::MissingConstRefs { proto: pi, missing });
    }
    let extra: Vec<u32> = refs.iter().filter(|r| !used.contains(r)).copied().collect();
    if !extra.is_empty() {
        return Err(VerifyError::TrailingConstRefs { proto: pi, extra });
    }
    Ok(())
}
