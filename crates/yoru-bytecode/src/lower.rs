//! AST → LBC0 lowering（阶段 2，20 号路线图）。
//!
//! 职责：把 typeck 通过的 AST 翻译为 LBC0 模块（04 号 §4.4 opcode 表 + 阶段 2 扩展）。
//! 顶层两遍（决策 11）：先收全部声明符号表（import/tag/struct/const/fn），再翻译函数体——
//! AI 生成代码的定义顺序不受惩罚。
//!
//! 关键约定：
//! - 栈布局：调用 = `[args..., callee]`（callee 在顶）/ 方法 = `[args..., receiver]`；
//!   CALL/CALLMETHOD 弹 n+1 压 1（04 号）。
//! - 所有表达式恒产 1 值；语句位结果用 POP 丢弃；赋值求值为 unit（NIL）。
//! - lambda 捕获走 Lua 式 upvalue 链（闭包按值快照 = 决策 31 的执行侧前提）。
//!
//! 阶段 2 边界（诚实声明，回写 12 号）：
//! - 不做类型感知优化（字段序号 = 全局字段名序号；阶段 3 引入 per-struct 序号）；
//! - `try` 双传播用 ISERR/ISNONE 双检实现（语义正确，阶段 3 可按返回类型特化单检）；
//! - 协议 impl 方法进原型表但不接分派（运算符协议分派 = 阶段 3）；
//! - 变体构造/测试不带类型标签（VM 阶段 3 按类型标签校验）。

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use yoru_syntax::ast;
use yoru_syntax::ast::StrItem;
use yoru_syntax::diagnostic::{Diagnostic, Severity, Span};

use crate::instr::{Instr, Opcode};
use crate::module::{Const, Lbc0Module, Proto, StructInfo, TagInfo, UpvalueDesc};
use crate::verify;

/// as 转换的目标类型标签（CONVERT 操作数）。
const CONVERT_INT: u32 = 0;
const CONVERT_FLOAT: u32 = 1;
const CONVERT_RUNE: u32 = 2;

/// 内建自由函数名集合——**从 `yoru-stdlib` 注册表派生（决策 77 单一事实源）**，
/// 构造保证与 typeck/VM 两侧一致。此前为手写 frozen 镜像（注释声称有
/// `registry_mirror_locked` 对账测试，实际不存在）：注册表新增 `get`/`get_or`
/// 后名单未同步，裸名调用撞 Y-T0913——漂移实证后根修为派生式。
/// 调用约定（阶段 3 定稿）：内建以 `Const(Str(名字))` 为 callee——VM 按名字
/// 分派内建注册表，裸名作为值传递（如 `map(xs, len)`）时同为 Str 值，调用期
/// 统一解析。模块限定调用（`string.to_runes`）在 lower 归一为同一 bare 名
/// （见 `module_builtin_ref`）。
static BUILTIN_FNS: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| yoru_stdlib::builtin_names().into_iter().collect());

/// lowering 入口。lowerer 对 AST 的递归与大栈线程配合（同 parse_source 注记：
/// Windows 主线程栈仅 1MB，16MB 大栈 + 深度闸双保险）。
pub fn lower(program: &ast::Program) -> Result<Lbc0Module, Vec<Diagnostic>> {
    lower_with_hosts(program, &[])
}

/// 带宿主自由函数名单的 lowering（阶段 4，12.2 契约）：名单内的未解析名
/// 按内建同构约定发射 `Const(Str(名))` callee，VM 调用期查宿主注册表。
/// 名单应与 typeck `Env::with_host_fns` 注入的签名面一致——不一致时
/// typeck 先拦（编译期 Y-T0001）或 VM fail closed（运行时 Y-H9000）。
pub fn lower_with_hosts(
    program: &ast::Program,
    host_fns: &[String],
) -> Result<Lbc0Module, Vec<Diagnostic>> {
    let hosts: HashSet<String> = host_fns.iter().cloned().collect();
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(s, || lower_inner(program, hosts))
            .expect("spawn lower thread")
            .join()
            .unwrap_or_else(|p| std::panic::resume_unwind(p))
    })
}

fn lower_inner(
    program: &ast::Program,
    host_fns: HashSet<String>,
) -> Result<Lbc0Module, Vec<Diagnostic>> {
    let mut lw = Lowerer::new(host_fns);
    lw.install_prelude();
    lw.declare_decls(program);
    lw.lower_bodies(program);
    lw.finalize();
    if lw.diags.iter().any(|d| d.is_error()) {
        Err(lw.diags)
    } else {
        Ok(lw.m)
    }
}

// ---- 模块级 lowering 状态 ----

struct Lowerer {
    m: Lbc0Module,
    diags: Vec<Diagnostic>,
    str_pool: HashMap<String, u32>,
    field_pool: HashMap<String, u32>,
    /// 可见名 → import 槽
    imports: HashMap<String, u32>,
    /// 可见名 → 模块名（决策 77：模块限定调用归一到 bare 内建名）
    import_modules: HashMap<String, String>,
    /// tag 名 → tag 序号
    tags: HashMap<String, u32>,
    /// 变体名 → (tag 序号, 变体序号)
    variants: HashMap<String, (u32, u32)>,
    /// struct 名 → 描述符序号
    structs: HashMap<String, u32>,
    /// 顶层 const 名 → 常量池序号
    consts_named: HashMap<String, u32>,
    /// 顶层 const 名 → 折叠值（供 const 表达式互相引用）
    const_values: HashMap<String, Const>,
    /// 顶层 fn / impl 方法名 → 原型序号
    fns: HashMap<String, u32>,
    std_slot: u32,
    /// 当前发射指令的源码字节偏移（调试段：pc → 源偏移，行号换算归宿主）
    cur_src: u32,
    /// 原型序号 → 名字字符串序号（u32::MAX = 匿名 lambda；调试段）
    proto_names: HashMap<usize, u32>,
    /// 原型序号 → 每 pc 源偏移表（与 code 平行；调试段）
    line_tabs: HashMap<usize, Vec<u32>>,
    /// 当前函数翻译状态（None = 尚未进入函数体）
    cur: Option<FnState>,
    /// 宿主自由函数名单（阶段 4）：名单内未解析名按内建同构约定发射
    /// `Const(Str(名))` callee（决策 76）。
    host_fns: HashSet<String>,
}

/// 闭包捕获层（Lua 式 upvalue 链）。
#[derive(Clone)]
struct CaptureLayer {
    /// 本层局部名 → 槽
    locals: Vec<(String, u16)>,
    /// 本层已捕获名 → (本层 upvalue 序号, 来自父层的描述)
    upvals: Vec<(String, u16, UpvalueDesc)>,
}

struct FnState {
    proto_idx: usize,
    scopes: Vec<Vec<(String, u16)>>,
    nslots: u16,
    loops: Vec<LoopCtx>,
    /// 捕获层栈：末位 = 当前层
    captures: Vec<CaptureLayer>,
}

struct LoopCtx {
    /// break JMP 占位的 pc 列表
    breaks: Vec<usize>,
    /// continue JMP 占位的 pc 列表
    continues: Vec<usize>,
}

/// 语句是否必然收束控制流（return/throw/break/continue，或全分支收束）。
/// 路径级分析：if 的 false 路径可到达回跳点时不得视作收束——
/// 指令级「最后一条是 Return」启发式在此类代码上产生死循环（T1 实测）。
fn block_always_terminates(b: &ast::Block) -> bool {
    b.stmts.last().is_some_and(stmt_always_terminates)
}

fn stmt_always_terminates(s: &ast::Stmt) -> bool {
    match &s.kind {
        ast::StmtKind::Return(_) | ast::StmtKind::Throw(_) => true,
        // break/continue 无条件跳转，其后代码不可达
        ast::StmtKind::Break | ast::StmtKind::Continue => true,
        ast::StmtKind::Block(b) => block_always_terminates(b),
        ast::StmtKind::Expr(e) => expr_always_terminates(e),
        _ => false,
    }
}

/// 表达式位收束：全分支收束的 if / 带 catch-all 且全臂收束的 match。
/// match 无 catch-all 时保守不收束（穷尽性是 typeck 契约，lower 不信）。
fn expr_always_terminates(e: &ast::Expr) -> bool {
    match &e.kind {
        ast::ExprKind::If {
            then_,
            else_: Some(els),
            ..
        } => block_always_terminates(then_) && expr_always_terminates(els),
        ast::ExprKind::Match { arms, .. } => {
            let catchall = arms
                .iter()
                .any(|a| matches!(a.pattern, ast::Pattern::Wildcard | ast::Pattern::Bind(_)));
            catchall && arms.iter().all(|a| expr_always_terminates(&a.body))
        }
        _ => false,
    }
}

/// 链式赋值目标访问链的一步（D-06/07/08/09）：字段名或索引表达式。
enum LvStep<'a> {
    Field(String),
    Index(&'a ast::Expr),
}

impl Lowerer {
    fn new(host_fns: HashSet<String>) -> Lowerer {
        let mut lw = Lowerer {
            m: Lbc0Module {
                version: crate::module::VERSION,
                flags: 0,
                imports: Vec::new(),
                tags: Vec::new(),
                structs: Vec::new(),
                field_names: Vec::new(),
                consts: Vec::new(),
                protos: Vec::new(),
                strings: Vec::new(),
                debug: None,
            },
            diags: Vec::new(),
            str_pool: HashMap::new(),
            field_pool: HashMap::new(),
            imports: HashMap::new(),
            import_modules: HashMap::new(),
            tags: HashMap::new(),
            variants: HashMap::new(),
            structs: HashMap::new(),
            consts_named: HashMap::new(),
            const_values: HashMap::new(),
            fns: HashMap::new(),
            std_slot: 0,
            cur_src: 0,
            proto_names: HashMap::new(),
            line_tabs: HashMap::new(),
            cur: None,
            host_fns,
        };
        lw.preset_kv();
        lw
    }

    /// 决策 82 第二步：KV entry 内建 struct 预置为 index 0——for-in-Map
    /// 产出 `KV { key, value }` struct，GetField/SetField 槽位表与
    /// NEWSTRUCT 对齐机制天然覆盖，typeck 与 VM 无需专用通道。
    fn preset_kv(&mut self) {
        debug_assert!(self.m.structs.is_empty(), "KV must be struct index 0");
        let sname = self.intern("KV");
        let kf = self.field_id("key");
        let vf = self.field_id("value");
        self.m.structs.push(StructInfo {
            name: sname,
            fields: vec![kf, vf],
        });
        self.structs.insert("KV".to_string(), 0);
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.str_pool.get(s) {
            return i;
        }
        let i = self.m.strings.len() as u32;
        self.m.strings.push(s.to_string());
        self.str_pool.insert(s.to_string(), i);
        i
    }

    fn field_id(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.field_pool.get(name) {
            return i;
        }
        let si = self.intern(name);
        let i = self.m.field_names.len() as u32;
        self.m.field_names.push(si);
        self.field_pool.insert(name.to_string(), i);
        i
    }

    fn add_const(&mut self, c: Const) -> u32 {
        let i = self.m.consts.len() as u32;
        self.m.consts.push(c);
        i
    }

    fn err(&mut self, code: &str, msg: impl Into<String>, span: Span) {
        self.diags
            .push(Diagnostic::coded(code, Severity::Error, msg, span));
    }

    /// prelude：std import 槽 + Result/Option 内建 tag。
    fn install_prelude(&mut self) {
        self.std_slot = self.declare_import("std");
        let result_name = self.intern("Result");
        let ri = self.m.tags.len() as u32;
        self.m.tags.push(TagInfo {
            name: result_name,
            variants: vec![true, true],
        });
        self.tags.insert("Result".into(), ri);
        self.variants.insert("Ok".into(), (ri, 0));
        self.variants.insert("Err".into(), (ri, 1));
        let option_name = self.intern("Option");
        let oi = self.m.tags.len() as u32;
        self.m.tags.push(TagInfo {
            name: option_name,
            variants: vec![false, true],
        });
        self.tags.insert("Option".into(), oi);
        self.variants.insert("None".into(), (oi, 0));
        self.variants.insert("Some".into(), (oi, 1));
    }

    fn declare_import(&mut self, visible: &str) -> u32 {
        if let Some(&s) = self.imports.get(visible) {
            return s;
        }
        let ni = self.intern(visible);
        let slot = self.m.imports.len() as u32;
        self.m.imports.push(ni);
        self.imports.insert(visible.to_string(), slot);
        slot
    }

    // ---- 第一遍：声明收集（顶层两遍，决策 11）----

    fn declare_decls(&mut self, program: &ast::Program) {
        for d in &program.decls {
            match &d.kind {
                ast::DeclKind::Import { name, alias, path } => {
                    let visible = alias.clone().unwrap_or_else(|| name.clone());
                    self.declare_import(&visible);
                    // 决策 77：记录 可见名 → 模块名，模块限定调用（`math.sqrt`）
                    // 据此归一到 bare 内建名（与内建/宿主同构分发约定）。
                    self.import_modules.insert(visible, name.clone());
                    let _ = self.intern(path); // 模块路径留档（T1 仅 std）
                }
                ast::DeclKind::Tag { name, variants, .. } => {
                    self.register_tag(name, variants);
                }
                ast::DeclKind::Type {
                    name,
                    body: ast::TypeBody::Union(vs),
                    ..
                } => {
                    // `type Tree[T] = Node(...) | Leaf` 与 tag 声明同构（19.5 示例形式）
                    self.register_tag(name, vs);
                }
                ast::DeclKind::Struct { name, fields, .. } => {
                    let si = self.m.structs.len() as u32;
                    let finfos: Vec<u32> = fields.iter().map(|f| self.intern(&f.name)).collect();
                    for f in fields {
                        self.field_id(&f.name); // 字段名进全局字段表
                    }
                    let sname = self.intern(name);
                    self.m.structs.push(StructInfo {
                        name: sname,
                        fields: finfos,
                    });
                    self.structs.insert(name.clone(), si);
                }
                ast::DeclKind::Const { name, value, .. } => match self.fold_const(value) {
                    Some(c) => {
                        let idx = self.add_const(c.clone());
                        self.consts_named.insert(name.clone(), idx);
                        self.const_values.insert(name.clone(), c);
                    }
                    None => {
                        self.err(
                            "Y-T0912",
                            "const 仅支持编译期可折叠的数值/布尔/字符串表达式（决策 50）",
                            d.span,
                        );
                    }
                },
                ast::DeclKind::Fn(f) => {
                    let pi = self.m.protos.len() as u32;
                    let name_idx = self.intern(&f.name);
                    self.proto_names.insert(pi as usize, name_idx);
                    self.m.protos.push(Proto {
                        params: f.params.len() as u8,
                        locals: 0,
                        max_stack: 0,
                        code: Vec::new(),
                        upvalues: Vec::new(),
                        const_refs: Vec::new(),
                    });
                    self.fns.insert(f.name.clone(), pi);
                }
                ast::DeclKind::Impl { proto, fns, .. } => {
                    for f in fns {
                        let qname = format!("{proto}.{}", f.name);
                        let pi = self.m.protos.len() as u32;
                        let name_idx = self.intern(&qname);
                        self.proto_names.insert(pi as usize, name_idx);
                        self.m.protos.push(Proto {
                            params: f.params.len() as u8,
                            locals: 0,
                            max_stack: 0,
                            code: Vec::new(),
                            upvalues: Vec::new(),
                            const_refs: Vec::new(),
                        });
                        self.fns.insert(qname, pi);
                    }
                }
                // 别名/distinct/protocol 不产字节码（编译期概念，决策 23）
                ast::DeclKind::Type { .. } | ast::DeclKind::Protocol { .. } => {}
            }
        }
    }

    fn register_tag(&mut self, name: &str, variants: &[ast::Variant]) {
        let ti = self.m.tags.len() as u32;
        let mut flags = Vec::with_capacity(variants.len());
        for (vi, v) in variants.iter().enumerate() {
            if self.variants.contains_key(&v.name) {
                self.err("Y-T0911", format!("variant `{}` 重复声明", v.name), v.span);
            }
            self.variants.insert(v.name.clone(), (ti, vi as u32));
            flags.push(v.payload.is_some());
        }
        let tname = self.intern(name);
        self.m.tags.push(TagInfo {
            name: tname,
            variants: flags,
        });
        self.tags.insert(name.to_string(), ti);
    }

    // ---- 第二遍：函数体翻译 ----

    fn lower_bodies(&mut self, program: &ast::Program) {
        for d in &program.decls {
            match &d.kind {
                ast::DeclKind::Fn(f) => {
                    let pi = self.fns[&f.name] as usize;
                    self.lower_fn_body(pi, &f.params, &f.body);
                }
                ast::DeclKind::Impl { proto, fns, .. } => {
                    for f in fns {
                        let qname = format!("{proto}.{}", f.name);
                        let pi = self.fns[&qname] as usize;
                        self.lower_fn_body(pi, &f.params, &f.body);
                    }
                }
                _ => {}
            }
        }
    }

    fn lower_fn_body(&mut self, proto_idx: usize, params: &[ast::Param], body: &ast::FnBody) {
        self.cur = Some(FnState {
            proto_idx,
            scopes: vec![Vec::new()],
            nslots: 0, // 参数由 declare_local 从 0 号槽分配
            loops: Vec::new(),
            captures: vec![CaptureLayer {
                locals: Vec::new(),
                upvals: Vec::new(),
            }],
        });
        for p in params {
            self.declare_local(&p.name);
        }
        match body {
            ast::FnBody::Block(b) => self.lower_block(b, false),
            ast::FnBody::Expr(e) => {
                self.lower_expr(e);
                self.emit_op(Opcode::Return);
            }
        }
        // 尾部无终结指令 → 补 NIL + RETURN（unit 返回语义）
        if !matches!(
            self.m.protos[proto_idx].code.last(),
            Some(Instr {
                op: Opcode::Return | Opcode::Raise,
                ..
            })
        ) {
            self.emit_op(Opcode::Nil);
            self.emit_op(Opcode::Return);
        }
        // 捕获层 upvalue 描述符落盘
        let ups: Vec<UpvalueDesc> = self
            .cur
            .as_ref()
            .unwrap()
            .captures
            .last()
            .map(|l| l.upvals.iter().map(|(_, _, d)| d.clone()).collect())
            .unwrap_or_default();
        self.m.protos[proto_idx].upvalues = ups;
        self.peephole_nil_pop(proto_idx);
        self.cur = None;
    }

    /// P0a 窥孔 pass：删除「Nil; Pop」相邻对（语句值补位 + 语句级丢弃 = 零功）。
    ///
    /// 发射期窥孔不安全（08_physics 实证）：块尾值 Nil 可能同时是跳转目标
    /// （如 if 无 else 臂补位 = JmpIfNot 的 else 落点），Pop 也可能是回跳
    /// 目标（循环体尾）。此处单 pass 后处理：原始坐标收集跳转表与待删对，
    /// 落在跳转目标上的对保守保留；删除后按「target 与 j_pc 各自前移量」
    /// 重算全部跳转偏移。P0a① 直方图实证（sort 负载）：Nil 与 Pop 各 7.7%。
    fn peephole_nil_pop(&mut self, pi: usize) {
        let is_jump = |op: Opcode| {
            matches!(
                op,
                Opcode::Jmp
                    | Opcode::JmpIf
                    | Opcode::JmpIfNot
                    | Opcode::JmpIfNotLt
                    | Opcode::JmpIfNotLe
                    | Opcode::JmpIfNotGt
                    | Opcode::JmpIfNotGe
            )
        };
        let code = &self.m.protos[pi].code;
        // 原始坐标跳转表：(指令 pc, 有符号偏移)
        let jumps: Vec<(usize, i32)> = code
            .iter()
            .enumerate()
            .filter(|(_, i)| is_jump(i.op))
            .map(|(pc, i)| (pc, i.sarg()))
            .collect();
        let targets: std::collections::HashSet<usize> = jumps
            .iter()
            .map(|(j, s)| (*j as i64 + *s as i64) as usize)
            .collect();
        // 待删对（Nil 的 pc，原始坐标；从后往前找，对之间不重叠）
        let mut deletions: Vec<usize> = Vec::new();
        let mut pc = code.len();
        while pc >= 2 {
            pc -= 1;
            if matches!(code[pc].op, Opcode::Nil)
                && matches!(code[pc + 1].op, Opcode::Pop)
                && !targets.contains(&pc)
                && !targets.contains(&(pc + 1))
            {
                deletions.push(pc);
            }
        }
        if deletions.is_empty() {
            return;
        }
        // 跳过集合：每对的 Nil 与 Pop 两个 pc
        let skip: std::collections::HashSet<usize> =
            deletions.iter().flat_map(|&d| [d, d + 1]).collect();
        // 前移量：位置 p 删除后新坐标 = p - 2 × (删除点 > p 的个数)
        let shift = |p: usize| -> usize { p - 2 * deletions.iter().filter(|&&d| d < p).count() };
        // 重建 code：跳过删除对；跳转偏移按 target 与 j_pc 各自前移量重算
        let old_code = std::mem::take(&mut self.m.protos[pi].code);
        let mut new_code = Vec::with_capacity(old_code.len() - 2 * deletions.len());
        for (jpc, instr) in old_code.iter().enumerate() {
            if skip.contains(&jpc) {
                continue;
            }
            let mut instr = *instr;
            if is_jump(instr.op) {
                let t = jpc as i64 + instr.sarg() as i64;
                let new_t = t - 2 * deletions.iter().filter(|&&d| (d as i64) < t).count() as i64;
                let new_j = shift(jpc) as i64;
                let s = (new_t - new_j) as i32;
                instr.arg = (s as u32) & 0x00FF_FFFF;
            }
            new_code.push(instr);
        }
        self.m.protos[pi].code = new_code;
        let old_tabs = std::mem::take(self.line_tabs.get_mut(&pi).expect("line tab"));
        let new_tabs: Vec<u32> = old_tabs
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !skip.contains(i))
            .map(|(_, t)| t)
            .collect();
        self.line_tabs.insert(pi, new_tabs);
    }

    // ---- 指令发射原语 ----

    fn code_len(&self) -> usize {
        let st = self.cur.as_ref().expect("emit outside fn");
        self.m.protos[st.proto_idx].code.len()
    }

    fn emit(&mut self, i: Instr) -> usize {
        let st = self.cur.as_ref().expect("emit outside fn");
        let pi = st.proto_idx;
        self.m.protos[pi].code.push(i);
        self.line_tabs.entry(pi).or_default().push(self.cur_src);
        self.m.protos[pi].code.len() - 1
    }

    /// 记录后续发射指令的源偏移（调试段；Span = 字节区间，行号换算归宿主）。
    fn note_line(&mut self, span: Span) {
        self.cur_src = span.start as u32;
    }

    fn emit_op(&mut self, op: Opcode) -> usize {
        // 窥孔融合（P0a③「Cmp+JumpIf」超指令）：`Xx; JmpIfNot t` → `JmpIfNotXx t`。
        // 比较语义逐位一致（同一 cmp_values 路径），patch 机制只改 arg 与
        // opcode 无关——目标回填对融合指令照常生效。安全性：比较指令紧跟
        // 其操作数发射，永不为跳转目标，删除不错位。
        if op == Opcode::JmpIfNot {
            let st = self.cur.as_ref().expect("emit outside fn");
            let pi = st.proto_idx;
            let fused = match self.m.protos[pi].code.last().map(|i| i.op) {
                Some(Opcode::Lt) => Some(Opcode::JmpIfNotLt),
                Some(Opcode::Le) => Some(Opcode::JmpIfNotLe),
                Some(Opcode::Gt) => Some(Opcode::JmpIfNotGt),
                Some(Opcode::Ge) => Some(Opcode::JmpIfNotGe),
                _ => None,
            };
            if let Some(fused_op) = fused {
                self.m.protos[pi].code.pop();
                self.line_tabs.get_mut(&pi).expect("line tab").pop();
                return self.emit(Instr::new(fused_op));
            }
        }
        // 窥孔融合（决策 80 族对称）：`LoadLocal b; LoadLocal i; GetIndex`
        // → `GetIndexLL(b<<16|i)`——省 2 条 LoadLocal 与操作数栈往返。
        if op == Opcode::GetIndex {
            let st = self.cur.as_ref().expect("emit outside fn");
            let pi = st.proto_idx;
            let code = &self.m.protos[pi].code;
            let pair = if code.len() >= 2 {
                match (&code[code.len() - 2], &code[code.len() - 1]) {
                    (
                        Instr {
                            op: Opcode::LoadLocal,
                            arg: b,
                        },
                        Instr {
                            op: Opcode::LoadLocal,
                            arg: i,
                        },
                    ) => Some((*b, *i)),
                    _ => None,
                }
            } else {
                None
            };
            if let Some((b, i)) = pair {
                let arg = (b << 16) | (i & 0xFFFF);
                self.m.protos[pi].code.pop();
                self.m.protos[pi].code.pop();
                let tabs = self.line_tabs.get_mut(&pi).expect("line tab");
                tabs.pop();
                tabs.pop();
                return self.emit_arg(Opcode::GetIndexLL, arg);
            }
        }
        self.emit(Instr::new(op))
    }

    fn emit_arg(&mut self, op: Opcode, arg: u32) -> usize {
        self.emit(Instr::with_arg(op, arg))
    }

    fn patch_jump(&mut self, pc: usize, target: usize) {
        let st = self.cur.as_ref().expect("patch outside fn");
        let pi = st.proto_idx;
        let off = (target as i64 - pc as i64) as i32;
        debug_assert!(
            (-0x80_0000..=0x7F_FFFF).contains(&off),
            "jump offset exceeds i24"
        );
        self.m.protos[pi].code[pc].arg = (off as u32) & 0x00FF_FFFF;
    }

    // ---- 局部槽与捕获 ----

    fn declare_local(&mut self, name: &str) -> u16 {
        let st = self.cur.as_mut().expect("declare outside fn");
        let slot = st.nslots;
        st.nslots += 1;
        st.scopes.last_mut().unwrap().push((name.to_string(), slot));
        let pi = st.proto_idx;
        self.m.protos[pi].locals = st.nslots;
        if let Some(layer) = st.captures.last_mut() {
            layer.locals.push((name.to_string(), slot));
        }
        slot
    }

    fn push_scope(&mut self) {
        self.cur
            .as_mut()
            .expect("scope outside fn")
            .scopes
            .push(Vec::new());
    }

    fn pop_scope(&mut self) {
        self.cur.as_mut().expect("scope outside fn").scopes.pop();
    }

    fn lookup_local(&self, name: &str) -> Option<u16> {
        let st = self.cur.as_ref()?;
        for scope in st.scopes.iter().rev() {
            for (n, slot) in scope.iter().rev() {
                if n == name {
                    return Some(*slot);
                }
            }
        }
        None
    }

    /// 递归捕获解析（Lua upvalue 链）。返回「layer_idx 层视角」的 UpvalueDesc：
    /// is_local=true → 该层局部槽；is_local=false → 该层 upvalue 序号。
    /// layer 0 = 顶层 fn，其局部同样可被内层 lambda 捕获。
    fn resolve_capture(&mut self, layer_idx: usize, name: &str) -> Option<UpvalueDesc> {
        {
            let st = self.cur.as_ref().unwrap();
            let layer = &st.captures[layer_idx];
            if let Some((_, slot)) = layer.locals.iter().rev().find(|(n, _)| n == name) {
                return Some(UpvalueDesc {
                    is_local: true,
                    index: *slot,
                });
            }
            if let Some((_, uv, _)) = layer.upvals.iter().rev().find(|(n, _, _)| n == name) {
                return Some(UpvalueDesc {
                    is_local: false,
                    index: *uv,
                });
            }
        }
        if layer_idx == 0 {
            return None; // 顶层 fn 之上是模块级（imports/consts/fns），不走捕获
        }
        let parent_desc = self.resolve_capture(layer_idx - 1, name)?;
        let st = self.cur.as_mut().unwrap();
        let layer = &mut st.captures[layer_idx];
        let idx = layer.upvals.len() as u16;
        layer.upvals.push((name.to_string(), idx, parent_desc));
        Some(UpvalueDesc {
            is_local: false,
            index: idx,
        })
    }

    // ---- 语句 ----

    fn lower_block(&mut self, b: &ast::Block, value: bool) {
        self.push_scope();
        let n = b.stmts.len();
        let mut produced = false;
        for (i, s) in b.stmts.iter().enumerate() {
            self.lower_stmt(s, value && i + 1 == n, &mut produced);
        }
        if value && !produced {
            self.emit_op(Opcode::Nil);
        }
        self.pop_scope();
    }

    /// `produced`：本块是否已压入值（块尾值语义用）。
    fn lower_stmt(&mut self, s: &ast::Stmt, _value_pos: bool, produced: &mut bool) {
        self.note_line(s.span);
        match &s.kind {
            ast::StmtKind::Let { name, value: e, .. } => {
                self.lower_expr(e);
                let slot = self.declare_local(name);
                self.emit_arg(Opcode::StoreLocal, slot as u32);
                *produced = false;
            }
            ast::StmtKind::Expr(e) => {
                // if 无 else 在语句位 = void（不产值不 POP）
                let is_void_if = matches!(&e.kind, ast::ExprKind::If { else_: None, .. });
                if is_void_if {
                    if let ast::ExprKind::If { cond, then_, .. } = &e.kind {
                        self.lower_if(cond, then_, None, false, e.span);
                    }
                    *produced = false;
                } else if _value_pos {
                    // 块尾表达式 = 块值（D-17：if 表达式/块值语义，19.2）——留在栈上
                    self.lower_expr(e);
                    *produced = true;
                } else {
                    self.lower_expr(e);
                    self.emit_op(Opcode::Pop);
                    *produced = false;
                }
            }
            ast::StmtKind::While { cond, body } => {
                self.lower_loop(cond, body);
                *produced = false;
            }
            ast::StmtKind::For { var, iter, body } => {
                self.lower_for(var, iter, body);
                *produced = false;
            }
            ast::StmtKind::Return(e) => {
                match e {
                    Some(e) => self.lower_expr(e),
                    None => {
                        self.emit_op(Opcode::Nil);
                    }
                }
                self.emit_op(Opcode::Return);
                *produced = false;
            }
            ast::StmtKind::Throw(e) => {
                self.lower_expr(e);
                self.emit_op(Opcode::Raise);
                *produced = false;
            }
            ast::StmtKind::Break => {
                let pc = self.emit_op(Opcode::Jmp);
                if let Some(lp) = self.cur.as_mut().and_then(|st| st.loops.last_mut()) {
                    lp.breaks.push(pc);
                }
                *produced = false;
            }
            ast::StmtKind::Continue => {
                let pc = self.emit_op(Opcode::Jmp);
                if let Some(lp) = self.cur.as_mut().and_then(|st| st.loops.last_mut()) {
                    lp.continues.push(pc);
                }
                *produced = false;
            }
            ast::StmtKind::Block(b) => {
                self.lower_block(b, false);
                *produced = false;
            }
        }
    }

    fn lower_loop(&mut self, cond: &ast::Expr, body: &ast::Block) {
        let head = self.code_len();
        self.lower_expr(cond); // 压条件
        let jnot = self.emit_op(Opcode::JmpIfNot);
        self.cur.as_mut().unwrap().loops.push(LoopCtx {
            breaks: Vec::new(),
            continues: Vec::new(),
        });
        self.push_scope();
        let mut produced = false;
        for s in &body.stmts {
            self.lower_stmt(s, false, &mut produced);
        }
        self.pop_scope();
        // 体全路径收束时跳过回跳 JMP（路径级分析）
        if !block_always_terminates(body) {
            let back = self.emit_op(Opcode::Jmp); // 回 head
            self.patch_jump(back, head);
        }
        let exit = self.code_len();
        self.patch_jump(jnot, exit);
        let st = self.cur.as_mut().unwrap();
        if let Some(lp) = st.loops.pop() {
            for pc in lp.breaks {
                self.patch_jump(pc, exit);
            }
            for pc in lp.continues {
                self.patch_jump(pc, head);
            }
        }
    }

    fn lower_for(&mut self, var: &str, iter: &ast::Expr, body: &ast::Block) {
        self.lower_expr(iter); // 压 iterable
        self.push_scope();
        let slot = self.declare_local(var);
        self.emit_arg(Opcode::ForPrep, slot as u32); // 弹 iterable 压游标
        let head = self.emit_arg(Opcode::ForNext, slot as u32); // 游标留栈，压 bool
        let jnot = self.emit_op(Opcode::JmpIfNot);
        self.cur.as_mut().unwrap().loops.push(LoopCtx {
            breaks: Vec::new(),
            continues: Vec::new(),
        });
        self.push_scope();
        let mut produced = false;
        for s in &body.stmts {
            self.lower_stmt(s, false, &mut produced);
        }
        self.pop_scope();
        // 体全路径收束时跳过回跳 JMP（路径级分析）
        if !block_always_terminates(body) {
            let back = self.emit_op(Opcode::Jmp);
            self.patch_jump(back, head);
        }
        let exit = self.code_len();
        self.patch_jump(jnot, exit);
        self.emit_op(Opcode::Pop); // 弹游标
        let st = self.cur.as_mut().unwrap();
        if let Some(lp) = st.loops.pop() {
            for pc in lp.breaks {
                self.patch_jump(pc, exit);
            }
            for pc in lp.continues {
                self.patch_jump(pc, head);
            }
        }
        self.pop_scope();
    }

    // ---- 表达式（恒产 1 值；lower_if 的 void 模式除外）----

    fn lower_expr(&mut self, e: &ast::Expr) {
        self.note_line(e.span);
        match &e.kind {
            ast::ExprKind::Int(v) => {
                let c = self.add_const(Const::Int(*v));
                self.emit_arg(Opcode::Const, c);
            }
            ast::ExprKind::Float(v) => {
                let c = self.add_const(Const::Float(*v));
                self.emit_arg(Opcode::Const, c);
            }
            ast::ExprKind::Bool(b) => {
                self.emit_op(if *b { Opcode::True } else { Opcode::False });
            }
            ast::ExprKind::Nil => {
                self.emit_op(Opcode::Nil);
            }
            ast::ExprKind::Str(parts) => self.lower_str(parts),
            ast::ExprKind::Ident(name) => self.lower_ident(name, e.span),
            ast::ExprKind::Lambda { params, body } => self.lower_lambda(params, body),
            ast::ExprKind::If { cond, then_, else_ } => {
                self.lower_if(cond, then_, else_.as_deref(), true, e.span);
            }
            ast::ExprKind::Match { scrutinee, arms } => self.lower_match(scrutinee, arms, e.span),
            ast::ExprKind::Try(inner) => {
                // 双传播（决策 54）：Err → RETURN，nil → RETURN，否则解包
                self.lower_expr(inner);
                let t = self.declare_local("$try");
                self.emit_arg(Opcode::StoreLocal, t as u32);
                self.emit_arg(Opcode::LoadLocal, t as u32);
                self.emit_op(Opcode::IsErr);
                let l1 = self.emit_op(Opcode::JmpIfNot);
                self.emit_arg(Opcode::LoadLocal, t as u32);
                self.emit_op(Opcode::Return);
                self.patch_jump(l1, self.code_len());
                self.emit_arg(Opcode::LoadLocal, t as u32);
                self.emit_op(Opcode::IsNone);
                let l2 = self.emit_op(Opcode::JmpIfNot);
                self.emit_arg(Opcode::LoadLocal, t as u32);
                self.emit_op(Opcode::Return);
                self.patch_jump(l2, self.code_len());
                self.emit_arg(Opcode::LoadLocal, t as u32);
                self.emit_op(Opcode::Unwrap);
            }
            ast::ExprKind::Await(inner) => {
                self.lower_expr(inner);
                self.emit_op(Opcode::Await);
            }
            ast::ExprKind::Unary { op, operand } => {
                self.lower_expr(operand);
                self.emit_op(match op {
                    ast::UnOp::Neg => Opcode::Neg,
                    ast::UnOp::Not => Opcode::Not,
                });
            }
            ast::ExprKind::Binary { op, lhs, rhs } => self.lower_binary(op, lhs, rhs),
            ast::ExprKind::Call { callee, args } => self.lower_call(callee, args),
            ast::ExprKind::Field { base, field } => {
                // 模块限定内建名作值传递（`map(xs, string.to_runes)`，决策 77）
                if let Some(name) = self.module_builtin_ref(base, field) {
                    self.emit_builtin_name(name);
                } else {
                    self.lower_expr(base);
                    let fid = self.field_id(field);
                    self.emit_arg(Opcode::GetField, fid);
                }
            }
            ast::ExprKind::Index { base, index } => {
                self.lower_expr(base);
                match index {
                    ast::IndexKind::Single(i) => {
                        self.lower_expr(i);
                        self.emit_op(Opcode::GetIndex);
                    }
                    ast::IndexKind::Range(a, b) => {
                        self.lower_expr(a);
                        self.lower_expr(b);
                        self.emit_op(Opcode::Slice);
                    }
                }
            }
            ast::ExprKind::Unwrap(inner) => {
                self.lower_expr(inner);
                self.emit_op(Opcode::Unwrap);
            }
            ast::ExprKind::Cast { operand, ty } => {
                self.lower_expr(operand);
                // distinct/同名 as = 编译期概念（零指令）；数值/码点转换发 CONVERT
                let conv = match ty {
                    ast::Type::Int => Some(CONVERT_INT),
                    ast::Type::Float => Some(CONVERT_FLOAT),
                    ast::Type::Rune => Some(CONVERT_RUNE),
                    _ => None,
                };
                if let Some(t) = conv {
                    self.emit_arg(Opcode::Convert, t);
                }
            }
            ast::ExprKind::StructLit {
                name,
                inits,
                spread,
                ..
            } => self.lower_struct_lit(name, inits, spread.as_deref(), e.span),
            ast::ExprKind::List(items) => {
                for it in items {
                    self.lower_expr(it);
                }
                self.emit_arg(Opcode::NewList, items.len() as u32);
            }
            ast::ExprKind::Map(pairs) => {
                for (k, v) in pairs {
                    self.lower_expr(k);
                    self.lower_expr(v);
                }
                self.emit_arg(Opcode::NewMap, pairs.len() as u32);
            }
            ast::ExprKind::Block(b) => {
                self.lower_block(b, true);
            }
            ast::ExprKind::Assign { target, op, value } => {
                self.lower_assign(target, *op, value);
            }
        }
    }

    fn lower_str(&mut self, parts: &[StrItem]) {
        if let [StrItem::Lit(s)] = parts {
            let si = self.intern(s);
            let c = self.add_const(Const::Str(si));
            self.emit_arg(Opcode::Const, c);
            return;
        }
        let mut first = true;
        for p in parts {
            match p {
                StrItem::Lit(s) => {
                    let si = self.intern(s);
                    let c = self.add_const(Const::Str(si));
                    self.emit_arg(Opcode::Const, c);
                }
                StrItem::Interp(e) => {
                    self.lower_expr(e);
                    self.emit_op(Opcode::ToString);
                }
            }
            if !first {
                self.emit_op(Opcode::Add); // 插值 = 拼串唯一形式（19.1）
            }
            first = false;
        }
    }

    fn lower_ident(&mut self, name: &str, span: Span) {
        // 1. 局部
        if let Some(slot) = self.lookup_local(name) {
            self.emit_arg(Opcode::LoadLocal, slot as u32);
            return;
        }
        // 2. 捕获链（lambda 内）
        let is_lambda = self
            .cur
            .as_ref()
            .map(|st| st.captures.len() > 1)
            .unwrap_or(false);
        if is_lambda {
            let top = self.cur.as_ref().unwrap().captures.len() - 1;
            if let Some(d) = self.resolve_capture(top, name) {
                self.emit_arg(Opcode::LoadUpvalue, d.index as u32);
                return;
            }
        }
        // 3. 顶层 const
        if let Some(&ci) = self.consts_named.get(name) {
            self.emit_arg(Opcode::Const, ci);
            return;
        }
        // 4. 裸变体（无载荷变体值构造）
        if let Some(&(tag, variant)) = self.variants.get(name) {
            let ci = self.add_const(Const::Variant { tag, variant });
            self.emit_arg(Opcode::TagNew, ci);
            return;
        }
        // 5. 顶层 fn
        if let Some(&pi) = self.fns.get(name) {
            self.emit_arg(Opcode::NewClosure, pi);
            return;
        }
        // 6. import 模块名
        if let Some(&slot) = self.imports.get(name) {
            self.emit_arg(Opcode::LoadImport, slot);
            return;
        }
        // 7. 内建自由函数（决策 25）：以 `Const(Str(名字))` 为 callee——VM 调用期按名
        //    分派内建注册表；裸名作值传递（`map(xs, len)`）亦为同一 Str 值，语义一致。
        if BUILTIN_FNS.contains(name) {
            self.emit_builtin_name(name);
            return;
        }
        // 8. 宿主自由函数（阶段 4，决策 76）：与内建同构的 callee 约定——
        //    `Const(Str(名))` 压栈 + Call 指令，VM 调用期内建未命中后查宿主
        //    注册表；签名契约单一闸口在 typeck（CallHost 0x64 留给直接编码路径）。
        if self.host_fns.contains(name) {
            let si = self.intern(name);
            let ci = self.add_const(Const::Str(si));
            self.emit_arg(Opcode::Const, ci);
            return;
        }
        self.err(
            "Y-T0913",
            format!("未解析的名字 `{name}`（typeck 应先行拦截）"),
            span,
        );
        self.emit_op(Opcode::Nil); // 保持栈效应可验证
    }

    fn lower_lambda(&mut self, params: &[ast::LamParam], body: &ast::FnBody) {
        let pi = self.m.protos.len();
        self.m.protos.push(Proto {
            params: params.len() as u8,
            locals: 0,
            max_stack: 0,
            code: Vec::new(),
            upvalues: Vec::new(),
            const_refs: Vec::new(),
        });
        self.emit_arg(Opcode::NewClosure, pi as u32);

        // 保存外层状态，切入 lambda。captures 必须 **move**（D-24 根修）：
        // 内层 lambda 的祖父捕获经 resolve_capture 递归把「传递 upvalue 条目」
        // 登记进中间层 upvals——若克隆 captures，条目会写进克隆体并在恢复
        // outer 时丢失（中间层 proto.upvalues 落空 → VM 取值越界 Y-I9004）。
        let mut outer = self.cur.take().expect("lambda outside fn");
        let mut captures = std::mem::take(&mut outer.captures);
        captures.push(CaptureLayer {
            locals: Vec::new(),
            upvals: Vec::new(),
        });
        let st = FnState {
            proto_idx: pi,
            scopes: vec![Vec::new()],
            nslots: 0, // 参数由 declare_local 从 0 号槽分配
            loops: Vec::new(),
            captures,
        };
        self.cur = Some(st);
        for p in params {
            self.declare_local(&p.name);
        }
        match body {
            ast::FnBody::Block(b) => self.lower_block(b, false),
            ast::FnBody::Expr(e) => {
                self.lower_expr(e);
                self.emit_op(Opcode::Return);
            }
        }
        if !matches!(
            self.m.protos[pi].code.last(),
            Some(Instr {
                op: Opcode::Return | Opcode::Raise,
                ..
            })
        ) {
            self.emit_op(Opcode::Nil);
            self.emit_op(Opcode::Return);
        }
        // 本层 upvalue 描述符落盘；随后摘掉本层、把 captures（含内层 lambda
        // 在中间层新登记的传递条目）放回外层（D-24 根修的另一半）。
        let mut st = self.cur.take().expect("lambda state");
        let ups: Vec<UpvalueDesc> = st
            .captures
            .last()
            .map(|l| l.upvals.iter().map(|(_, _, d)| d.clone()).collect())
            .unwrap_or_default();
        self.m.protos[pi].upvalues = ups;
        st.captures.pop();
        outer.captures = st.captures;
        self.cur = Some(outer);
    }

    fn lower_if(
        &mut self,
        cond: &ast::Expr,
        then_: &ast::Block,
        else_: Option<&ast::Expr>,
        value_mode: bool,
        span: Span,
    ) {
        self.lower_expr(cond);
        let jnot = self.emit_op(Opcode::JmpIfNot);
        self.lower_block(then_, value_mode);
        // then 全路径收束时跳过 jend（canonical：无死码；路径级分析）
        let jend = if block_always_terminates(then_) {
            None
        } else {
            Some(self.emit_op(Opcode::Jmp))
        };
        let else_pc = self.code_len();
        self.patch_jump(jnot, else_pc);
        match else_ {
            Some(e) => match &e.kind {
                ast::ExprKind::If { .. } if !value_mode => {
                    if let ast::ExprKind::If {
                        cond, then_, else_, ..
                    } = &e.kind
                    {
                        self.lower_if(cond, then_, else_.as_deref(), false, e.span);
                    }
                }
                ast::ExprKind::If { .. } => self.lower_expr(e),
                ast::ExprKind::Block(b) => self.lower_block(b, value_mode),
                _ => {
                    self.lower_expr(e);
                    if !value_mode {
                        self.emit_op(Opcode::Pop);
                    }
                }
            },
            None => {
                if value_mode {
                    self.err("Y-T0914", "if 作值必须带 else（19.2）", span);
                    self.emit_op(Opcode::Nil); // 保持栈效应可验证
                }
            }
        }
        let end = self.code_len();
        if let Some(pc) = jend {
            self.patch_jump(pc, end);
        }
    }

    fn lower_match(&mut self, scrutinee: &ast::Expr, arms: &[ast::MatchArm], _span: Span) {
        self.lower_expr(scrutinee);
        let t = self.declare_local("$match");
        self.emit_arg(Opcode::StoreLocal, t as u32);
        let mut ends: Vec<usize> = Vec::new();
        let mut handled_catchall = false;
        for arm in arms {
            if handled_catchall {
                self.err(
                    "Y-T0915",
                    "catch-all 模式之后的分支不可达（canonical form：catch-all 必须最后）",
                    arm.span,
                );
                break;
            }
            match &arm.pattern {
                // RW-e 消歧：裸 identifier 先查变体表——命中即无载荷变体模式，
                // 否则才是 catch-all 绑定（名字解析先查变体，19 号 §19.9e）
                ast::Pattern::Bind(name) if name != "_" && self.variants.contains_key(name) => {
                    let ci = {
                        let &(tag, variant) = self.variants.get(name).unwrap();
                        self.add_const(Const::Variant { tag, variant })
                    };
                    self.emit_arg(Opcode::LoadLocal, t as u32);
                    self.emit_arg(Opcode::IsTag, ci);
                    let jnext = self.emit_op(Opcode::JmpIfNot);
                    self.emit_arg(Opcode::LoadLocal, t as u32);
                    self.emit_arg(Opcode::Payload, ci);
                    self.emit_op(Opcode::Pop); // 无载荷变体：PAYLOAD 压 nil 后丢弃
                    self.lower_expr(&arm.body);
                    ends.push(self.emit_op(Opcode::Jmp));
                    let na = self.code_len();
                    self.patch_jump(jnext, na);
                }
                ast::Pattern::Wildcard | ast::Pattern::Bind(_) => {
                    if let ast::Pattern::Bind(bn) = &arm.pattern {
                        if bn != "_" {
                            self.emit_arg(Opcode::LoadLocal, t as u32);
                            let slot = self.declare_local(bn);
                            self.emit_arg(Opcode::StoreLocal, slot as u32);
                        }
                    }
                    self.lower_expr(&arm.body);
                    ends.push(self.emit_op(Opcode::Jmp));
                    handled_catchall = true;
                }
                ast::Pattern::Variant { name, payload } => {
                    let ci = match self.variants.get(name) {
                        Some(&(tag, variant)) => self.add_const(Const::Variant { tag, variant }),
                        None => {
                            self.err("Y-T0916", format!("未知变体 `{name}`"), arm.span);
                            self.emit_op(Opcode::Nil);
                            ends.push(self.emit_op(Opcode::Jmp));
                            continue;
                        }
                    };
                    self.emit_arg(Opcode::LoadLocal, t as u32);
                    self.emit_arg(Opcode::IsTag, ci);
                    let jnext = self.emit_op(Opcode::JmpIfNot);
                    self.emit_arg(Opcode::LoadLocal, t as u32);
                    self.emit_arg(Opcode::Payload, ci);
                    match payload {
                        Some(p) => {
                            let slot = self.declare_local(p);
                            self.emit_arg(Opcode::StoreLocal, slot as u32);
                        }
                        None => {
                            self.emit_op(Opcode::Pop); // PAYLOAD 对无载荷变体压 nil
                        }
                    }
                    self.lower_expr(&arm.body);
                    ends.push(self.emit_op(Opcode::Jmp));
                    let na = self.code_len();
                    self.patch_jump(jnext, na);
                }
                ast::Pattern::Nil => {
                    self.emit_arg(Opcode::LoadLocal, t as u32);
                    self.emit_op(Opcode::IsNone);
                    let jnext = self.emit_op(Opcode::JmpIfNot);
                    self.lower_expr(&arm.body);
                    ends.push(self.emit_op(Opcode::Jmp));
                    let na = self.code_len();
                    self.patch_jump(jnext, na);
                }
                ast::Pattern::Int(v) => self.lower_literal_arm(t, &Const::Int(*v), arm, &mut ends),
                ast::Pattern::Float(v) => {
                    self.lower_literal_arm(t, &Const::Float(*v), arm, &mut ends)
                }
                ast::Pattern::Bool(b) => {
                    self.emit_arg(Opcode::LoadLocal, t as u32);
                    self.emit_op(if *b { Opcode::True } else { Opcode::False });
                    self.emit_op(Opcode::Eq);
                    let jnext = self.emit_op(Opcode::JmpIfNot);
                    self.lower_expr(&arm.body);
                    ends.push(self.emit_op(Opcode::Jmp));
                    let na = self.code_len();
                    self.patch_jump(jnext, na);
                }
                ast::Pattern::Str(parts) => {
                    if parts.iter().any(|p| matches!(p, StrItem::Interp(_))) {
                        self.err("Y-T0917", "match 模式字符串不允许插值", arm.span);
                    }
                    let lit: String = parts
                        .iter()
                        .map(|p| match p {
                            StrItem::Lit(s) => s.clone(),
                            StrItem::Interp(_) => String::new(),
                        })
                        .collect();
                    self.emit_arg(Opcode::LoadLocal, t as u32);
                    let si = self.intern(&lit);
                    let c = self.add_const(Const::Str(si));
                    self.emit_arg(Opcode::Const, c);
                    self.emit_op(Opcode::Eq);
                    let jnext = self.emit_op(Opcode::JmpIfNot);
                    self.lower_expr(&arm.body);
                    ends.push(self.emit_op(Opcode::Jmp));
                    let na = self.code_len();
                    self.patch_jump(jnext, na);
                }
            }
        }
        if !handled_catchall {
            // 穷尽性由 typeck 保证（决策 20：match 穷尽处理）——运行期不可达兜底：
            // 静态可达的 Nil + RAISE 陷阱，保持栈效应闭合且验证器可证。
            self.emit_op(Opcode::Nil);
            self.emit_op(Opcode::Raise);
        }
        let end = self.code_len();
        for pc in ends {
            self.patch_jump(pc, end);
        }
    }

    fn lower_literal_arm(&mut self, t: u16, c: &Const, arm: &ast::MatchArm, ends: &mut Vec<usize>) {
        self.emit_arg(Opcode::LoadLocal, t as u32);
        let ci = self.add_const(c.clone());
        self.emit_arg(Opcode::Const, ci);
        self.emit_op(Opcode::Eq);
        let jnext = self.emit_op(Opcode::JmpIfNot);
        self.lower_expr(&arm.body);
        ends.push(self.emit_op(Opcode::Jmp));
        let na = self.code_len();
        self.patch_jump(jnext, na);
    }

    fn lower_binary(&mut self, op: &ast::BinOp, lhs: &ast::Expr, rhs: &ast::Expr) {
        use ast::BinOp::*;
        match op {
            Or => {
                self.lower_expr(lhs);
                let jtrue = self.emit_op(Opcode::JmpIf);
                self.lower_expr(rhs);
                let jend = self.emit_op(Opcode::Jmp);
                let ltrue = self.code_len();
                self.patch_jump(jtrue, ltrue);
                self.emit_op(Opcode::True);
                let end = self.code_len();
                self.patch_jump(jend, end);
            }
            And => {
                self.lower_expr(lhs);
                let jfalse = self.emit_op(Opcode::JmpIfNot);
                self.lower_expr(rhs);
                let jend = self.emit_op(Opcode::Jmp);
                let lfalse = self.code_len();
                self.patch_jump(jfalse, lfalse);
                self.emit_op(Opcode::False);
                let end = self.code_len();
                self.patch_jump(jend, end);
            }
            Pipe => {
                // x |> f(y) = f(x, y)（决策 25 左线程）：lhs 是第一个实参，
                // 实参总数 = args.len() + 1（阶段 3 VM 上线抓出的发射缺陷——
                // 验证器只查栈深不查调用语义，此类 bug 只有真执行才暴露）。
                if let ast::ExprKind::Call { callee, args } = &rhs.kind {
                    self.lower_expr(lhs);
                    for a in args {
                        self.lower_expr(a);
                    }
                    self.lower_callee(callee, args.len() + 1);
                } else {
                    self.lower_expr(lhs);
                    self.lower_expr(rhs);
                    self.emit_arg(Opcode::Call, 1);
                }
            }
            Range => {
                self.lower_expr(lhs);
                self.lower_expr(rhs);
                self.emit_op(Opcode::NewRange);
            }
            _ => {
                self.lower_expr(lhs);
                self.lower_expr(rhs);
                let opc = match op {
                    Add => Opcode::Add,
                    Sub => Opcode::Sub,
                    Mul => Opcode::Mul,
                    Div => Opcode::Div,
                    Mod => Opcode::Mod,
                    BitAnd => Opcode::Band,
                    BitOr => Opcode::Bor,
                    BitXor => Opcode::Bxor,
                    Shl => Opcode::Shl,
                    Shr => Opcode::Shr,
                    Eq => Opcode::Eq,
                    Ne => Opcode::Ne,
                    Lt => Opcode::Lt,
                    Le => Opcode::Le,
                    Gt => Opcode::Gt,
                    Ge => Opcode::Ge,
                    _ => unreachable!("pipe/range/short-circuit handled above"),
                };
                self.emit_op(opc);
            }
        }
    }

    /// callee 发射：`base.field(args)` → receiver 存临时槽，args 入栈后取字段得闭包
    /// 再 CALL（修复：阶段 2 曾发 CALLMETHOD 且丢失方法名，运行期无法分派——
    /// 值语义下字段即闭包，GETFIELD+CALL 完全正确；CALLMETHOD 保留给阶段 4 协议分派）。
    /// **模块限定调用（决策 77）**：`string.to_runes(s)` 若 base 是 import 模块名且
    /// `模块.字段` 命中注册表 → 归一为 bare 内建名 `Const(Str) + Call`（与内建/宿主
    /// 同构）——修复此前 LoadImport(Nil)+GetField 运行期断链。
    /// 其余 → callee + CALL。约定栈布局 `[args..., callee/receiver]`。
    fn lower_callee(&mut self, callee: &ast::Expr, nargs: usize) {
        if let ast::ExprKind::Field { base, field } = &callee.kind {
            if let Some(name) = self.module_builtin_ref(base, field) {
                self.emit_builtin_name(name);
                self.emit_arg(Opcode::Call, nargs as u32);
                return;
            }
            self.lower_expr(base);
            let t = self.declare_local("$recv");
            self.emit_arg(Opcode::StoreLocal, t as u32);
            self.emit_arg(Opcode::LoadLocal, t as u32);
            let fid = self.field_id(field);
            self.emit_arg(Opcode::GetField, fid);
            self.emit_arg(Opcode::Call, nargs as u32);
        } else {
            self.lower_expr(callee);
            self.emit_arg(Opcode::Call, nargs as u32);
        }
    }

    /// `import模块.字段` 是否命中内建注册表（决策 77）。局部绑定优先遮蔽
    /// （与 Ident 解析同序）；宿主 import 面不在此列——走 LoadImport+GetField。
    fn module_builtin_ref(&self, base: &ast::Expr, field: &str) -> Option<&'static str> {
        if let ast::ExprKind::Ident(n) = &base.kind {
            if self.lookup_local(n).is_some() {
                return None;
            }
            if let Some(module) = self.import_modules.get(n) {
                return yoru_stdlib::module_fn(module, field).map(|b| b.name);
            }
        }
        None
    }

    /// 发射 bare 内建名常量（与 Ident 路径的 Const(Str) callee 约定一致）。
    fn emit_builtin_name(&mut self, name: &str) {
        let si = self.intern(name);
        let ci = self.add_const(Const::Str(si));
        self.emit_arg(Opcode::Const, ci);
    }

    fn lower_call(&mut self, callee: &ast::Expr, args: &[ast::Expr]) {
        // 变体构造：Err(e) / Ok(v) / BadInput(s) / TooDeep（裸）
        if let ast::ExprKind::Ident(name) = &callee.kind {
            if let Some(&(tag, variant)) = self.variants.get(name) {
                let ci = self.add_const(Const::Variant { tag, variant });
                let has_payload = self.m.tags[tag as usize].variants[variant as usize];
                if has_payload {
                    if args.len() != 1 {
                        self.err(
                            "Y-T0919",
                            format!("变体 `{name}` 恰好接受 1 个载荷参数"),
                            callee.span,
                        );
                    }
                    if let Some(a) = args.first() {
                        self.lower_expr(a);
                    } else {
                        self.emit_op(Opcode::Nil);
                    }
                } else if !args.is_empty() {
                    self.err(
                        "Y-T0919",
                        format!("变体 `{name}` 无载荷，不接受参数"),
                        callee.span,
                    );
                }
                self.emit_arg(Opcode::TagNew, ci);
                return;
            }
        }
        for a in args {
            self.lower_expr(a);
        }
        self.lower_callee(callee, args.len());
    }

    fn lower_struct_lit(
        &mut self,
        name: &str,
        inits: &[ast::FieldInit],
        spread: Option<&ast::Expr>,
        _span: Span,
    ) {
        let si = match self.structs.get(name) {
            Some(&s) => s,
            None => {
                // 未声明 struct（宿主/镜像类型，如 demo 环境的 Item）：
                // 阶段 2 按字面量字段合成临时描述符，阶段 3 接 typeck 类型信息后收紧
                let si = self.m.structs.len() as u32;
                let finfos: Vec<u32> = inits.iter().map(|f| self.intern(&f.name)).collect();
                for f in inits {
                    self.field_id(&f.name);
                }
                let sname = self.intern(name);
                self.m.structs.push(StructInfo {
                    name: sname,
                    fields: finfos,
                });
                self.structs.insert(name.to_string(), si);
                si
            }
        };
        match spread {
            None => {
                for init in inits {
                    if !self.struct_field_exists(si, &init.name) {
                        self.err(
                            "Y-T0921",
                            format!("struct `{name}` 没有字段 `{}`", init.name),
                            init.span,
                        );
                    }
                    self.lower_expr(&init.value);
                }
                self.emit_arg(Opcode::NewStruct, si);
                // 栈布局：[f0, f1, ..., fn]，NEWSTRUCT 弹 n 值压 struct
            }
            Some(base) => {
                // T { ..base, f: v }（决策 41）：压 base 后逐字段 SETFIELD（原地变更，决策 13）
                self.lower_expr(base);
                for init in inits {
                    if !self.struct_field_exists(si, &init.name) {
                        self.err(
                            "Y-T0921",
                            format!("struct `{name}` 没有字段 `{}`", init.name),
                            init.span,
                        );
                    }
                    self.lower_expr(&init.value);
                    let fid = self.field_id(&init.name);
                    self.emit_arg(Opcode::SetField, fid);
                }
                // 值在栈上（返回更新后的 struct）
            }
        }
    }

    fn struct_field_exists(&self, si: u32, field: &str) -> bool {
        self.m.structs.get(si as usize).map(|s| {
            s.fields
                .iter()
                .any(|&f| self.m.strings.get(f as usize).map(String::as_str) == Some(field))
        }) == Some(true)
    }

    /// 赋值。所有路径恒产 1 值（unit = NIL）。
    fn lower_assign(&mut self, target: &ast::Expr, op: ast::AssignOp, value: &ast::Expr) {
        use ast::AssignOp::*;
        let bin = match op {
            Assign => None,
            AddAssign => Some(Opcode::Add),
            SubAssign => Some(Opcode::Sub),
        };
        match &target.kind {
            ast::ExprKind::Ident(name) => {
                let slot = match self.lookup_local(name) {
                    Some(s) => s,
                    None => {
                        self.err(
                            "Y-T0922",
                            format!("赋值目标 `{name}` 不是局部绑定（值语义：外层绑定不可变）"),
                            target.span,
                        );
                        self.lower_expr(value);
                        self.emit_op(Opcode::Pop);
                        self.emit_op(Opcode::Nil);
                        return;
                    }
                };
                match bin {
                    None => self.lower_expr(value),
                    Some(b) => {
                        self.emit_arg(Opcode::LoadLocal, slot as u32);
                        self.lower_expr(value);
                        self.emit_op(b);
                    }
                }
                self.emit_arg(Opcode::StoreLocal, slot as u32);
                self.emit_op(Opcode::Nil);
            }
            ast::ExprKind::Field { .. } => {
                // D-06/D-07：统一链式回写——容器入临时槽，变异后写回根槽位。
                // 旧实现 Pop 丢弃栈上容器副本（未回写）→ 简单赋值静默 no-op、
                // 复合赋值操作数错位 Y-R9012。
                if let Some((root, steps)) = self.lvalue_chain(target) {
                    if let Some(slot) = self.lookup_local(&root) {
                        self.lower_chain_assign(slot, &steps, bin, value);
                        self.emit_op(Opcode::Nil);
                        return;
                    }
                }
                // 根非局部（import 等）：typeck 已拦（值语义：外层绑定不可变），保栈平衡
                self.lower_lvalue_prefix(target);
                self.lower_expr(value);
                self.emit_op(Opcode::Pop);
                self.emit_op(Opcode::Nil);
            }
            ast::ExprKind::Index { base, index } => match index {
                ast::IndexKind::Single(idx) => {
                    // 快路径：base 是局部变量 → SetIndexLocal 对槽位原地变异。
                    // 走通用 SetIndex 时栈上容器副本与局部槽双重引用，COW 互斥
                    // 判定（决策 13）恒不成立 → 每次赋值整表拷贝（T2b 基准实测）。
                    if bin.is_none() {
                        if let ast::ExprKind::Ident(name) = &base.kind {
                            if let Some(slot) = self.lookup_local(name) {
                                self.lower_expr(idx);
                                self.lower_expr(value);
                                self.emit_arg(Opcode::SetIndexLocal, slot as u32);
                                self.emit_op(Opcode::Nil);
                                return;
                            }
                        }
                    }
                    // D-08/D-09：统一链式回写——下降逐层取容器（索引入临时防重复求值
                    // 副作用）→ 内层变异 → 上升逐层 SetIndex → 根槽位回写。
                    // 旧实现复合路径两临时互换（Y-R9013）、链式路径 Pop 丢副本（no-op）。
                    if let Some((root, steps)) = self.lvalue_chain(target) {
                        if let Some(slot) = self.lookup_local(&root) {
                            self.lower_chain_assign(slot, &steps, bin, value);
                            self.emit_op(Opcode::Nil);
                            return;
                        }
                    }
                    self.lower_lvalue_prefix(base);
                    self.lower_expr(idx);
                    self.lower_expr(value);
                    self.emit_op(Opcode::Pop);
                    self.emit_op(Opcode::Nil);
                }
                ast::IndexKind::Range(..) => {
                    self.err("Y-T0923", "切片不支持赋值", target.span);
                    self.lower_expr(value);
                    self.emit_op(Opcode::Pop);
                    self.emit_op(Opcode::Nil);
                }
            },
            _ => {
                self.err("Y-T0922", "非法赋值目标", target.span);
                self.lower_expr(value);
                self.emit_op(Opcode::Pop);
                self.emit_op(Opcode::Nil);
            }
        }
    }

    /// lvalue 前缀（除最后一段外的访问链）：压最终容器。
    /// 收集赋值目标的访问链：`root[a].b[c]` → (root, [Index(a), Field(b), Index(c)])。
    /// 根必须是纯 Ident；链上只允许 Field/Index；Range 切片不可作目标（None）。
    fn lvalue_chain<'a>(&self, target: &'a ast::Expr) -> Option<(String, Vec<LvStep<'a>>)> {
        let mut steps = Vec::new();
        let mut cur = target;
        loop {
            match &cur.kind {
                ast::ExprKind::Field { base, field } => {
                    steps.push(LvStep::Field(field.clone()));
                    cur = base;
                }
                ast::ExprKind::Index { base, index } => match index {
                    ast::IndexKind::Single(idx) => {
                        steps.push(LvStep::Index(idx));
                        cur = base;
                    }
                    ast::IndexKind::Range(..) => return None,
                },
                ast::ExprKind::Ident(name) => {
                    steps.reverse();
                    return Some((name.clone(), steps));
                }
                _ => return None,
            }
        }
    }

    /// 统一链式赋值（D-06/07/08/09，26 号登记）：
    /// `root[i0].f[i1] (=|+=) value` →
    ///   ① c0 = 根副本；
    ///   ② 下降：c_{k+1} = c_k[step_k]（索引入临时，上升阶段免重复求值副作用）；
    ///   ③ 内层变异（复合 = 读改写，值经临时重排保证 SetField/SetIndex 操作数序）；
    ///   ④ 上升：c_k = SetField/SetIndex(c_k, step_k, c_{k+1})；
    ///   ⑤ 根槽位回写。
    /// 值语义由「副本入临时 + 写回槽位」保证（决策 13 COW；决策 80 同族）。
    fn lower_chain_assign<'a>(
        &mut self,
        slot: u16,
        steps: &[LvStep<'a>],
        bin: Option<Opcode>,
        value: &ast::Expr,
    ) {
        // ① c0 = 根容器副本
        self.emit_arg(Opcode::LoadLocal, slot as u32);
        let c0 = self.declare_local("$c0");
        self.emit_arg(Opcode::StoreLocal, c0 as u32);
        // ② 下降
        let mut cs = vec![c0];
        let mut its: Vec<Option<u16>> = Vec::new(); // its[k] = 第 k 层 Index 索引临时
        for k in 0..steps.len() - 1 {
            self.emit_arg(Opcode::LoadLocal, cs[k] as u32);
            match &steps[k] {
                LvStep::Field(name) => {
                    let fid = self.field_id(name);
                    self.emit_arg(Opcode::GetField, fid);
                    its.push(None);
                }
                LvStep::Index(idx) => {
                    self.lower_expr(idx);
                    let t = self.declare_local("$ci");
                    self.emit_arg(Opcode::StoreLocal, t as u32);
                    its.push(Some(t));
                    self.emit_arg(Opcode::LoadLocal, t as u32);
                    self.emit_op(Opcode::GetIndex);
                }
            }
            let cn = self.declare_local("$cd");
            self.emit_arg(Opcode::StoreLocal, cn as u32);
            cs.push(cn);
        }
        // ③ 内层变异
        let last = steps.len() - 1;
        let cv = self.declare_local("$cv");
        self.emit_arg(Opcode::LoadLocal, cs[last] as u32);
        match &steps[last] {
            LvStep::Field(name) => {
                let fid = self.field_id(name);
                match bin {
                    None => self.lower_expr(value),
                    Some(b) => {
                        self.emit_arg(Opcode::GetField, fid);
                        self.lower_expr(value);
                        self.emit_op(b);
                        // SetField 需 [容器, 值]：结果经临时重排
                        let tv = self.declare_local("$tv");
                        self.emit_arg(Opcode::StoreLocal, tv as u32);
                        self.emit_arg(Opcode::LoadLocal, cs[last] as u32);
                        self.emit_arg(Opcode::LoadLocal, tv as u32);
                    }
                }
                self.emit_arg(Opcode::SetField, fid);
            }
            LvStep::Index(idx) => {
                match bin {
                    None => {
                        self.lower_expr(idx);
                        self.lower_expr(value);
                    }
                    Some(b) => {
                        self.lower_expr(idx);
                        let ti = self.declare_local("$ti");
                        self.emit_arg(Opcode::StoreLocal, ti as u32);
                        self.emit_arg(Opcode::LoadLocal, ti as u32);
                        self.emit_op(Opcode::GetIndex);
                        self.lower_expr(value);
                        self.emit_op(b);
                        // SetIndex 需 [容器, 索引, 值]：结果经临时重排
                        let tv = self.declare_local("$tv");
                        self.emit_arg(Opcode::StoreLocal, tv as u32);
                        self.emit_arg(Opcode::LoadLocal, cs[last] as u32);
                        self.emit_arg(Opcode::LoadLocal, ti as u32);
                        self.emit_arg(Opcode::LoadLocal, tv as u32);
                    }
                }
                self.emit_arg(Opcode::SetIndex, 0);
            }
        }
        self.emit_arg(Opcode::StoreLocal, cv as u32);
        // ④ 上升
        let mut cur = cv;
        for k in (0..steps.len() - 1).rev() {
            self.emit_arg(Opcode::LoadLocal, cs[k] as u32);
            match &steps[k] {
                LvStep::Field(name) => {
                    let fid = self.field_id(name);
                    self.emit_arg(Opcode::LoadLocal, cur as u32);
                    self.emit_arg(Opcode::SetField, fid);
                }
                LvStep::Index(_) => {
                    if let Some(t) = its[k] {
                        self.emit_arg(Opcode::LoadLocal, t as u32);
                    }
                    self.emit_arg(Opcode::LoadLocal, cur as u32);
                    self.emit_arg(Opcode::SetIndex, 0);
                }
            }
            let ck = cs[k];
            self.emit_arg(Opcode::StoreLocal, ck as u32);
            cur = ck;
        }
        // ⑤ 根槽位回写
        self.emit_arg(Opcode::LoadLocal, cur as u32);
        self.emit_arg(Opcode::StoreLocal, slot as u32);
    }

    fn lower_lvalue_prefix(&mut self, e: &ast::Expr) {
        match &e.kind {
            ast::ExprKind::Ident(name) => {
                if let Some(slot) = self.lookup_local(name) {
                    self.emit_arg(Opcode::LoadLocal, slot as u32);
                } else if let Some(&slot) = self.imports.get(name) {
                    self.emit_arg(Opcode::LoadImport, slot);
                } else {
                    self.err(
                        "Y-T0922",
                        format!("赋值目标基础 `{name}` 不是局部绑定"),
                        e.span,
                    );
                    self.emit_op(Opcode::Nil);
                }
            }
            ast::ExprKind::Field { base, field } => {
                self.lower_lvalue_prefix(base);
                let fid = self.field_id(field);
                self.emit_arg(Opcode::GetField, fid);
            }
            ast::ExprKind::Index { base, index } => {
                self.lower_lvalue_prefix(base);
                if let ast::IndexKind::Single(i) = index {
                    self.lower_expr(i);
                }
                self.emit_op(Opcode::GetIndex);
            }
            _ => {
                self.err("Y-T0922", "非法赋值目标基础", e.span);
                self.emit_op(Opcode::Nil);
            }
        }
    }

    // ---- 收尾 ----

    fn finalize(&mut self) {
        // 1. 每原型栈深静态重算 → max_stack（与验证器同一算法，保证声明==计算）
        let n = self.m.protos.len();
        for pi in 0..n {
            let computed = verify::compute_max_stack(&self.m, pi).unwrap_or(0);
            self.m.protos[pi].max_stack = computed as u16;
        }
        // 2. 常量引用表（决策 9 第 4 项：原型级引用清单）
        for pi in 0..n {
            let mut refs: Vec<u32> = self.m.protos[pi]
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
            refs.sort_unstable();
            refs.dedup();
            self.m.protos[pi].const_refs = refs;
        }
        // 3. 调试段（决策 70）：原型名表 + 每 pc 源偏移（RLE）。
        //    debug 段缺席（strip 产物）语义不变——差分测试锁定（22 号约束 6）。
        self.build_debug();
    }

    /// 调试段布局 v1（"YDI1"）：
    /// `u32 magic | u32 count | { u32 proto_idx | u32 name_stridx(MAX=匿名)
    ///  | u32 run_count | { u32 start_pc | u32 src_off }*run_count }*count`
    fn build_debug(&mut self) {
        const DEBUG_MAGIC: u32 = 0x59_44_49_31; // "YDI1"
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&DEBUG_MAGIC.to_le_bytes());
        let mut entries: Vec<usize> = self.line_tabs.keys().copied().collect();
        entries.sort_unstable();
        buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for pi in entries {
            let offs = &self.line_tabs[&pi];
            buf.extend_from_slice(&(pi as u32).to_le_bytes());
            let name = self.proto_names.get(&pi).copied().unwrap_or(u32::MAX);
            buf.extend_from_slice(&name.to_le_bytes());
            // RLE：连续同偏移合并为 (start_pc, src_off) run
            let mut runs: Vec<(u32, u32)> = Vec::new();
            for (pc, &off) in offs.iter().enumerate() {
                match runs.last_mut() {
                    Some((_, o)) if *o == off => {}
                    _ => runs.push((pc as u32, off)),
                }
            }
            buf.extend_from_slice(&(runs.len() as u32).to_le_bytes());
            for (start_pc, off) in runs {
                buf.extend_from_slice(&start_pc.to_le_bytes());
                buf.extend_from_slice(&off.to_le_bytes());
            }
        }
        self.m.debug = Some(buf);
    }

    fn fold_const(&mut self, e: &ast::Expr) -> Option<Const> {
        match &e.kind {
            ast::ExprKind::Int(v) => Some(Const::Int(*v)),
            ast::ExprKind::Float(v) => Some(Const::Float(*v)),
            ast::ExprKind::Bool(b) => Some(Const::Bool(*b)),
            ast::ExprKind::Str(parts) => {
                if let [StrItem::Lit(s)] = parts.as_slice() {
                    Some(Const::Str(self.intern(s)))
                } else {
                    None
                }
            }
            ast::ExprKind::Unary { op, operand } => match (op, self.fold_const(operand)) {
                (ast::UnOp::Neg, Some(Const::Int(v))) => v.checked_neg().map(Const::Int),
                (ast::UnOp::Neg, Some(Const::Float(v))) => Some(Const::Float(-v)),
                _ => None,
            },
            ast::ExprKind::Binary { op, lhs, rhs } => {
                let l = self.fold_const(lhs)?;
                let r = self.fold_const(rhs)?;
                match (l, r) {
                    (Const::Int(a), Const::Int(b)) => match op {
                        ast::BinOp::Add => a.checked_add(b).map(Const::Int),
                        ast::BinOp::Sub => a.checked_sub(b).map(Const::Int),
                        ast::BinOp::Mul => a.checked_mul(b).map(Const::Int),
                        // 决策 44 单一事实源在 VM：Div/Mod 必须 euclid（floor）
                        // 语义，与运行时 checked_div_euclid/checked_rem_euclid
                        // 逐位一致——const 折叠透明性（S 线回归测试锁定）。
                        ast::BinOp::Div if b != 0 => a.checked_div_euclid(b).map(Const::Int),
                        ast::BinOp::Mod if b != 0 => a.checked_rem_euclid(b).map(Const::Int),
                        _ => None,
                    },
                    (Const::Float(a), Const::Float(b)) => match op {
                        ast::BinOp::Add => Some(Const::Float(a + b)),
                        ast::BinOp::Sub => Some(Const::Float(a - b)),
                        ast::BinOp::Mul => Some(Const::Float(a * b)),
                        ast::BinOp::Div => Some(Const::Float(a / b)),
                        _ => None,
                    },
                    (Const::Str(a), Const::Str(b)) if matches!(op, ast::BinOp::Add) => {
                        let mut s = self.m.strings[a as usize].clone();
                        s.push_str(&self.m.strings[b as usize]);
                        Some(Const::Str(self.intern(&s)))
                    }
                    _ => None,
                }
            }
            ast::ExprKind::Ident(name) => self.const_values.get(name).cloned(),
            _ => None,
        }
    }
}
