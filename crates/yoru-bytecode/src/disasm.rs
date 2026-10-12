//! 反汇编器：LBC0 模块 → 人类可读文本。
//!
//! 用途：round-trip 测试（阶段 2 退出标准）、诊断输出、阶段 3 VM 调试。

use crate::instr::{Instr, Opcode};
use crate::module::{Const, Lbc0Module};

fn const_label(m: &Lbc0Module, idx: u32) -> String {
    match m.consts.get(idx as usize) {
        Some(Const::Int(v)) => format!("int {v}"),
        Some(Const::Float(v)) => format!("float {v}"),
        Some(Const::Bool(b)) => format!("bool {b}"),
        Some(Const::Str(i)) => format!(
            "str {:?}",
            m.strings
                .get(*i as usize)
                .map(String::as_str)
                .unwrap_or("<oob>")
        ),
        Some(Const::Proto(i)) => format!("proto #{i}"),
        Some(Const::Variant { tag, variant }) => format!("variant tag#{tag}#{variant}"),
        None => "<oob>".to_string(),
    }
}

fn instr_text(m: &Lbc0Module, pc: usize, i: Instr) -> String {
    let name = format!("{:?}", i.op);
    match i.op.operand_kind() {
        crate::instr::OperandKind::None => name,
        crate::instr::OperandKind::U24 => match i.op {
            Opcode::Const | Opcode::IsTag | Opcode::Payload | Opcode::TagNew => {
                format!("{:<12} {} ; {}", name, i.arg, const_label(m, i.arg))
            }
            Opcode::JmpIfNot | Opcode::JmpIf | Opcode::Jmp => {
                let target = pc as i64 + i.sarg() as i64;
                format!("{:<12} -> pc {target}", name.trim_end())
            }
            _ => format!("{name:<12} {}", i.arg),
        },
        crate::instr::OperandKind::S24 => {
            let target = pc as i64 + i.sarg() as i64;
            format!("{name:<12} -> pc {target}")
        }
    }
}

/// 反汇编整个模块（含 import/tag/struct/字段名声明表头）。
pub fn disassemble(m: &Lbc0Module) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "LBC0 version {}.{} imports={} tags={} structs={} consts={} protos={} strings={}\n",
        m.version >> 8,
        m.version & 0xFF,
        m.imports.len(),
        m.tags.len(),
        m.structs.len(),
        m.consts.len(),
        m.protos.len(),
        m.strings.len()
    ));
    for (i, &imp) in m.imports.iter().enumerate() {
        let name = m
            .strings
            .get(imp as usize)
            .map(String::as_str)
            .unwrap_or("<oob>");
        out.push_str(&format!("  import[{i}] = {name}\n"));
    }
    for (ti, t) in m.tags.iter().enumerate() {
        let name = m
            .strings
            .get(t.name as usize)
            .map(String::as_str)
            .unwrap_or("<oob>");
        let variants: Vec<String> = t
            .variants
            .iter()
            .enumerate()
            .map(|(vi, &p)| format!("{}{vi}", if p { "+" } else { "-" }))
            .collect();
        out.push_str(&format!("  tag[{ti}] {name} = {}\n", variants.join(" | ")));
    }
    for (si, s) in m.structs.iter().enumerate() {
        let name = m
            .strings
            .get(s.name as usize)
            .map(String::as_str)
            .unwrap_or("<oob>");
        out.push_str(&format!(
            "  struct[{si}] {name} ({} fields)\n",
            s.fields.len()
        ));
    }
    out.push('\n');
    for (pi, p) in m.protos.iter().enumerate() {
        out.push_str(&format!(
            "proto #{pi}: params={} locals={} max_stack={} upvalues={}\n",
            p.params,
            p.locals,
            p.max_stack,
            p.upvalues.len()
        ));
        for (pc, i) in p.code.iter().enumerate() {
            out.push_str(&format!("  {pc:04}  {}\n", instr_text(m, pc, *i)));
        }
        if pi + 1 < m.protos.len() {
            out.push('\n');
        }
    }
    out
}
