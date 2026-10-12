//! VM 错误模型（20 号错误码五域的 Y-R 运行时域）。
//!
//! 二分（QuickJS 印证，22 号决策 71）：
//! - [`VmError::Raised`]：RAISE 指令抛出的脚本值——**可被宿主捕获**；
//! - [`VmError::Runtime`]：VM 内建检查失败（越界/类型/溢出）——结构化，带 pc 与源偏移；
//! - [`VmError::Limit`] / [`VmError::Interrupted`]：资源三限与宿主中断——**不可捕获**，
//!   直接穿透一切脚本层，终止执行。

use crate::value::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum LimitKind {
    Instructions,
    CallDepth,
    HeapBytes,
    Deadline,
}

impl std::fmt::Display for LimitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LimitKind::Instructions => write!(f, "instruction budget exhausted"),
            LimitKind::CallDepth => write!(f, "call depth limit exceeded"),
            LimitKind::HeapBytes => write!(f, "heap byte limit exceeded"),
            LimitKind::Deadline => write!(f, "execution deadline exceeded"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeError {
    /// Y-R 错误码（20 号 §2）
    pub code: &'static str,
    pub message: String,
    /// 出错原型与 pc（行号换算由 DebugInfo + 源码在宿主侧完成）
    pub proto: usize,
    pub pc: usize,
    /// 源码字节偏移（调试段可用时填充；strip 产物为 None）
    pub src_off: Option<u32>,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {} (proto #{}, pc {})", self.code, self.message, self.proto, self.pc)?;
        if let Some(off) = self.src_off {
            write!(f, " @src+{off}")?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum VmError {
    /// RAISE 指令：脚本值沿调用链上抛，宿主可捕获
    Raised(Value),
    /// VM 内建运行时检查失败（不可被脚本捕获；Y-R）
    Runtime(RuntimeError),
    /// 资源限额（不可捕获，决策 71）
    Limit(LimitKind),
    /// 宿主中断钩子要求停止（不可捕获，决策 71）
    Interrupted,
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmError::Raised(v) => write!(f, "raised: {}", v.display()),
            VmError::Runtime(e) => write!(f, "{e}"),
            VmError::Limit(k) => write!(f, "limit: {k}"),
            VmError::Interrupted => write!(f, "interrupted by host"),
        }
    }
}

impl std::error::Error for VmError {}
