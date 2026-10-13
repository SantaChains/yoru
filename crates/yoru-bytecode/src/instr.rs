//! 指令编码：opcode 表（04 号 §4.4）+ 扩展位（0x70-0x7F，阶段 2 落定的最小扩展集）。
//!
//! 编码：每条指令 u32 little-endian，bit0-7 = opcode，bit8-31 = 操作数（u24；
//! S24 形态按二进制补码解释为 i24，跳转相对当前指令偏移）。
//! 任何 opcode 表修改 = LBC0 版本升级（04 §4.8：禁止无声破坏）。

/// 0x70-0x7F 扩展位上的阶段 2 最小扩展（各自Stack效应见 verify::stack_effect）：
/// - `TOSTRING`      0x70 插值拼串的显示转换（19.1：插值 = 拼串唯一形式）
/// - `FORPREP`       0x71 for-in 迭代器建立（A = 循环变量局部槽）
/// - `FORNEXT`       0x72 for-in 步进（A = 循环变量局部槽；有下个值→存槽压 true，
///   穷尽→压 false；游标留栈，循环退出后 POP）
/// - `ISTAG`         0x73 match 变体测试（A = 变体常量池序号）
/// - `PAYLOAD`       0x74 match 载荷取出（A = 变体常量池序号；无载荷变体压 nil）
/// - `ISERR`         0x75 Result Err 判定（决策 54：try 双传播的 Result 半边）
/// - `AWAIT`         0x76 await 语义占位（阶段 3/4 定稿暂停/恢复协议）
/// - `CONVERT`       0x77 显式 as 的数值/码点转换（A = 目标类型标签，决策 26）
/// - `POP`           0x78 语句位求值结果丢弃
/// - `TAGNEW`        0x79 变体构造（A = 变体常量池序号；弹载荷压变体值）
/// - `NEWRANGE`      0x7A 区间值构造（决策 42：`a..b`）
/// - `SLICE`         0x7B 半开切片（决策 42：`xs[a..b]`）
///
/// 扩展指令的正式化（进 04 号 opcode 表或收回）随阶段 3 VM 定稿一并回写文档。
use crate::verify::VerifyError;

/// opcode 全集（04 号 §4.4 表 + 0x70-0x7B 阶段 2 扩展）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Opcode {
    // 0x00-0x0F 常量/构造
    Const = 0x00,
    Nil = 0x01,
    True = 0x02,
    False = 0x03,
    NewList = 0x04,
    NewMap = 0x05,
    NewStruct = 0x06,
    NewClosure = 0x07,
    // 0x10-0x1F 变量/闭包
    LoadLocal = 0x10,
    StoreLocal = 0x11,
    LoadUpvalue = 0x12,
    StoreUpvalue = 0x13,
    LoadImport = 0x14,
    // 0x20-0x2F 运算
    Add = 0x20,
    Sub = 0x21,
    Mul = 0x22,
    Div = 0x23,
    Mod = 0x24,
    Neg = 0x25,
    Not = 0x26,
    Band = 0x27,
    Bor = 0x28,
    Bxor = 0x29,
    Shl = 0x2A,
    Shr = 0x2B,
    Ushr = 0x2C,
    // 0x30-0x3F 比较/控制
    Eq = 0x30,
    Ne = 0x31,
    Lt = 0x32,
    Le = 0x33,
    Gt = 0x34,
    Ge = 0x35,
    Jmp = 0x36,
    JmpIf = 0x37,
    JmpIfNot = 0x38,
    // 0x40-0x4F 对象
    GetField = 0x40,
    SetField = 0x41,
    GetIndex = 0x42,
    SetIndex = 0x43,
    // 0x50-0x5F 调用
    Call = 0x50,
    CallMethod = 0x51,
    Return = 0x52,
    TailCall = 0x53, // P1（决策链 13.5），表内预留
    // 0x60-0x6F Option/异步/FFI/错误
    IsNone = 0x60,
    Unwrap = 0x61,
    Suspend = 0x62,
    Resume = 0x63,
    CallHost = 0x64,
    Raise = 0x65,
    /// 局部变量下标赋值：弹 (值, 索引)，对 `locals[arg]` 原地变异。
    /// 快路径（T2b 基准发现）：base 为局部变量时绕开栈操作数与局部槽的
    /// 双重引用，使决策 13 的 `refcount==1` 互斥判定真正可观测——
    /// 否则局部数组每次下标赋值都触发 O(n) 整表拷贝。
    SetIndexLocal = 0x66,
    /// 局部变量下标读：`locals[base][locals[idx]]`，arg = base_slot << 16 | idx_slot。
    /// 对称推广 SetIndexLocal（决策 80 族，P0a① 直方图驱动）：`xs[j]` 双局部
    /// 模式省 2 条 LoadLocal 与操作数栈往返（sort 负载 GetIndex 7.7% 近全为此形态）。
    GetIndexLL = 0x67,
    // 0x70-0x7F 阶段 2 扩展（见模块注释）
    ToString = 0x70,
    ForPrep = 0x71,
    ForNext = 0x72,
    IsTag = 0x73,
    Payload = 0x74,
    IsErr = 0x75,
    Await = 0x76,
    Convert = 0x77,
    Pop = 0x78,
    TagNew = 0x79,
    NewRange = 0x7A,
    Slice = 0x7B,
    // ---- 可捕获失败通道（决策 87：方案 A throws 单通道阶段①）----
    // CatchPush arg = catch 块入口 pc；Raise 先查本帧 handler 栈，命中则
    // 截栈压错误值跳转 catch 块，未命中沿调用链上抛（宿主边界语义不变）。
    CatchPush = 0x68,
    CatchPop = 0x69,
    // ---- 融合条件跳转（P0a③「Cmp+JumpIf」超指令，25 号 §A 直方图驱动）----
    // 语义 = `Xx; JmpIfNot target` 融合：弹 (b, a)，`!(a cmp b)` 成立则跳转。
    // 比较语义与独立指令逐位一致（含 Y-R9007/Y-R9011 错误面）；lower 窥孔
    // 在 JmpIfNot 发射点检测前置比较指令自动生成（sort 负载 -9.1% 指令）。
    JmpIfNotLt = 0x7C,
    JmpIfNotLe = 0x7D,
    JmpIfNotGt = 0x7E,
    JmpIfNotGe = 0x7F,
}

/// opcode 编码空间大小（0x00-0x67 连续 + 0x70-0x7B 扩展段）——
/// VM 直方图表驱动用（P0a① profile 释疑，25 号 §A）。
pub const OPCODE_COUNT: usize = 0x80;

impl Opcode {
    pub fn from_byte(b: u8) -> Option<Opcode> {
        // 表驱动：区间连续，直接 match 全枚举。
        Some(match b {
            0x00 => Opcode::Const,
            0x01 => Opcode::Nil,
            0x02 => Opcode::True,
            0x03 => Opcode::False,
            0x04 => Opcode::NewList,
            0x05 => Opcode::NewMap,
            0x06 => Opcode::NewStruct,
            0x07 => Opcode::NewClosure,
            0x10 => Opcode::LoadLocal,
            0x11 => Opcode::StoreLocal,
            0x12 => Opcode::LoadUpvalue,
            0x13 => Opcode::StoreUpvalue,
            0x14 => Opcode::LoadImport,
            0x20 => Opcode::Add,
            0x21 => Opcode::Sub,
            0x22 => Opcode::Mul,
            0x23 => Opcode::Div,
            0x24 => Opcode::Mod,
            0x25 => Opcode::Neg,
            0x26 => Opcode::Not,
            0x27 => Opcode::Band,
            0x28 => Opcode::Bor,
            0x29 => Opcode::Bxor,
            0x2A => Opcode::Shl,
            0x2B => Opcode::Shr,
            0x2C => Opcode::Ushr,
            0x30 => Opcode::Eq,
            0x31 => Opcode::Ne,
            0x32 => Opcode::Lt,
            0x33 => Opcode::Le,
            0x34 => Opcode::Gt,
            0x35 => Opcode::Ge,
            0x36 => Opcode::Jmp,
            0x37 => Opcode::JmpIf,
            0x38 => Opcode::JmpIfNot,
            0x40 => Opcode::GetField,
            0x41 => Opcode::SetField,
            0x42 => Opcode::GetIndex,
            0x43 => Opcode::SetIndex,
            0x50 => Opcode::Call,
            0x51 => Opcode::CallMethod,
            0x52 => Opcode::Return,
            0x53 => Opcode::TailCall,
            0x60 => Opcode::IsNone,
            0x61 => Opcode::Unwrap,
            0x62 => Opcode::Suspend,
            0x63 => Opcode::Resume,
            0x64 => Opcode::CallHost,
            0x65 => Opcode::Raise,
            0x66 => Opcode::SetIndexLocal,
            0x67 => Opcode::GetIndexLL,
            0x70 => Opcode::ToString,
            0x71 => Opcode::ForPrep,
            0x72 => Opcode::ForNext,
            0x73 => Opcode::IsTag,
            0x74 => Opcode::Payload,
            0x75 => Opcode::IsErr,
            0x76 => Opcode::Await,
            0x77 => Opcode::Convert,
            0x78 => Opcode::Pop,
            0x79 => Opcode::TagNew,
            0x7A => Opcode::NewRange,
            0x7B => Opcode::Slice,
            0x68 => Opcode::CatchPush,
            0x69 => Opcode::CatchPop,
            0x7C => Opcode::JmpIfNotLt,
            0x7D => Opcode::JmpIfNotLe,
            0x7E => Opcode::JmpIfNotGt,
            0x7F => Opcode::JmpIfNotGe,
            _ => return None,
        })
    }

    /// 操作数形态（04 号 §4.5 的 A/sA 区分；无操作数指令忽略 bit8-31）。
    pub fn operand_kind(self) -> OperandKind {
        match self {
            Opcode::Const
            | Opcode::NewList
            | Opcode::NewMap
            | Opcode::NewStruct
            | Opcode::NewClosure
            | Opcode::LoadLocal
            | Opcode::StoreLocal
            | Opcode::LoadUpvalue
            | Opcode::StoreUpvalue
            | Opcode::LoadImport
            | Opcode::GetField
            | Opcode::SetField
            | Opcode::Call
            | Opcode::CallMethod
            | Opcode::TailCall
            | Opcode::CallHost
            | Opcode::SetIndexLocal
            | Opcode::ForPrep
            | Opcode::ForNext
            | Opcode::IsTag
            | Opcode::Payload
            | Opcode::Convert
            | Opcode::TagNew
            | Opcode::CatchPush => OperandKind::U24,
            Opcode::Jmp | Opcode::JmpIf | Opcode::JmpIfNot => OperandKind::S24,
            _ => OperandKind::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandKind {
    None,
    U24,
    S24,
}

/// 单条指令。`arg` 恒为 u24 原始位型；S24 用 [`Instr::sarg`] 读。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instr {
    pub op: Opcode,
    pub arg: u32,
}

impl Instr {
    pub fn new(op: Opcode) -> Instr {
        Instr { op, arg: 0 }
    }

    pub fn with_arg(op: Opcode, arg: u32) -> Instr {
        debug_assert!(arg <= 0x00FF_FFFF, "operand exceeds u24");
        Instr { op, arg }
    }

    /// 有符号操作数（跳转偏移，相对当前指令）。
    pub fn sarg(self) -> i32 {
        let raw = self.arg;
        if raw & 0x0080_0000 != 0 {
            (raw | 0xFF00_0000) as i32 // 二进制补码展开
        } else {
            raw as i32
        }
    }

    /// 编码为 u32 小端字。
    pub fn encode(self) -> u32 {
        (self.op as u32) | (self.arg << 8)
    }

    /// 从 u32 字解码。保留区 opcode 返回结构化错误（决策 9 第 1 项）。
    pub fn decode(word: u32, pc: usize) -> Result<Instr, VerifyError> {
        let byte = (word & 0xFF) as u8;
        let op = Opcode::from_byte(byte).ok_or(VerifyError::BadOpcode {
            pc,
            opcode: word & 0xFF,
        })?;
        let arg = word >> 8;
        // S24 形态操作数不得超出 i24 值域
        if op.operand_kind() == OperandKind::S24 && arg & 0x0080_0000 == 0 && arg > 0x007F_FFFF {
            return Err(VerifyError::BadOpcode {
                pc,
                opcode: word & 0xFF,
            });
        }
        Ok(Instr { op, arg })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instr_roundtrip() {
        for b in 0u32..=0xFF {
            let word = b | (0x12_3456 << 8);
            if let Ok(i) = Instr::decode(word, 0) {
                assert_eq!(i.op as u32, b);
                assert_eq!(i.arg, 0x12_3456);
                assert_eq!(i.encode(), word);
            }
        }
    }

    #[test]
    fn reserved_opcode_rejected() {
        // 0x67 已分配 GetIndexLL（决策 80 族对称，P0a③）；0x68/0x69 已分配
        // CatchPush/CatchPop（决策 87）；0x6A-0x6F 仍为空洞
        assert!(Instr::decode(0x67, 0).is_ok());
        assert!(Instr::decode(0x68, 0).is_ok());
        assert!(Instr::decode(0x69, 0).is_ok());
        assert!(Instr::decode(0x6A, 0).is_err());
        assert!(Instr::decode(0xC0, 0).is_err());
    }

    #[test]
    fn sarg_negative() {
        let i = Instr::with_arg(Opcode::Jmp, (-4i32 as u32) & 0x00FF_FFFF);
        assert_eq!(i.sarg(), -4);
        let j = Instr::with_arg(Opcode::Jmp, 8);
        assert_eq!(j.sarg(), 8);
    }
}
