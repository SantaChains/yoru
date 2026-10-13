//! LBC0 模块格式（04 号 §4.5 定稿）：文件头 + 段表 + CRC32。
//!
//! ```text
//! 文件头（12 字节）
//!   magic     : u32 = 0x4C_42_43_30 ("LBC0")
//!   version   : u16 = major(1) << 8 | minor(0)
//!   flags     : u16（bit0=含调试段）
//! 段表（4 段 × 8 字节：offset u32 | len u32）
//!   seg_const / seg_proto / seg_debug / seg_string
//! 校验和：CRC32 u32（文件尾，覆盖文件头之后全部字节）
//! ```
//!
//! **阶段 2 实现细化（v1.0，需回写 04 号）**：
//! - 常量池段头部承载模块级声明表：import 表 / tag 表 / struct 表 / 字段名表，
//!   随后才是常量记录——这些都是 LOADIMPORT/TAGNEW/NEWSTRUCT/GETFIELD 的界检查依据；
//! - 原型段每项含变长表长度前缀（指令数/upvalue 数/常量引用数）——变长结构必须有
//!   显式计数才能无 panic 解码（决策 9 第 4 项的前提）；
//! - 常量引用表照 04 号规格保留（CONST/ISTAG/PAYLOAD/TagNew 操作数的去重清单），
//!   解码侧跳过、验证侧交叉核对。
//!
//! 固定 little-endian（05 号数值语义承诺）。CRC32 为 IEEE 多项式，零依赖实现。

use crate::instr::Instr;

pub const MAGIC: u32 = 0x4C_42_43_30; // "LBC0"
/// 2.0：KV entry 语义变更（决策 82 第二步）——for-in-Map 产出 KV struct
/// 取代键；旧 1.x 产物语义不符，major 拒绝。
pub const VERSION: u16 = 2 << 8; // 2.0

pub const FLAG_DEBUG: u16 = 1 << 0;

const HEADER_LEN: usize = 12;
const SEGTABLE_LEN: usize = 4 * 8;
const CRC_LEN: usize = 4;
const MIN_FILE_LEN: usize = HEADER_LEN + SEGTABLE_LEN + CRC_LEN;

// 常量类型标签
pub const CTAG_INT: u8 = 0;
pub const CTAG_FLOAT: u8 = 1;
pub const CTAG_STR: u8 = 2;
pub const CTAG_PROTO: u8 = 3;
pub const CTAG_VARIANT: u8 = 4;
pub const CTAG_BOOL: u8 = 5;

#[derive(Debug, Clone, PartialEq)]
pub enum Const {
    Int(i64),
    Float(f64),
    /// 字符串表索引
    Str(u32),
    /// 原型索引
    Proto(u32),
    /// 变体标识（tag 序号, 变体序号）——ISTAG/PAYLOAD/TagNew 的操作数
    Variant {
        tag: u32,
        variant: u32,
    },
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpvalueDesc {
    pub is_local: bool,
    pub index: u16,
}

/// 函数原型（04 号 §4.5：参数数/局部槽数/最大栈深/指令流/upvalue 表/常量引用表）。
#[derive(Debug, Clone, PartialEq)]
pub struct Proto {
    pub params: u8,
    pub locals: u16,
    pub max_stack: u16,
    pub code: Vec<Instr>,
    pub upvalues: Vec<UpvalueDesc>,
    /// 本原型引用的常量池序号（去重升序；验证器交叉核对）
    pub const_refs: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TagInfo {
    /// tag 名（字符串表索引）
    pub name: u32,
    /// 每变体的载荷标志（true = 带载荷；决定 TagNew 栈效应与 match 载荷取出）
    pub variants: Vec<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StructInfo {
    /// struct 名（字符串表索引）
    pub name: u32,
    /// 字段名列表（字符串表索引；序号即 NEWSTRUCT 的弹栈对应序）
    pub fields: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Lbc0Module {
    pub version: u16,
    pub flags: u16,
    /// import 槽（字符串表索引；LOADIMPORT 的界）
    pub imports: Vec<u32>,
    /// tag 表（ISTAG/PAYLOAD/TagNew 的 Variant 常量界）
    pub tags: Vec<TagInfo>,
    /// struct 表（NEWSTRUCT 的界）
    pub structs: Vec<StructInfo>,
    /// 全局字段名表（GETFIELD/SETFIELD 的界；阶段 2 = 全局字段名序号）
    pub field_names: Vec<u32>,
    pub consts: Vec<Const>,
    pub protos: Vec<Proto>,
    pub strings: Vec<String>,
    /// 调试段负载（v1.0 恒 None；flags.bit0 由本字段存在性决定）
    pub debug: Option<Vec<u8>>,
}

// ---- CRC32（IEEE 802.3，零依赖）----

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

// ---- 序列化 ----

struct Sink {
    buf: Vec<u8>,
}

impl Sink {
    fn new() -> Sink {
        Sink { buf: Vec::new() }
    }
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_bits().to_le_bytes());
    }
    fn len(&self) -> usize {
        self.buf.len()
    }
}

fn write_const(s: &mut Sink, c: &Const) {
    match c {
        Const::Int(v) => {
            s.u8(CTAG_INT);
            s.i64(*v);
        }
        Const::Float(v) => {
            s.u8(CTAG_FLOAT);
            s.f64(*v);
        }
        Const::Str(i) => {
            s.u8(CTAG_STR);
            s.u32(*i);
        }
        Const::Proto(i) => {
            s.u8(CTAG_PROTO);
            s.u32(*i);
        }
        Const::Variant { tag, variant } => {
            s.u8(CTAG_VARIANT);
            s.u32(*tag);
            s.u32(*variant);
        }
        Const::Bool(b) => {
            s.u8(CTAG_BOOL);
            s.u8(*b as u8);
        }
    }
}

impl Lbc0Module {
    /// 序列化为 LBC0 字节流（固定 little-endian）。
    pub fn serialize(&self) -> Vec<u8> {
        // 段负载
        let mut seg_const = Sink::new();
        seg_const.u32(self.imports.len() as u32);
        for &i in &self.imports {
            seg_const.u32(i);
        }
        seg_const.u32(self.tags.len() as u32);
        for t in &self.tags {
            seg_const.u32(t.name);
            seg_const.u32(t.variants.len() as u32);
            for &p in &t.variants {
                seg_const.u8(p as u8);
            }
        }
        seg_const.u32(self.structs.len() as u32);
        for st in &self.structs {
            seg_const.u32(st.name);
            seg_const.u32(st.fields.len() as u32);
            for &f in &st.fields {
                seg_const.u32(f);
            }
        }
        seg_const.u32(self.field_names.len() as u32);
        for &f in &self.field_names {
            seg_const.u32(f);
        }
        seg_const.u32(self.consts.len() as u32);
        for c in &self.consts {
            write_const(&mut seg_const, c);
        }

        let mut seg_proto = Sink::new();
        seg_proto.u32(self.protos.len() as u32);
        for p in &self.protos {
            seg_proto.u8(p.params);
            seg_proto.u16(p.locals);
            seg_proto.u16(p.max_stack);
            seg_proto.u32(p.code.len() as u32);
            for i in &p.code {
                seg_proto.u32(i.encode());
            }
            seg_proto.u16(p.upvalues.len() as u16);
            for u in &p.upvalues {
                seg_proto.u8(u.is_local as u8);
                seg_proto.u16(u.index);
            }
            seg_proto.u32(p.const_refs.len() as u32);
            for &r in &p.const_refs {
                seg_proto.u32(r);
            }
        }

        let seg_debug = self.debug.clone().unwrap_or_default();
        let has_debug = self.debug.is_some();

        let mut seg_string = Sink::new();
        seg_string.u32(self.strings.len() as u32);
        for s in &self.strings {
            seg_string.u32(s.len() as u32);
            seg_string.buf.extend_from_slice(s.as_bytes());
        }

        // 布局：头 + 段表 + [const][proto][debug][string] + CRC
        let off_const = (HEADER_LEN + SEGTABLE_LEN) as u32;
        let off_proto = off_const + seg_const.len() as u32;
        let off_debug = off_proto + seg_proto.len() as u32;
        let off_string = off_debug + seg_debug.len() as u32;

        let mut out = Sink::new();
        out.u32(MAGIC);
        out.u16(self.version);
        out.u16(if has_debug {
            self.flags | FLAG_DEBUG
        } else {
            self.flags & !FLAG_DEBUG
        });
        out.buf.extend_from_slice(&[0, 0, 0, 0]); // 保留 4 字节（文件头定额 12 字节，04 号 §4.5）
        // 段表
        out.u32(off_const);
        out.u32(seg_const.len() as u32);
        out.u32(off_proto);
        out.u32(seg_proto.len() as u32);
        out.u32(off_debug);
        out.u32(seg_debug.len() as u32);
        out.u32(off_string);
        out.u32(seg_string.len() as u32);
        out.buf.extend_from_slice(&seg_const.buf);
        out.buf.extend_from_slice(&seg_proto.buf);
        out.buf.extend_from_slice(&seg_debug);
        out.buf.extend_from_slice(&seg_string.buf);
        let crc = crc32(&out.buf[HEADER_LEN..]);
        out.u32(crc);
        out.buf
    }

    /// 反序列化。任何截断/魔数不符/版本不符/CRC 不符 = 结构化错误，绝不 panic
    /// （决策 9：validate-before-use；本函数是 fuzz 直靶）。
    pub fn deserialize(bytes: &[u8]) -> Result<Lbc0Module, String> {
        if bytes.len() < MIN_FILE_LEN {
            return Err(format!(
                "file too short: {} bytes (min {MIN_FILE_LEN})",
                bytes.len()
            ));
        }
        let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        if magic != MAGIC {
            return Err(format!("bad magic: 0x{magic:08X} (expect 0x{MAGIC:08X})"));
        }
        let version = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
        if version >> 8 != VERSION >> 8 {
            return Err(format!(
                "incompatible version: 0x{version:04X} (expect major {})",
                VERSION >> 8
            ));
        }
        let flags = u16::from_le_bytes(bytes[6..8].try_into().unwrap());

        // CRC：文件尾 u32，覆盖 [HEADER_LEN, len-CRC_LEN)
        let body = &bytes[HEADER_LEN..bytes.len() - CRC_LEN];
        let stored = u32::from_le_bytes(bytes[bytes.len() - CRC_LEN..].try_into().unwrap());
        if crc32(body) != stored {
            return Err("CRC32 mismatch (file corrupted)".to_string());
        }

        let rd = |off: usize, n: usize| -> Result<&[u8], String> {
            bytes
                .get(off..off + n)
                .ok_or_else(|| format!("truncated at offset {off}"))
        };
        let ru32 = |off: usize| -> Result<u32, String> {
            Ok(u32::from_le_bytes(rd(off, 4)?.try_into().unwrap()))
        };

        // 段表
        let mut seg = [(0u32, 0u32); 4];
        for (i, s) in seg.iter_mut().enumerate() {
            let base = HEADER_LEN + i * 8;
            *s = (ru32(base)?, ru32(base + 4)?);
        }
        for (idx, &(off, len)) in seg.iter().enumerate() {
            let end = off
                .checked_add(len)
                .ok_or_else(|| format!("segment {idx} overflow"))?;
            if end as usize > bytes.len() {
                return Err(format!(
                    "segment {idx} out of bounds: {off}+{len} > {}",
                    bytes.len()
                ));
            }
        }
        let (c_off, c_len) = seg[0];
        let (p_off, p_len) = seg[1];
        let (d_off, d_len) = seg[2];
        let (s_off, s_len) = seg[3];

        // 字符串段
        let mut pos = s_off as usize;
        let s_end = (s_off + s_len) as usize;
        let str_count = ru32(pos)?;
        pos += 4;
        let mut strings = Vec::new();
        for _ in 0..str_count {
            let l = ru32(pos)? as usize;
            pos += 4;
            let raw = bytes
                .get(pos..pos + l)
                .ok_or_else(|| "string segment truncated".to_string())?;
            let s = std::str::from_utf8(raw)
                .map_err(|_| "invalid UTF-8 in string segment".to_string())?;
            strings.push(s.to_string());
            pos += l;
        }
        if pos != s_end {
            return Err("string segment length mismatch".to_string());
        }
        let str_ok = |i: u32| -> Result<(), String> {
            if i as usize >= strings.len() {
                Err(format!(
                    "string index {i} out of bounds ({})",
                    strings.len()
                ))
            } else {
                Ok(())
            }
        };

        // 常量池段（含模块级声明表）
        let c_end = (c_off + c_len) as usize;
        let mut pos = c_off as usize;
        let import_count = ru32(pos)?;
        pos += 4;
        let mut imports = Vec::new();
        for _ in 0..import_count {
            let i = ru32(pos)?;
            pos += 4;
            str_ok(i)?;
            imports.push(i);
        }
        let tag_count = ru32(pos)?;
        pos += 4;
        let mut tags = Vec::new();
        for _ in 0..tag_count {
            let name = ru32(pos)?;
            let variant_count = ru32(pos + 4)? as usize;
            pos += 8;
            str_ok(name)?;
            let mut variants = Vec::with_capacity(variant_count.min(4096));
            for _ in 0..variant_count {
                let flag = *bytes
                    .get(pos)
                    .ok_or_else(|| "tag segment truncated".to_string())?;
                pos += 1;
                variants.push(flag != 0);
            }
            tags.push(TagInfo { name, variants });
        }
        let struct_count = ru32(pos)?;
        pos += 4;
        let mut structs = Vec::new();
        for _ in 0..struct_count {
            let name = ru32(pos)?;
            let field_count = ru32(pos + 4)? as usize;
            pos += 8;
            str_ok(name)?;
            let mut fields = Vec::with_capacity(field_count.min(4096));
            for _ in 0..field_count {
                let f = ru32(pos)?;
                pos += 4;
                str_ok(f)?;
                fields.push(f);
            }
            structs.push(StructInfo { name, fields });
        }
        let field_count = ru32(pos)?;
        pos += 4;
        let mut field_names = Vec::new();
        for _ in 0..field_count {
            let f = ru32(pos)?;
            pos += 4;
            str_ok(f)?;
            field_names.push(f);
        }
        let const_count = ru32(pos)?;
        pos += 4;
        let mut consts = Vec::new();
        for _ in 0..const_count {
            let tag = *bytes
                .get(pos)
                .ok_or_else(|| "const segment truncated".to_string())?;
            pos += 1;
            let c = match tag {
                CTAG_INT => {
                    let v = i64::from_le_bytes(rd(pos, 8)?.try_into().unwrap());
                    pos += 8;
                    Const::Int(v)
                }
                CTAG_FLOAT => {
                    let v = f64::from_le_bytes(rd(pos, 8)?.try_into().unwrap());
                    pos += 8;
                    Const::Float(v)
                }
                CTAG_STR => {
                    let i = ru32(pos)?;
                    pos += 4;
                    str_ok(i)?;
                    Const::Str(i)
                }
                CTAG_PROTO => {
                    let i = ru32(pos)?;
                    pos += 4;
                    Const::Proto(i) // 界检查留给验证器（proto 表尚未读入）
                }
                CTAG_VARIANT => {
                    let t = ru32(pos)?;
                    let v = ru32(pos + 4)?;
                    pos += 8;
                    Const::Variant { tag: t, variant: v } // 界检查留给验证器
                }
                CTAG_BOOL => {
                    let b = *bytes
                        .get(pos)
                        .ok_or_else(|| "const segment truncated".to_string())?;
                    pos += 1;
                    Const::Bool(b != 0)
                }
                other => return Err(format!("unknown const type tag {other}")),
            };
            consts.push(c);
        }
        if pos != c_end {
            return Err("const segment length mismatch".to_string());
        }

        // 原型段
        let p_end = (p_off + p_len) as usize;
        let mut pos = p_off as usize;
        let proto_count = ru32(pos)?;
        pos += 4;
        let mut protos = Vec::new();
        for _ in 0..proto_count {
            let params = *bytes
                .get(pos)
                .ok_or_else(|| "proto segment truncated".to_string())?;
            let locals = u16::from_le_bytes(rd(pos + 1, 2)?.try_into().unwrap());
            let max_stack = u16::from_le_bytes(rd(pos + 3, 2)?.try_into().unwrap());
            pos += 5;
            let instr_count = ru32(pos)? as usize;
            pos += 4;
            if instr_count > (1 << 24) {
                return Err(format!("proto instr_count {instr_count} exceeds limit"));
            }
            let mut code = Vec::with_capacity(instr_count);
            for pc in 0..instr_count {
                let word = ru32(pos)?;
                pos += 4;
                // 严格解码：保留区 opcode = 加载失败（决策 9 第 1 项）
                code.push(Instr::decode(word, pc).map_err(|e| e.to_string())?);
            }
            let uv_count = u16::from_le_bytes(rd(pos, 2)?.try_into().unwrap()) as usize;
            pos += 2;
            let mut upvalues = Vec::new();
            for _ in 0..uv_count {
                let is_local = *bytes
                    .get(pos)
                    .ok_or_else(|| "proto segment truncated".to_string())?
                    != 0;
                let index = u16::from_le_bytes(rd(pos + 1, 2)?.try_into().unwrap());
                pos += 3;
                upvalues.push(UpvalueDesc { is_local, index });
            }
            let ref_count = ru32(pos)? as usize;
            pos += 4;
            let mut const_refs = Vec::new();
            for _ in 0..ref_count {
                let r = ru32(pos)?;
                pos += 4;
                const_refs.push(r);
            }
            protos.push(Proto {
                params,
                locals,
                max_stack,
                code,
                upvalues,
                const_refs,
            });
        }
        if pos != p_end {
            return Err("proto segment length mismatch".to_string());
        }

        let debug = if flags & FLAG_DEBUG != 0 {
            if d_len == 0 {
                return Err("flags declare debug segment but it is empty".to_string());
            }
            Some(bytes[d_off as usize..(d_off + d_len) as usize].to_vec())
        } else {
            None
        };

        Ok(Lbc0Module {
            version,
            flags,
            imports,
            tags,
            structs,
            field_names,
            consts,
            protos,
            strings,
            debug,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn empty_module_roundtrip() {
        let m = Lbc0Module {
            version: VERSION,
            flags: 0,
            imports: vec![0],
            tags: vec![TagInfo {
                name: 1,
                variants: vec![true, false],
            }],
            structs: vec![StructInfo {
                name: 2,
                fields: vec![2, 3],
            }],
            field_names: vec![2, 3],
            consts: vec![
                Const::Int(-7),
                Const::Float(2.5),
                Const::Str(0),
                Const::Variant { tag: 0, variant: 1 },
            ],
            protos: vec![Proto {
                params: 2,
                locals: 4,
                max_stack: 8,
                code: vec![
                    Instr::with_arg(crate::instr::Opcode::Const, 0),
                    Instr::new(crate::instr::Opcode::Return),
                ],
                upvalues: vec![UpvalueDesc {
                    is_local: true,
                    index: 1,
                }],
                const_refs: vec![0],
            }],
            strings: vec!["std".into(), "Err".into(), "x".into(), "y".into()],
            debug: None,
        };
        let bytes = m.serialize();
        // 反序列化后指令流以验证器为准；此处仅验证头部/CRC/声明表往返
        let back = Lbc0Module::deserialize(&bytes).expect("deserialize");
        assert_eq!(back.imports, m.imports);
        assert_eq!(back.tags, m.tags);
        assert_eq!(back.structs, m.structs);
        assert_eq!(back.consts, m.consts);
        assert_eq!(back.protos[0].params, 2);
        assert_eq!(back.protos[0].upvalues, m.protos[0].upvalues);
    }

    #[test]
    fn truncated_input_never_panics() {
        let full = Lbc0Module {
            version: VERSION,
            flags: 0,
            imports: vec![0, 1],
            tags: vec![],
            structs: vec![],
            field_names: vec![],
            consts: vec![Const::Int(1)],
            protos: vec![],
            strings: vec!["a".into(), "b".into()],
            debug: None,
        }
        .serialize();
        for n in 0..full.len() {
            let _ = Lbc0Module::deserialize(&full[..n]); // 只要不 panic 即通过
        }
    }

    #[test]
    fn bit_flip_detected_by_crc() {
        let mut full = Lbc0Module {
            version: VERSION,
            flags: 0,
            imports: vec![0],
            tags: vec![],
            structs: vec![],
            field_names: vec![],
            consts: vec![Const::Int(42)],
            protos: vec![],
            strings: vec!["std".into()],
            debug: None,
        }
        .serialize();
        let last = full.len() - 1;
        full[last - 1] ^= 0xFF; // 翻转 CRC 区前一个字节
        assert!(Lbc0Module::deserialize(&full).is_err());
    }
}
