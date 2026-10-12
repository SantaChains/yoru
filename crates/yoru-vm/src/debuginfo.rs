//! 调试段解析（决策 70：debug/strip 双产物）。
//!
//! lower 产出的 "YDI1" 段：原型名表 + 每 pc 源偏移（RLE）。strip 产物无此段——
//! 语义不变，仅损失源级回溯（差分测试锁定，22 号约束 6）。
//! 行号换算需要源码文本，归宿主/CLI 完成，VM 只报字节偏移。

use yoru_bytecode::Lbc0Module;

const DEBUG_MAGIC: u32 = 0x59_44_49_31; // "YDI1"

#[derive(Debug, Default, Clone)]
pub struct DebugInfo {
    /// proto_idx → 名字字符串序号（None = 匿名 lambda）
    pub names: Vec<Option<u32>>,
    /// proto_idx → RLE 行偏移表 [(start_pc, src_off)]
    pub lines: Vec<(usize, Vec<(u32, u32)>)>,
}

impl DebugInfo {
    /// 从模块调试段解析。损坏/缺失 → Err/DebugInfo::default()，绝不 panic。
    pub fn from_module(m: &Lbc0Module) -> Result<DebugInfo, String> {
        let bytes = match &m.debug {
            Some(b) => b,
            None => return Ok(DebugInfo::default()),
        };
        let rd_u32 = |pos: usize| -> Result<u32, String> {
            bytes
                .get(pos..pos + 4)
                .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
                .ok_or_else(|| "debug segment truncated".to_string())
        };
        if rd_u32(0)? != DEBUG_MAGIC {
            return Err("bad debug segment magic".to_string());
        }
        let count = rd_u32(4)? as usize;
        let mut info = DebugInfo {
            names: Vec::new(),
            lines: Vec::new(),
        };
        let mut pos = 8;
        for _ in 0..count {
            let pi = rd_u32(pos)? as usize;
            let name = rd_u32(pos + 4)?;
            let run_count = rd_u32(pos + 8)? as usize;
            pos += 12;
            let mut runs = Vec::with_capacity(run_count.min(1 << 16));
            for _ in 0..run_count {
                let start_pc = rd_u32(pos)?;
                let off = rd_u32(pos + 4)?;
                pos += 8;
                runs.push((start_pc, off));
            }
            if pi >= info.names.len() {
                info.names.resize(pi + 1, None);
            }
            info.names[pi] = (name != u32::MAX).then_some(name);
            info.lines.push((pi, runs));
        }
        Ok(info)
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.lines.is_empty()
    }

    /// 按名字找原型序号（入口发现：`main` 或 --entry）。
    pub fn proto_by_name(&self, name: &str, strings: &[String]) -> Option<usize> {
        self.names
            .iter()
            .position(|n| n.map(|i| strings.get(i as usize).map(String::as_str) == Some(name)) == Some(true))
    }

    /// pc → 源码字节偏移（二分 RLE run）。
    pub fn src_off(&self, proto: usize, pc: usize) -> Option<u32> {
        let (_, runs) = self.lines.iter().find(|(p, _)| *p == proto)?;
        let pc = pc as u32;
        let mut best: Option<u32> = None;
        for &(start, off) in runs {
            if start <= pc {
                best = Some(off);
            } else {
                break;
            }
        }
        best
    }
}
