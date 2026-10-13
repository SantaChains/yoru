//! yoru-vm：T1 栈式字节码解释器（阶段 3，20 号路线图；22 号六条硬约束）。
//!
//! 约束落实：
//! 1. 栈式 + giant match（决策 68；GDScript/QuickJS 同构验证）；
//! 2. `Vm`（堆+资源）× `Instance`（模块+Ctx）两层（决策 69），堆按实例隔离；
//! 3. 资源三限独立旋钮 + 中断计数器节流（决策 71，QuickJS interrupt_counter 印证）；
//! 4. debug 段消费：运行时错误携带源码字节偏移（决策 70；行号换算归宿主）；
//! 5. `stats()` 观测 API 与三限同源（决策 72）；
//! 6. debug/strip 产物差分一致（测试锁定，约束 6）。
//!
//! 语义对齐：与 `yoru-bytecode::lower` 的发射约定逐条对应——
//! 栈布局 `[args..., callee]`、COW 值语义（refcount==1 原地变更，决策 13）、
//! floor 除法/取模（决策 44）、溢出抛错（决策 16）、Map 缺键取 nil（决策 55）、
//! 内建以 `Const(Str(名))` 分派注册表（阶段 3 调用约定）。

// 同 yoru-bytecode/yoru-typeck 裁决：VM 的公开输入即 `yoru_bytecode` 的
// `Lbc0Module`（`Instance::new`/`Vm::load`/`DebugInfo`）——运行字节码的
// 消费者按构造依赖字节码 crate，workspace 内 semver 同步无错配面。
#![allow(exported_private_dependencies)]

pub mod builtins;
pub mod debuginfo;
pub mod demo;
pub mod error;
pub mod value;

// 嵌入面（embedder surface）根级 re-export：宿主注入三件套
// （`Value` 参数/返回、`VmError` 错误、`HostFn` 类型别名）+ VM 公开错误类型
// 在 crate 根直接可见，嵌入方无需记子模块路径（外部消费者构建实证的摩擦点）。
pub use error::{LimitKind, RuntimeError, VmError};
pub use value::{ClosureObj, Cursor, HostFn, MapBox, StructObj, Value, VariantVal};

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use yoru_bytecode::{Const, Instr, Lbc0Module, Opcode};

use crate::debuginfo::DebugInfo;

/// 资源三限 + 节流参数（决策 71：三旋钮独立，运行时可调）。
#[derive(Debug, Clone)]
pub struct Limits {
    /// 指令预算（u64；默认 1 亿）
    pub max_instructions: u64,
    /// Yoru 调用深度上限（同时是 Rust 递归安全界；默认 256）
    pub max_call_depth: usize,
    /// 堆字节近似限额（决策 72 的量级估算；run 作用域累计，决策 79；默认 64 MiB）
    pub max_heap_bytes: i64,
    /// 中断检查节流：每 N 条指令检查一次钩子/时钟（QuickJS = 10000）
    pub check_interval: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_instructions: 100_000_000,
            max_call_depth: 256,
            max_heap_bytes: 64 * 1024 * 1024,
            check_interval: 10_000,
        }
    }
}

/// 观测快照（决策 72：宿主轮询，不做回调风暴式 profiling）。
///
/// 作用域（决策 79）：`instructions`/`calls`/`max_call_depth`/`heap_bytes`
/// 为 **run 作用域**（`load` 时重置，三限的预算基础——长驻宿主同一 Vm
/// 反复 run 不被历史消耗饿死）；`total_*` 为 **Vm 生命周期单调累计**
/// （不参与限额，供宿主观测整个生命周期的总消耗）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub instructions: u64,
    pub calls: u64,
    pub max_call_depth: usize,
    /// 堆字节近似值（创建计账，不追踪释放——量级观测用途；run 作用域）
    pub heap_bytes: i64,
    pub output_bytes: usize,
    /// Vm 生命周期累计指令数（单调，不随 run 重置）
    pub total_instructions: u64,
    /// Vm 生命周期累计分配字节（单调，不随 run 重置）
    pub total_allocated: i64,
}

/// 调用栈帧。
struct Frame {
    closure: Rc<ClosureObj>,
    pc: usize,
    locals: Vec<Value>,
    /// Return 时栈截断基线（调用点弹参后的深度）
    stack_base: usize,
}

/// 模块实例（决策 69）：模块 + import 槽值 + 宿主函数表。多实例互不可见。
#[derive(Clone)]
pub struct Instance {
    module: Rc<Lbc0Module>,
    /// import 槽 → 宿主提供的值（模块对象等）
    imports: Vec<Value>,
    /// Result/Option tag 序号（按 tag 名解析；prelude 恒在）
    result_tag: Option<u32>,
    /// 字符串序号 → 全局字段名序号（GETFIELD/SETFIELD/NEWSTRUCT 对齐）
    fid_of_str: HashMap<u32, u32>,
}

impl Instance {
    pub fn new(module: Lbc0Module) -> Instance {
        let module = Rc::new(module);
        let result_tag = Self::find_tag(&module, "Result");
        let mut fid_of_str = HashMap::new();
        for (i, &sidx) in module.field_names.iter().enumerate() {
            fid_of_str.insert(sidx, i as u32);
        }
        let imports = vec![Value::Nil; module.imports.len()];
        Instance {
            module,
            imports,
            result_tag,
            fid_of_str,
        }
    }

    fn find_tag(m: &Lbc0Module, name: &str) -> Option<u32> {
        m.tags
            .iter()
            .position(|t| m.strings.get(t.name as usize).map(String::as_str) == Some(name))
            .map(|i| i as u32)
    }

    /// 按 import 可见名绑定槽值（模块对象 / Ctx 宿主对象等）。
    pub fn set_import(&mut self, name: &str, value: Value) -> Result<(), String> {
        let slot = self
            .module
            .imports
            .iter()
            .position(|&s| self.module.strings.get(s as usize).map(String::as_str) == Some(name))
            .ok_or_else(|| format!("module has no import `{name}`"))?;
        self.imports[slot] = value;
        Ok(())
    }

    pub fn module(&self) -> &Lbc0Module {
        &self.module
    }

    /// strip 变体（决策 70 双产物）：移除调试段——仅删元数据不改语义（差分测试锁定）。
    pub fn strip_debug(&mut self) {
        let mut m = (*self.module).clone();
        m.debug = None;
        m.flags &= !yoru_bytecode::module::FLAG_DEBUG;
        self.module = Rc::new(m);
    }
}

/// 中断钩子：返回 true = 立即终止（不可捕获）。
pub type InterruptFn = Box<dyn FnMut(&Stats) -> bool>;

/// T1 栈式虚拟机（决策 69：堆与资源归 Vm；跨实例顺序复用，互不可见）。
pub struct Vm {
    limits: Limits,
    stats: Stats,
    // ---- 当前执行的实例上下文（run 时注入）----
    module: Option<Rc<Lbc0Module>>,
    imports: Vec<Value>,
    /// 宿主自由函数注册表（阶段 4，12.2 契约）：register_host_fn 注入，
    /// 内建未命中后按名分派（决策 76：与内建同构的调用约定）。
    host_fns: HashMap<String, HostFn>,
    result_tag: Option<u32>,
    fid_of_str: HashMap<u32, u32>,
    debug: DebugInfo,
    // ---- 执行状态 ----
    frames: Vec<Frame>,
    stack: Vec<Value>,
    heap_bytes: i64,
    /// 输出缓冲（print 内建写入；宿主经 take_output 取走）
    output: Vec<u8>,
    interrupt: Option<InterruptFn>,
    deadline: Option<Instant>,
    /// opcode 直方图（P0a① profile 释疑）：None = 零开销（热循环单分支）。
    /// `enable_op_histogram` 显式开启，run 结束经 `op_histogram()` 读取。
    op_histogram: Option<Box<[u64]>>,
    /// 中断节流倒数（2026-10-13 数学验证）：到 0 触发检查并重置为
    /// `check_interval`。等价取代热路径 `%` 取模——运行时变量除法每指令
    /// ~25 cycles（x64 div，非二次幂无法强度削减），dec+cmp ~1 cycle。
    /// 触发点逐位不变（第 k·check_interval 条）。
    check_countdown: u64,
}

enum Step {
    Continue,
    /// 栈顶框架 RETURN——由最近的 exec() 捕获为调用结果
    Returned(Value),
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

impl Vm {
    pub fn new() -> Vm {
        Vm {
            limits: Limits::default(),
            stats: Stats::default(),
            module: None,
            imports: Vec::new(),
            host_fns: HashMap::new(),
            result_tag: None,
            fid_of_str: HashMap::new(),
            debug: DebugInfo::default(),
            frames: Vec::new(),
            stack: Vec::new(),
            heap_bytes: 0,
            output: Vec::new(),
            interrupt: None,
            deadline: None,
            op_histogram: None,
            // max(1,·)：check_interval=0 在取模实现下是除零 panic 面，
            // 倒数实现下按「每次检查」收敛为 1——不 panic（安全边界）。
            check_countdown: Limits::default().check_interval.max(1),
        }
    }

    /// 开启 opcode 直方图统计（profiling 专用；默认关闭零开销）。
    pub fn enable_op_histogram(&mut self) {
        self.op_histogram = Some(vec![0u64; yoru_bytecode::OPCODE_COUNT].into_boxed_slice());
    }

    /// 取走直方图（索引 = opcode 编码，值 = 执行次数）。未开启返回 None。
    pub fn take_op_histogram(&mut self) -> Option<Box<[u64]>> {
        self.op_histogram.take()
    }

    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
        self.check_countdown = self.limits.check_interval.max(1);
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// 观测快照（决策 72）。
    pub fn stats(&self) -> Stats {
        Stats {
            output_bytes: self.output.len(),
            ..self.stats
        }
    }

    /// 取走输出缓冲（print 内建产物）。
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    /// 带模块上下文的值格式化（27 号 A3 / 归并 26 号 D-21）：Variant
    /// 反查模块 tag 表显示 `Tag名#变体序号`（如 `MyErr#0`），替代此前
    /// 无身份的 `<variant 0>`。tag 名存于模块字符串表（TagInfo.name 为
    /// 字符串索引），变体名未入字节码格式（只有载荷标志）——显示到
    /// tag 名 + 序号 + 载荷已足够定位错误身份；完全恢复变体名需扩
    /// TagInfo 序列化格式，归入未来需要时再议（27 号 §5.3）。
    /// 无模块上下文或越界时退回 `<variant N>`（Value::display_with）。
    pub fn format_value(&self, v: &Value) -> String {
        match self.module.as_ref() {
            Some(m) => {
                let mut resolve = |tag: u32, variant: u32| -> Option<String> {
                    m.tags
                        .get(tag as usize)
                        .and_then(|t| m.strings.get(t.name as usize))
                        .map(|name| format!("{name}#{variant}"))
                };
                v.display_with(&mut resolve)
            }
            None => v.display(),
        }
    }

    /// 输出一行（print 内建与 demo ctx.log 共用；决策 70/73 宿主通道）。
    pub(crate) fn emit_line(&mut self, parts: &[Value]) {
        let line: Vec<String> = parts.iter().map(|v| self.format_value(v)).collect();
        let mut out = line.join(" ");
        out.push('\n');
        self.output.extend_from_slice(out.as_bytes());
    }

    /// 设置中断钩子（决策 71：每 check_interval 条指令检查一次）。
    pub fn set_interrupt(&mut self, f: InterruptFn) {
        self.interrupt = Some(f);
    }

    /// 设置执行截止时刻（节流检查，不影响热循环性能）。
    pub fn set_deadline(&mut self, at: Instant) {
        self.deadline = Some(at);
    }

    /// 注册宿主自由函数（阶段 4，12.2 契约；纯 Rust 闭包零 C ABI）。
    /// 能力注入的运行时通道：脚本调用 `名(args)`（内建未命中后按名分派，
    /// 决策 76）。签名契约单一闸口在 typeck（`Env::with_host_fns`）——
    /// 未注入名字的调用 = 编译期 Y-T0001；此处仅做 fail-closed 兜底
    /// （未注册名调用 = 运行时 Y-H9000，覆盖未经 typeck 的字节码直载）。
    ///
    /// **嵌入体感（2026-10-12 外部探针实证）**：接受任意 `Fn` 闭包（含
    /// `move` 捕获宿主状态），内部统一 `Rc` 包装——嵌入方不写
    /// `Rc::new(..) as HostFn` 礼节（`demo.rs` 旧写法即该礼节的来源）。
    /// 泛型参数使裸闭包与 `Rc<dyn Fn>` 均可传入（后者经 `Rc<F>: Fn`）。
    pub fn register_host_fn<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&mut Vm, &[Value]) -> Result<Value, VmError> + 'static,
    {
        let boxed: HostFn = Rc::new(f);
        self.host_fns.insert(name.to_string(), boxed);
    }

    /// 宿主函数调用：查注册表 → **panic 边界**（catch_unwind → 结构化
    /// Y-H9001，宿主 panic 不穿透 VM 边界——退出标准）。panic 后 VM 帧状态
    /// 视为作废：错误直接终止本次 run，脚本不可捕获（宿主 panic 是宿主
    /// 侧缺陷，语义严于 Lua pcall）。
    pub(crate) fn call_host(&mut self, name: &str, args: &[Value]) -> Result<Value, VmError> {
        let Some(f) = self.host_fns.get(name).cloned() else {
            return Err(self.rerr(
                "Y-H9000",
                format!("host fn `{name}` not registered (capability not injected)"),
            ));
        };
        // edition 2024 析构时序：catch_unwind 的临时量（闭包捕获 f/self/args）
        // 先绑定再 match——显式固定「临时量先于局部 f 析构」的次序，消除
        // tail-expr-drop-order 跨 edition 语义差（f 为 Rc 闭包，两侧均无副作用，
        // 绑定使次序与 edition 无关）。
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self, args)));
        match caught {
            Ok(r) => r,
            Err(payload) => {
                let detail = payload
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                let msg = if detail.is_empty() {
                    format!("host fn `{name}` panicked")
                } else {
                    format!("host fn `{name}` panicked: {detail}")
                };
                Err(self.rerr("Y-H9001", msg))
            }
        }
    }

    /// 调试段（决策 70）：错误行偏移回溯与入口发现。
    pub fn debug(&self) -> &DebugInfo {
        &self.debug
    }

    // ---- 运行入口 ----

    /// 执行入口函数。`entry=None` 时找 `main`；找不到 = 结构化错误。
    pub fn run(
        &mut self,
        inst: Instance,
        entry: Option<&str>,
        args: Vec<Value>,
    ) -> Result<Value, VmError> {
        self.load(inst);
        let name = entry.unwrap_or("main");
        let proto = self
            .debug
            .proto_by_name(name, &self.module_ref().strings)
            .ok_or_else(|| {
                self.rerr_no_pc(
                    "Y-R9000",
                    format!("entry `{name}` not found (module lacks debug segment or function)"),
                )
            })?;
        self.run_at_loaded(proto, args)
    }

    /// 按原型序号执行（strip 产物的入口路径：宿主在 debug 产物上解析好序号再 strip）。
    pub fn run_at(
        &mut self,
        inst: Instance,
        proto: usize,
        args: Vec<Value>,
    ) -> Result<Value, VmError> {
        self.load(inst);
        if proto >= self.module_ref().protos.len() {
            return Err(self.rerr_no_pc("Y-R9000", format!("proto #{proto} out of range")));
        }
        self.run_at_loaded(proto, args)
    }

    fn run_at_loaded(&mut self, proto: usize, args: Vec<Value>) -> Result<Value, VmError> {
        let closure = Value::Closure(Rc::new(ClosureObj {
            proto: proto as u32,
            upvals: Vec::new(),
        }));
        self.call_value(closure, args)
    }

    fn load(&mut self, inst: Instance) {
        self.module = Some(inst.module.clone());
        self.imports = inst.imports;
        self.result_tag = inst.result_tag;
        self.fid_of_str = inst.fid_of_str;
        self.debug = DebugInfo::from_module(&inst.module).unwrap_or_default();
        self.frames.clear();
        self.stack.clear();
        // 决策 79：三限预算改 run 作用域——指令/调用/堆计数随 load 重置，
        // 长驻宿主（同一 Vm 反复 run：REPL/服务器/宿主多次回调脚本）不再
        // 被历史消耗饿死；单调累计在 stats.total_*（观测，不参与限额）。
        // deadline/interrupt 有意不重置：宿主 `set_deadline(t); run(..)`
        // 的调用序会被 load 抹掉。
        self.heap_bytes = 0;
        self.stats.instructions = 0;
        self.stats.calls = 0;
        self.stats.max_call_depth = 0;
        self.stats.heap_bytes = 0;
        // 节流倒数随 run 作用域重置（与指令计数同生命周期）
        self.check_countdown = self.limits.check_interval.max(1);
    }

    fn module_ref(&self) -> &Lbc0Module {
        self.module.as_ref().expect("run without load")
    }

    // ---- 调用与帧 ----

    /// 统一调用点：闭包（压帧 + 递归执行）/ 宿主函数 / 内建名分派。
    fn call_value(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, VmError> {
        match callee {
            Value::Closure(c) => {
                if self.frames.len() >= self.limits.max_call_depth {
                    return Err(VmError::Limit(LimitKind::CallDepth));
                }
                let proto = &self.module_ref().protos[c.proto as usize];
                if args.len() != proto.params as usize {
                    let name = self
                        .debug
                        .names
                        .get(c.proto as usize)
                        .copied()
                        .flatten()
                        .and_then(|i| self.module_ref().strings.get(i as usize).cloned())
                        .unwrap_or_else(|| format!("#{}", c.proto));
                    return Err(self.rerr_no_pc(
                        "Y-R9002",
                        format!("`{name}` expects {} args, got {}", proto.params, args.len()),
                    ));
                }
                let locals_n = proto.locals as usize;
                let mut locals = vec![Value::Nil; locals_n];
                for (i, a) in args.into_iter().enumerate() {
                    locals[i] = a;
                }
                self.stats.calls += 1;
                self.frames.push(Frame {
                    closure: c,
                    pc: 0,
                    locals,
                    stack_base: self.stack.len(),
                });
                self.stats.max_call_depth = self.stats.max_call_depth.max(self.frames.len());
                self.exec()
            }
            Value::HostFn(f) => f(self, &args),
            Value::Str(name) => self.dispatch_builtin(&name, &args),
            other => Err(self.rerr_no_pc(
                "Y-R9003",
                format!("value is not callable: {}", other.display()),
            )),
        }
    }

    /// 执行到当前栈顶框架 RETURN，返回其返回值（Rust 递归深度 = Yoru 调用深度，
    /// 受 max_call_depth 约束）。
    fn exec(&mut self) -> Result<Value, VmError> {
        loop {
            match self.step()? {
                Step::Returned(v) => return Ok(v),
                Step::Continue => {}
            }
        }
    }

    fn cur_proto(&self) -> usize {
        self.frames
            .last()
            .expect("step without frame")
            .closure
            .proto as usize
    }

    fn cur_pc(&self) -> usize {
        self.frames.last().expect("step without frame").pc
    }

    /// 运行时错误（Y-R 域）：附当前 pc 与源偏移（决策 70）。
    fn rerr(&self, code: &'static str, msg: impl Into<String>) -> VmError {
        let (proto, pc) = if self.frames.is_empty() {
            (usize::MAX, 0)
        } else {
            (self.cur_proto(), self.cur_pc())
        };
        let src_off = self.debug.src_off(proto, pc);
        VmError::Runtime(RuntimeError {
            code,
            message: msg.into(),
            proto,
            pc,
            src_off,
        })
    }

    fn rerr_no_pc(&self, code: &'static str, msg: impl Into<String>) -> VmError {
        VmError::Runtime(RuntimeError {
            code,
            message: msg.into(),
            proto: usize::MAX,
            pc: 0,
            src_off: None,
        })
    }

    // ---- 指令步进 ----

    /// 值排序比较（Lt/Le/Gt/Ge 与融合条件跳转共用；语义单一事实源）。
    fn cmp_values(&mut self, a: &Value, b: &Value) -> Result<std::cmp::Ordering, VmError> {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
            (Value::Str(x), Value::Str(y)) => Ok(x.cmp(y)),
            (Value::Float(x), Value::Float(y)) => x
                .partial_cmp(y)
                .ok_or_else(|| self.rerr("Y-R9011", "float comparison with NaN is unordered")),
            _ => Err(self.rerr(
                "Y-R9007",
                format!("cannot order {} and {}", a.display(), b.display()),
            )),
        }
    }

    fn step(&mut self) -> Result<Step, VmError> {
        // 预算与节流（决策 71）：max_instructions 每条检查（cmp 便宜，上界
        // 精确）；中断/时钟经倒数计数器节流——取代 `%` 取模（运行时变量
        // 除法 ~25 cycles/条，非二次幂不可强度削减），触发点逐位等价。
        self.stats.instructions += 1;
        self.stats.total_instructions += 1;
        if self.stats.instructions > self.limits.max_instructions {
            return Err(VmError::Limit(LimitKind::Instructions));
        }
        self.check_countdown -= 1;
        if self.check_countdown == 0 {
            self.check_countdown = self.limits.check_interval.max(1);
            if let Some(d) = self.deadline {
                if Instant::now() >= d {
                    return Err(VmError::Limit(LimitKind::Deadline));
                }
            }
            // 快照先行：interrupt 可变借用不得跨 stats() 不可变借用
            let snap = self.stats();
            if let Some(cb) = self.interrupt.as_mut() {
                if cb(&snap) {
                    return Err(VmError::Interrupted);
                }
            }
        }

        let pc = self.cur_pc();
        let proto = self.cur_proto();
        let instr: Instr = self.module_ref().protos[proto].code[pc];
        self.frames.last_mut().expect("frame").pc += 1;

        // P0a①：直方图统计点——None 时是单个可预测分支，热路径零实质开销
        if let Some(h) = self.op_histogram.as_mut() {
            h[instr.op as usize] += 1;
        }

        match instr.op {
            // ---- 常量/构造 ----
            Opcode::Const => {
                let c = self.module_ref().consts[instr.arg as usize].clone();
                let v = match c {
                    Const::Int(v) => Value::Int(v),
                    Const::Float(v) => Value::Float(v),
                    Const::Str(i) => self.str_value(i),
                    Const::Bool(b) => Value::Bool(b),
                    Const::Variant { tag, variant } => Value::Variant(Rc::new(VariantVal {
                        tag,
                        variant,
                        payload: None,
                    })),
                    Const::Proto(_) => {
                        return Err(self.rerr("Y-I9002", "Const(Proto) is not executable"));
                    }
                };
                self.push(v);
            }
            Opcode::Nil => self.push(Value::Nil),
            Opcode::True => self.push(Value::Bool(true)),
            Opcode::False => self.push(Value::Bool(false)),
            Opcode::NewList => {
                let items = self.pop_n(instr.arg as usize)?;
                self.alloc(items.len() as i64 * 16 + 32)?;
                self.push(Value::List(Rc::new(RefCell::new(items))));
            }
            Opcode::NewMap => {
                let n = instr.arg as usize;
                let flat = self.pop_n(n * 2)?;
                let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(n);
                let mut it = flat.into_iter().rev();
                for _ in 0..n {
                    let v = it.next().expect("pair v");
                    let k = it.next().expect("pair k");
                    pairs.push((k, v));
                }
                self.alloc(n as i64 * 32 + 32)?;
                self.push(Value::Map(Rc::new(RefCell::new(pairs))));
            }
            Opcode::NewStruct => {
                let si = instr.arg as usize;
                let field_strs = self.module_ref().structs[si].fields.clone();
                let vals = self.pop_n(field_strs.len())?;
                let mut fields = Vec::with_capacity(field_strs.len());
                for (sidx, v) in field_strs.into_iter().zip(vals) {
                    let fid = *self.fid_of_str.get(&sidx).ok_or_else(|| {
                        self.rerr("Y-I9003", format!("field string {sidx} not in field table"))
                    })?;
                    fields.push((fid, v));
                }
                self.alloc(fields.len() as i64 * 16 + 32)?;
                self.push(Value::Struct(Rc::new(RefCell::new(StructObj {
                    struct_id: si as u32,
                    fields,
                }))));
            }
            Opcode::NewClosure => {
                let pi = instr.arg as usize;
                let descs = self.module_ref().protos[pi].upvalues.clone();
                let mut upvals = Vec::with_capacity(descs.len());
                for d in descs {
                    let v = if d.is_local {
                        self.frames
                            .last()
                            .expect("frame")
                            .locals
                            .get(d.index as usize)
                            .cloned()
                            .unwrap_or(Value::Nil)
                    } else {
                        match self
                            .frames
                            .last()
                            .expect("frame")
                            .closure
                            .upvals
                            .get(d.index as usize)
                        {
                            Some(v) => v.clone(),
                            None => {
                                return Err(self.rerr("Y-I9004", "upvalue index out of range"));
                            }
                        }
                    };
                    upvals.push(v);
                }
                self.alloc(upvals.len() as i64 * 16 + 32)?;
                self.push(Value::Closure(Rc::new(ClosureObj {
                    proto: pi as u32,
                    upvals,
                })));
            }

            // ---- 变量/闭包 ----
            Opcode::LoadLocal => {
                let v = self.frames.last().expect("frame").locals[instr.arg as usize].clone();
                self.push(v);
            }
            Opcode::StoreLocal => {
                let v = self.pop();
                let f = self.frames.last_mut().expect("frame");
                f.locals[instr.arg as usize] = v;
            }
            Opcode::LoadUpvalue => {
                let f = self.frames.last().expect("frame");
                let v = f
                    .closure
                    .upvals
                    .get(instr.arg as usize)
                    .cloned()
                    .ok_or_else(|| self.rerr("Y-I9004", "upvalue index out of range"))?;
                self.push(v);
            }
            Opcode::StoreUpvalue => {
                let v = self.pop();
                // 按值快照模型（决策 31）：写入闭包自身的快照槽
                let idx = instr.arg as usize;
                let f = self.frames.last().expect("frame");
                let mut rc = Rc::clone(&f.closure);
                // 按值快照模型（决策 31）：get_mut 双重裁决（共享拒绝 / 索引在界）
                let slot = Rc::get_mut(&mut rc)
                    .ok_or_else(|| self.rerr("Y-I9005", "upvalue closure is shared, cannot store"))?
                    .upvals
                    .get_mut(idx)
                    .ok_or_else(|| self.rerr("Y-I9004", "upvalue index out of range"))?;
                *slot = v;
            }
            Opcode::LoadImport => {
                let v = self
                    .imports
                    .get(instr.arg as usize)
                    .cloned()
                    .ok_or_else(|| self.rerr("Y-I9006", "import slot unset"))?;
                self.push(v);
            }

            // ---- 运算 ----
            Opcode::Add | Opcode::Sub | Opcode::Mul | Opcode::Div | Opcode::Mod => {
                let (a, b) = self.pop2()?;
                let v = match (a, b) {
                    (Value::Int(a), Value::Int(b)) => {
                        let r = match instr.op {
                            Opcode::Add => a.checked_add(b),
                            Opcode::Sub => a.checked_sub(b),
                            Opcode::Mul => a.checked_mul(b),
                            Opcode::Div => {
                                if b == 0 {
                                    return Err(self.rerr("Y-R9006", "integer division by zero"));
                                }
                                a.checked_div_euclid(b)
                            }
                            _ => {
                                if b == 0 {
                                    return Err(self.rerr("Y-R9006", "integer modulo by zero"));
                                }
                                a.checked_rem_euclid(b)
                            }
                        };
                        Value::Int(r.ok_or_else(|| self.rerr("Y-R9001", "integer overflow"))?)
                    }
                    (Value::Float(a), Value::Float(b)) => {
                        let r = match instr.op {
                            Opcode::Add => a + b,
                            Opcode::Sub => a - b,
                            Opcode::Mul => a * b,
                            Opcode::Div => a / b,
                            _ => a % b,
                        };
                        Value::Float(r)
                    }
                    (Value::Str(a), Value::Str(b)) if instr.op == Opcode::Add => {
                        let s: String = format!("{a}{b}");
                        self.alloc(s.len() as i64)?;
                        Value::Str(Rc::from(s.as_str()))
                    }
                    (a, b) => {
                        return Err(self.rerr(
                            "Y-R9007",
                            format!(
                                "bad operand types for arithmetic: {}, {}",
                                a.display(),
                                b.display()
                            ),
                        ));
                    }
                };
                self.push(v);
            }
            Opcode::Neg => {
                let v = self.pop();
                let r = match v {
                    Value::Int(a) => Value::Int(
                        a.checked_neg()
                            .ok_or_else(|| self.rerr("Y-R9001", "integer overflow"))?,
                    ),
                    Value::Float(a) => Value::Float(-a),
                    other => {
                        return Err(
                            self.rerr("Y-R9007", format!("cannot negate {}", other.display()))
                        );
                    }
                };
                self.push(r);
            }
            Opcode::Not => {
                let v = self.pop();
                let r = match v {
                    Value::Bool(b) => Value::Bool(!b),
                    other => {
                        return Err(self.rerr(
                            "Y-R9008",
                            format!("`not` expects bool, got {}", other.display()),
                        ));
                    }
                };
                self.push(r);
            }
            Opcode::Band
            | Opcode::Bor
            | Opcode::Bxor
            | Opcode::Shl
            | Opcode::Shr
            | Opcode::Ushr => {
                let (a, b) = self.pop2()?;
                let (Value::Int(a), Value::Int(b)) = (&a, &b) else {
                    return Err(self.rerr(
                        "Y-R9007",
                        format!(
                            "bitwise expects int operands, got {}, {}",
                            a.display(),
                            b.display()
                        ),
                    ));
                };
                let (a, b) = (*a, *b);
                let r = match instr.op {
                    Opcode::Band => Value::Int(a & b),
                    Opcode::Bor => Value::Int(a | b),
                    Opcode::Bxor => Value::Int(a ^ b),
                    _ => {
                        if !(0..64).contains(&b) {
                            return Err(
                                self.rerr("Y-R9009", format!("shift amount {b} outside 0..64"))
                            );
                        }
                        match instr.op {
                            Opcode::Shl => Value::Int(a.checked_shl(b as u32).unwrap_or(0)),
                            Opcode::Shr => Value::Int(a >> b),
                            _ => Value::Int(((a as u64) >> b) as i64),
                        }
                    }
                };
                self.push(r);
            }
            Opcode::Eq | Opcode::Ne => {
                let (a, b) = self.pop2()?;
                let eq = a.deep_eq(&b);
                self.push(Value::Bool(if instr.op == Opcode::Eq { eq } else { !eq }));
            }
            Opcode::Lt | Opcode::Le | Opcode::Gt | Opcode::Ge => {
                let (a, b) = self.pop2()?;
                let ok = match self.cmp_values(&a, &b)? {
                    Ordering::Less => matches!(instr.op, Opcode::Lt | Opcode::Le),
                    Ordering::Equal => matches!(instr.op, Opcode::Le | Opcode::Ge),
                    Ordering::Greater => matches!(instr.op, Opcode::Gt | Opcode::Ge),
                };
                self.push(Value::Bool(ok));
            }

            // ---- 控制流 ----
            Opcode::Jmp => {
                self.jump(instr);
            }
            Opcode::JmpIf | Opcode::JmpIfNot => {
                let cond = self.pop();
                let Value::Bool(b) = cond else {
                    return Err(self.rerr("Y-R9008", "branch condition must be bool"));
                };
                if b == (instr.op == Opcode::JmpIf) {
                    self.jump(instr);
                }
            }
            // 融合条件跳转（P0a③）：`Xx; JmpIfNot t` 一条完成——比较语义经
            // cmp_values 与独立比较指令逐位一致（含 Y-R9007/Y-R9011 错误面）。
            Opcode::JmpIfNotLt | Opcode::JmpIfNotLe | Opcode::JmpIfNotGt | Opcode::JmpIfNotGe => {
                let b = self.pop();
                let a = self.pop();
                let holds = match self.cmp_values(&a, &b)? {
                    Ordering::Less => matches!(instr.op, Opcode::JmpIfNotLt | Opcode::JmpIfNotLe),
                    Ordering::Equal => {
                        matches!(instr.op, Opcode::JmpIfNotLe | Opcode::JmpIfNotGe)
                    }
                    Ordering::Greater => {
                        matches!(instr.op, Opcode::JmpIfNotGt | Opcode::JmpIfNotGe)
                    }
                };
                if !holds {
                    self.jump(instr);
                }
            }

            // ---- 对象 ----
            Opcode::GetField => {
                let base = self.pop();
                let name = self.field_name(instr.arg)?;
                let v = match base {
                    Value::Struct(s) => {
                        let s = s.borrow();
                        s.fields
                            .iter()
                            .find(|(f, _)| *f == instr.arg)
                            .map(|(_, v)| v.clone())
                            .ok_or_else(|| {
                                self.rerr("Y-R9012", format!("struct has no field `{name}`"))
                            })?
                    }
                    Value::Map(m) => m
                        .borrow()
                        .iter()
                        .find(|(k, _)| matches!(k, Value::Str(s) if **s == name))
                        .map(|(_, v)| v.clone())
                        .unwrap_or(Value::Nil),
                    other => {
                        return Err(self.rerr(
                            "Y-R9012",
                            format!("cannot access field `{name}` on {}", other.display()),
                        ));
                    }
                };
                self.push(v);
            }
            Opcode::SetField => {
                let value = self.pop();
                let container = self.pop();
                let name = self.field_name(instr.arg)?;
                let out = match container {
                    Value::Struct(s) => self.with_struct_mut(s, instr.arg, value)?,
                    Value::Map(m) => Value::Map(self.with_map_mut(
                        m,
                        Value::Str(Rc::from(name.as_str())),
                        value,
                    )?),
                    other => {
                        return Err(self.rerr(
                            "Y-R9012",
                            format!("cannot set field `{name}` on {}", other.display()),
                        ));
                    }
                };
                self.push(out);
            }
            Opcode::GetIndex => {
                let idx = self.pop();
                let base = self.pop();
                let v = match (&base, &idx) {
                    (Value::List(l), Value::Int(i)) => {
                        let l = l.borrow();
                        let i = *i;
                        if i < 0 || i as usize >= l.len() {
                            return Err(self.rerr(
                                "Y-R9013",
                                format!("list index {i} outside 0..{}", l.len()),
                            ));
                        }
                        l[i as usize].clone()
                    }
                    (Value::Map(m), k) => m
                        .borrow()
                        .iter()
                        .find(|(k2, _)| k2.deep_eq(k))
                        .map(|(_, v)| v.clone())
                        .unwrap_or(Value::Nil), // 决策 55：Map 缺键取 nil
                    (Value::Str(_), Value::Int(_)) => {
                        return Err(self.rerr(
                            "Y-R9014",
                            "string indexing forbidden (decision 46); iterate or slice",
                        ));
                    }
                    _ => {
                        return Err(self.rerr(
                            "Y-R9013",
                            format!("cannot index {} with {}", base.display(), idx.display()),
                        ));
                    }
                };
                self.push(v);
            }
            // 融合下标读（决策 80 族对称，P0a③）：`locals[base][locals[idx]]`
            // 一条完成——语义与 GetIndex 逐位一致（边界/Map 缺键/Str 拒绝同面）。
            Opcode::GetIndexLL => {
                let idx = self.frames.last().expect("frame").locals[(instr.arg & 0xFFFF) as usize]
                    .clone();
                let base =
                    self.frames.last().expect("frame").locals[(instr.arg >> 16) as usize].clone();
                let v = match (&base, &idx) {
                    (Value::List(l), Value::Int(i)) => {
                        let l = l.borrow();
                        let i = *i;
                        if i < 0 || i as usize >= l.len() {
                            return Err(self.rerr(
                                "Y-R9013",
                                format!("list index {i} outside 0..{}", l.len()),
                            ));
                        }
                        l[i as usize].clone()
                    }
                    (Value::Map(m), k) => m
                        .borrow()
                        .iter()
                        .find(|(k2, _)| k2.deep_eq(k))
                        .map(|(_, v)| v.clone())
                        .unwrap_or(Value::Nil), // 决策 55：Map 缺键取 nil
                    (Value::Str(_), Value::Int(_)) => {
                        return Err(self.rerr(
                            "Y-R9014",
                            "string indexing forbidden (decision 46); iterate or slice",
                        ));
                    }
                    _ => {
                        return Err(self.rerr(
                            "Y-R9013",
                            format!("cannot index {} with {}", base.display(), idx.display()),
                        ));
                    }
                };
                self.push(v);
            }
            Opcode::SetIndex => {
                let value = self.pop();
                let idx = self.pop();
                let base = self.pop();
                let out = match base {
                    Value::List(l) => {
                        let Value::Int(i) = &idx else {
                            return Err(self.rerr("Y-R9013", "list index must be int"));
                        };
                        let i = *i;
                        let len = l.borrow().len();
                        if i < 0 || i as usize >= len {
                            return Err(
                                self.rerr("Y-R9013", format!("list index {i} outside 0..{len}"))
                            );
                        }
                        self.with_list_mut(l, |v| v[i as usize] = value.clone())?
                    }
                    Value::Map(m) => Value::Map(self.with_map_mut(m, idx, value)?),
                    other => {
                        return Err(self.rerr(
                            "Y-R9013",
                            format!("cannot index-assign {}", other.display()),
                        ));
                    }
                };
                self.push(out);
            }

            // ---- 调用 ----
            Opcode::Call | Opcode::CallMethod | Opcode::TailCall => {
                // CALLMETHOD 保留协议分派（阶段 4）；T1 中语义同 CALL（callee 在顶）。
                // TAILCALL 暂以普通调用执行（正确性优先，优化留 JIT 前）。
                let callee = self.pop();
                let args = self.pop_n(instr.arg as usize)?;
                let v = self.call_value(callee, args)?;
                self.push(v);
            }
            Opcode::CallHost => {
                // 直接编码的宿主调用（决策 76）：与 `Const(Str(名))` + Call
                // 同构——callee 须为名（Str），执行路径与内建未命中后的宿主
                // 分派完全一致（call_host：panic 边界 + fail closed）。
                let callee = self.pop();
                let args = self.pop_n(instr.arg as usize)?;
                let name = match &callee {
                    Value::Str(s) => s.to_string(),
                    other => {
                        return Err(self.rerr(
                            "Y-H9003",
                            format!("CallHost callee must be a name, got {}", other.display()),
                        ));
                    }
                };
                let v = self.call_host(&name, &args)?;
                self.push(v);
            }
            Opcode::Return => {
                let v = self.pop();
                let f = self.frames.pop().expect("return without frame");
                self.stack.truncate(f.stack_base);
                return Ok(Step::Returned(v));
            }

            // ---- Option/Result/错误 ----
            Opcode::IsNone => {
                let v = self.pop();
                // Option 运行期表示唯一 = Nil | T（D-05/D-20 修复后 mk_option
                // 恒产 Nil）——None 判定即 Nil。旧 Variant 臂（option_tag &&
                // variant==0）为表示统一前的残留死代码，决策 82 清理。
                self.push(Value::Bool(matches!(v, Value::Nil)));
            }
            Opcode::Unwrap => {
                let v = self.pop();
                // 表示统一（D-05/D-20）：`T?` 运行期 = Nil | T。Variant 仅 Result/用户
                // tag——解包取载荷；Nil 即 unwrap of nil（Y-R9015）；其余（Some 裸值）
                // 恒等返回。
                let payload = match &v {
                    Value::Variant(var) => var.payload.clone(),
                    Value::Nil => {
                        return Err(self.rerr("Y-R9015", "unwrap of nil"));
                    }
                    _ => None,
                };
                self.push(payload.map(|p| (*p).clone()).unwrap_or(v));
            }
            Opcode::IsErr => {
                let v = self.pop();
                let is_err = match &v {
                    Value::Variant(var) => Some(var.tag) == self.result_tag && var.variant == 1,
                    _ => false,
                };
                self.push(Value::Bool(is_err));
            }
            Opcode::SetIndexLocal => {
                // 快路径：弹 (值, 索引)，对局部槽原地变异。槽位值在此期间暂出，
                // 使 with_list_mut/with_map_mut 看到 refcount==1（仅槽位自身持有）。
                // 运行时错误必终止 run（try 是 Result 传播非异常捕获），槽位无需还原。
                let value = self.pop();
                let idx = self.pop();
                let slot = instr.arg as usize;
                let base = std::mem::replace(
                    &mut self.frames.last_mut().expect("frame").locals[slot],
                    Value::Nil,
                );
                let out: Result<Value, VmError> = 'op: {
                    match base {
                        Value::List(l) => {
                            let Value::Int(i) = &idx else {
                                break 'op Err(self.rerr("Y-R9013", "list index must be int"));
                            };
                            let i = *i;
                            let len = l.borrow().len();
                            if i < 0 || i as usize >= len {
                                break 'op Err(self
                                    .rerr("Y-R9013", format!("list index {i} outside 0..{len}")));
                            }
                            self.with_list_mut(l, |v| v[i as usize] = value)
                        }
                        Value::Map(m) => self.with_map_mut(m, idx, value).map(Value::Map),
                        other => {
                            break 'op Err(self.rerr(
                                "Y-R9013",
                                format!("cannot index-assign {}", other.display()),
                            ));
                        }
                    }
                };
                self.frames.last_mut().expect("frame").locals[slot] = out?;
            }
            Opcode::Raise => {
                let v = self.pop();
                return Err(VmError::Raised(v));
            }

            // ---- 阶段 2 扩展 ----
            Opcode::ToString => {
                let v = self.pop();
                let s = v.display();
                self.alloc(s.len() as i64)?;
                self.push(Value::Str(Rc::from(s.as_str())));
            }
            Opcode::ForPrep => {
                let iterable = self.pop();
                let cursor = match iterable {
                    Value::List(l) => Cursor::List {
                        items: l.borrow().clone(), // 按值快照（决策 31）
                        next: 0,
                    },
                    Value::Range(a, b) => Cursor::Range { next: a, end: b },
                    Value::Str(s) => Cursor::Str {
                        chars: s.chars().collect(),
                        next: 0,
                    },
                    Value::Map(m) => Cursor::Map {
                        keys: m.borrow().iter().map(|(k, _)| k.clone()).collect(),
                        next: 0,
                    },
                    other => {
                        return Err(
                            self.rerr("Y-R9016", format!("{} is not iterable", other.display()))
                        );
                    }
                };
                self.alloc(64)?;
                self.push(Value::Cursor(Rc::new(RefCell::new(cursor))));
            }
            Opcode::ForNext => {
                let slot = instr.arg as usize;
                let cursor_rc = match self.stack.last() {
                    Some(Value::Cursor(c)) => Rc::clone(c),
                    _ => return Err(self.rerr("Y-I9007", "ForNext without cursor on stack")),
                };
                let mut cursor = cursor_rc.borrow_mut();
                let next = match &mut *cursor {
                    Cursor::List { items, next } => {
                        if *next < items.len() {
                            let v = items[*next].clone();
                            *next += 1;
                            Some(v)
                        } else {
                            None
                        }
                    }
                    Cursor::Range { next, end } => {
                        if *next < *end {
                            let v = *next;
                            *next += 1;
                            Some(Value::Int(v))
                        } else {
                            None
                        }
                    }
                    Cursor::Str { chars, next } => {
                        if *next < chars.len() {
                            let v = chars[*next];
                            *next += 1;
                            Some(Value::Int(v as i64))
                        } else {
                            None
                        }
                    }
                    Cursor::Map { keys, next } => {
                        if *next < keys.len() {
                            let v = keys[*next].clone();
                            *next += 1;
                            Some(v)
                        } else {
                            None
                        }
                    }
                };
                drop(cursor);
                let has = next.is_some();
                if let Some(v) = next {
                    let f = self.frames.last_mut().expect("frame");
                    f.locals[slot] = v;
                }
                self.push(Value::Bool(has));
            }
            Opcode::IsTag => {
                let v = self.pop();
                let (tag, variant) = self.variant_const(instr.arg)?;
                let ok =
                    matches!(&v, Value::Variant(var) if var.tag == tag && var.variant == variant);
                self.push(Value::Bool(ok));
            }
            Opcode::Payload => {
                let v = self.pop();
                let (tag, variant) = self.variant_const(instr.arg)?;
                let payload = match v {
                    Value::Variant(var) if var.tag == tag && var.variant == variant => {
                        var.payload.clone()
                    }
                    other => {
                        return Err(self.rerr(
                            "Y-R9017",
                            format!(
                                "payload extract on non-matching variant: {}",
                                other.display()
                            ),
                        ));
                    }
                };
                self.push(payload.map(|p| (*p).clone()).unwrap_or(Value::Nil));
            }
            Opcode::TagNew => {
                let (tag, variant) = self.variant_const(instr.arg)?;
                let has_payload = self.module_ref().tags[tag as usize].variants[variant as usize];
                let payload = if has_payload {
                    Some(Rc::new(self.pop()))
                } else {
                    None
                };
                self.alloc(32)?;
                self.push(Value::Variant(Rc::new(VariantVal {
                    tag,
                    variant,
                    payload,
                })));
            }
            Opcode::Convert => {
                let v = self.pop();
                let r = match (instr.arg, &v) {
                    (0, Value::Int(a)) => Value::Int(*a), // as int
                    (0, Value::Float(a)) => {
                        if !a.is_finite() || *a < i64::MIN as f64 || *a >= i64::MAX as f64 {
                            return Err(self.rerr("Y-R9018", "float as int out of range"));
                        }
                        Value::Int(*a as i64)
                    }
                    (1, Value::Int(a)) => Value::Float(*a as f64), // as float
                    (1, Value::Float(a)) => Value::Float(*a),
                    (2, Value::Int(a)) => {
                        // as rune：码点合法域检查（决策 46）
                        match u32::try_from(*a).ok().and_then(char::from_u32) {
                            Some(_) => Value::Int(*a),
                            None => {
                                return Err(
                                    self.rerr("Y-R9019", format!("invalid rune code point {a}"))
                                );
                            }
                        }
                    }
                    _ => {
                        return Err(self.rerr(
                            "Y-R9018",
                            format!(
                                "unsupported conversion (target {}) on {}",
                                instr.arg,
                                v.display()
                            ),
                        ));
                    }
                };
                self.push(r);
            }
            Opcode::Pop => {
                self.pop();
            }
            Opcode::NewRange => {
                let (a, b) = self.pop2()?;
                let (Value::Int(a), Value::Int(b)) = (a, b) else {
                    return Err(self.rerr("Y-R9007", "range bounds must be int"));
                };
                self.push(Value::Range(a, b));
            }
            Opcode::Slice => {
                let hi = self.pop();
                let lo = self.pop();
                let base = self.pop();
                let (Value::Int(lo), Value::Int(hi)) = (lo, hi) else {
                    return Err(self.rerr("Y-R9007", "slice bounds must be int"));
                };
                if lo < 0 || hi < lo {
                    return Err(self.rerr("Y-R9020", format!("invalid slice {lo}..{hi}")));
                }
                let v = match base {
                    Value::List(l) => {
                        let l = l.borrow();
                        if hi as usize > l.len() {
                            return Err(self.rerr(
                                "Y-R9020",
                                format!("slice {lo}..{hi} outside 0..{}", l.len()),
                            ));
                        }
                        let items: Vec<Value> = l[lo as usize..hi as usize].to_vec();
                        self.alloc(items.len() as i64 * 16 + 32)?;
                        Value::List(Rc::new(RefCell::new(items)))
                    }
                    Value::Str(s) => {
                        let chars: Vec<char> = s.chars().collect();
                        if hi as usize > chars.len() {
                            return Err(self.rerr(
                                "Y-R9020",
                                format!("slice {lo}..{hi} outside 0..{}", chars.len()),
                            ));
                        }
                        let out: String = chars[lo as usize..hi as usize].iter().collect();
                        self.alloc(out.len() as i64)?;
                        Value::Str(Rc::from(out.as_str()))
                    }
                    other => {
                        return Err(
                            self.rerr("Y-R9020", format!("cannot slice {}", other.display()))
                        );
                    }
                };
                self.push(v);
            }
            Opcode::Await => {
                // T1：所有值恒就绪，await = 恒等（22 号诚实边界：暂停/恢复协议留阶段 4）
            }
            Opcode::Suspend | Opcode::Resume => {
                return Err(self.rerr("Y-R9010", "coroutines not supported in T1"));
            }
        }
        Ok(Step::Continue)
    }

    // ---- 基础操作 ----

    fn push(&mut self, v: Value) {
        self.stack.push(v);
    }

    fn pop(&mut self) -> Value {
        self.stack.pop().expect("stack underflow (verified)")
    }

    fn pop2(&mut self) -> Result<(Value, Value), VmError> {
        let b = self.pop();
        let a = self.pop();
        Ok((a, b))
    }

    fn pop_n(&mut self, n: usize) -> Result<Vec<Value>, VmError> {
        if self.stack.len() < n {
            return Err(self.rerr("Y-I9008", "stack underflow in pop_n"));
        }
        Ok(self.stack.split_off(self.stack.len() - n))
    }

    /// 跳转：pc 已 +1，目标相对当前指令（与 patch_jump(pc, target) 发射语义一致）。
    fn jump(&mut self, instr: Instr) {
        let f = self.frames.last_mut().expect("frame");
        let target = (f.pc as i64 - 1) + instr.sarg() as i64;
        f.pc = target.max(0) as usize;
    }

    fn str_value(&self, i: u32) -> Value {
        let s = self
            .module_ref()
            .strings
            .get(i as usize)
            .cloned()
            .unwrap_or_default();
        Value::Str(Rc::from(s.as_str()))
    }

    fn field_name(&self, fid: u32) -> Result<String, VmError> {
        let sidx = self
            .module_ref()
            .field_names
            .get(fid as usize)
            .copied()
            .ok_or_else(|| self.rerr("Y-I9010", format!("field id {fid} out of range")))?;
        Ok(self
            .module_ref()
            .strings
            .get(sidx as usize)
            .cloned()
            .unwrap_or_default())
    }

    fn variant_const(&self, ci: u32) -> Result<(u32, u32), VmError> {
        match self.module_ref().consts.get(ci as usize) {
            Some(Const::Variant { tag, variant }) => Ok((*tag, *variant)),
            _ => Err(self.rerr("Y-I9011", format!("const {ci} is not a variant"))),
        }
    }

    /// 堆账本记账（决策 71/72）：bytes 由各创建点按**创建时量级**估算
    /// （List/Map/Struct/Closure ×(元素数×槽宽)+对象头，Str=len）——这是堆
    /// 观测与 max_heap_bytes 限额的**唯一口径**（诚实边界：量级估算而非精确
    /// 簿记，Rc 共享不重复计）。Value 侧不再提供平行估算 API，避免口径漂移。
    fn alloc(&mut self, bytes: i64) -> Result<(), VmError> {
        self.heap_bytes += bytes;
        self.stats.heap_bytes = self.heap_bytes;
        self.stats.total_allocated += bytes;
        if self.heap_bytes > self.limits.max_heap_bytes {
            return Err(VmError::Limit(LimitKind::HeapBytes));
        }
        Ok(())
    }

    /// COW 列表变更（决策 13：refcount==1 原地，否则拷贝后改）。
    fn with_list_mut<F>(&mut self, l: Rc<RefCell<Vec<Value>>>, f: F) -> Result<Value, VmError>
    where
        F: FnOnce(&mut Vec<Value>),
    {
        if Rc::strong_count(&l) == 1 {
            f(&mut l.borrow_mut());
            Ok(Value::List(l))
        } else {
            let mut copy = l.borrow().clone();
            self.alloc(copy.capacity() as i64 * 16 + 32)?;
            f(&mut copy);
            Ok(Value::List(Rc::new(RefCell::new(copy))))
        }
    }

    /// COW 映射变更：键存在则更新（保持插入序），否则尾部插入（决策 36）。
    fn with_map_mut(&mut self, m: MapBox, k: Value, v: Value) -> Result<MapBox, VmError> {
        if Rc::strong_count(&m) == 1 {
            {
                let mut guard = m.borrow_mut();
                match guard.iter_mut().find(|(k2, _)| k2.deep_eq(&k)) {
                    Some(slot) => slot.1 = v,
                    None => guard.push((k, v)),
                }
            }
            Ok(m)
        } else {
            let mut copy = m.borrow().clone();
            self.alloc(copy.capacity() as i64 * 32 + 32)?;
            match copy.iter_mut().find(|(k2, _)| k2.deep_eq(&k)) {
                Some(slot) => slot.1 = v,
                None => copy.push((k, v)),
            }
            Ok(Rc::new(RefCell::new(copy)))
        }
    }

    /// COW struct 变更（决策 13）。
    fn with_struct_mut(
        &mut self,
        s: Rc<RefCell<StructObj>>,
        fid: u32,
        value: Value,
    ) -> Result<Value, VmError> {
        if Rc::strong_count(&s) == 1 {
            {
                let mut s = s.borrow_mut();
                match s.fields.iter_mut().find(|(f, _)| *f == fid) {
                    Some(slot) => slot.1 = value,
                    None => {
                        return Err(self.rerr("Y-R9012", format!("struct has no field id {fid}")));
                    }
                }
            }
            Ok(Value::Struct(s))
        } else {
            let mut copy = s.borrow().clone();
            self.alloc(copy.fields.len() as i64 * 16 + 32)?;
            match copy.fields.iter_mut().find(|(f, _)| *f == fid) {
                Some(slot) => slot.1 = value,
                None => {
                    return Err(self.rerr("Y-R9012", format!("struct has no field id {fid}")));
                }
            }
            Ok(Value::Struct(Rc::new(RefCell::new(copy))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yoru_syntax::parse_source;

    fn compile(src: &str) -> Instance {
        let program = parse_source(src).expect("parse");
        let m = yoru_bytecode::lower(&program).expect("lower");
        yoru_bytecode::verify_module(&m).expect("verify");
        Instance::new(m)
    }

    fn run_src(src: &str) -> Result<Value, VmError> {
        let inst = compile(src);
        Vm::new().run(inst, None, vec![])
    }

    // ---- 基础语义 ----

    #[test]
    fn arithmetic_and_strings() {
        let v = run_src(
            "pub fn main() -> string {\n    let a = 7\n    let b = 2\n    return \"sum=${a + b} mod=${a % b} neg=${-(a - 9)}\"\n}\n",
        )
        .expect("run");
        assert_eq!(v, Value::Str(Rc::from("sum=9 mod=1 neg=2")));
    }

    #[test]
    fn floor_div_mod_nonnegative() {
        let v = run_src("pub fn main() -> int {\n    return (0 - 7) % 3\n}\n").expect("run");
        let Value::Int(m) = v else { panic!("int") };
        assert_eq!(m, 2, "floor-mod 恒非负（决策 44）");
    }

    #[test]
    fn integer_overflow_raises_runtime_error() {
        let err =
            run_src("pub fn main() -> int {\n    return 9223372036854775807 + 1\n}\n").unwrap_err();
        let VmError::Runtime(e) = err else {
            panic!("runtime")
        };
        assert_eq!(e.code, "Y-R9001");
    }

    #[test]
    fn control_flow_loops() {
        let v = run_src(
            "pub fn main() -> int {\n    let mut total = 0\n    for i in 0..10 {\n        if i % 2 == 0 { continue }\n        if i > 7 { break }\n        total = total + i\n    }\n    let mut n = 5\n    while n > 0 { n = n - 1 }\n    return total + n\n}\n",
        )
        .expect("run");
        let Value::Int(t) = v else { panic!("int") };
        assert_eq!(t, 16); // 1+3+5+7 = 16
    }

    #[test]
    fn closures_capture_by_value_snapshot() {
        let v = run_src(
            "pub fn main() -> int {\n    let base = 100\n    let add = fn(x: int) = x + base\n    let r1 = add(5)\n    let base2 = add(1)\n    return r1 * 10 + base2\n}\n",
        )
        .expect("run");
        let Value::Int(t) = v else { panic!("int") };
        assert_eq!(t, 105 * 10 + 101);
    }

    // D-24 根修回归：嵌套 lambda 祖父捕获。修复前 lower_lambda 克隆 captures，
    // 内层 lambda 经 resolve_capture 登记进中间层的传递 upvalue 条目随克隆体
    // 丢弃 → 中间层 proto.upvalues 落空 → VM Y-I9004。
    #[test]
    fn nested_lambda_grandparent_capture() {
        // 内层 fn(y) = y + r 越过中间层直接捕获 fn(x) 外层的 r
        let v = run_src(
            "pub fn main() -> int {\n    let r = 7\n    let g = map([1, 2], fn(x: int) =\n        map([1, 2], fn(y: int) = y + r))\n    return g[1][1]\n}\n",
        )
        .expect("run");
        let Value::Int(t) = v else { panic!("int") };
        assert_eq!(t, 2 + 7);
    }

    #[test]
    fn nested_lambda_for_var_per_iteration_snapshot() {
        // for 循环变量作祖父捕获：每轮按值快照（决策 31），三轮互不串扰
        let v = run_src(
            "pub fn main() -> int {\n    let acc = 0\n    for r in range(0, 3) {\n        let row = map(range(0, 3), fn(x: int) =\n            map(range(0, 2), fn(y: int) = y + r))\n        acc = acc + row[2][1]\n    }\n    return acc\n}\n",
        )
        .expect("run");
        let Value::Int(t) = v else { panic!("int") };
        // row[2][1] = 1 + r，三轮 r=0/1/2 → 1+2+3 = 6
        assert_eq!(t, 6);
    }

    // R 线收尾（27 号 §3.2 strict 取键）：get/get_or 命中、缺键、缺省三面。
    #[test]
    fn map_get_hit_and_miss() {
        let v = run_src(
            "pub fn main() -> int? {\n    let m = [\"a\": 1, \"b\": 2]\n    let miss = get(m, \"z\")\n    if miss == nil {\n        return get(m, \"b\")\n    }\n    return miss\n}\n",
        )
        .expect("run");
        let Value::Int(t) = v else { panic!("int") };
        assert_eq!(t, 2); // 缺键走 nil 分支，命中返回 2
    }

    #[test]
    fn map_get_or_default() {
        let v = run_src(
            "pub fn main() -> int {\n    let m = [\"a\": 1]\n    return get_or(m, \"a\", 99) * 100 + get_or(m, \"zz\", 99)\n}\n",
        )
        .expect("run");
        let Value::Int(t) = v else { panic!("int") };
        assert_eq!(t, 199);
    }

    /// 类型闸（决策 55/27 §3.2）：`V?` 不得赋给裸 `V`——fail-soft 毒化在编译期拦。
    #[test]
    fn map_get_option_type_gate() {
        let src = "pub fn main() -> int {\n    let m = [\"a\": 1]\n    let x: int = get(m, \"a\")\n    return x\n}\n";
        let (diags, ok) = yoru_typeck::check_source(src, &yoru_typeck::Env::core());
        assert!(!ok, "int? must not assign to int");
        assert!(
            diags
                .iter()
                .any(|d| d.is_error() && d.message.contains("int?")),
            "expect int? mismatch diagnostic, got {diags:?}"
        );
    }

    #[test]
    fn match_and_try_propagation() {
        let v = run_src(
            "tag E = NotFound | BadInput(string)\n\nfn find(xs: [int], want: int) -> Result[int, E] {\n    for i in 0..len(xs) {\n        if xs[i] == want {\n            return Ok(i)\n        }\n    }\n    return Err(NotFound)\n}\n\npub fn main() -> Result<int, E> {\n    let a = try find([3, 1, 4, 1, 5], 4)\n    return match find([3, 1, 4, 1, 5], 9) {\n        Ok(v) -> Ok(v)\n        Err(NotFound) -> Ok(a * 10 + 1)\n        Err(other) -> Err(other)\n    }\n}\n",
        )
        .expect("run");
        // try 对 Ok 解包 → a = 2；find(...,9) = Err(NotFound) → Ok(21)
        let Value::Variant(var) = &v else {
            panic!("variant")
        };
        assert_eq!(var.variant, 0, "Ok");
        assert!(
            var.payload
                .as_ref()
                .expect("payload")
                .deep_eq(&Value::Int(21))
        );
    }

    #[test]
    fn struct_cow_field_update() {
        let v = run_src(
            "struct P { x: int, y: int }\n\npub fn main() -> int {\n    let a = P { x: 1, y: 2 }\n    let b = a\n    let c = P { ..b, x: 10 }\n    return a.x * 100 + b.x * 10 + c.x\n}\n",
        )
        .expect("run");
        let Value::Int(t) = v else { panic!("int") };
        assert_eq!(t, 100 + 10 + 10, "spread 更新不改原值（决策 13 COW）");
    }

    #[test]
    fn map_missing_key_is_nil() {
        let v = run_src(
            "pub fn main() -> int? {\n    let mut m: [string: int] = [:]\n    m[\"a\"] = 1\n    return m[\"b\"]\n}\n",
        )
        .expect("run");
        assert!(
            matches!(v, Value::Variant(ref var) if var.variant == 0) || matches!(v, Value::Nil)
        );
    }

    // ---- 决策 83 / 29 号 D2：注释穿透延续（F0） ----

    #[test]
    fn trailing_line_comment_keeps_expression_continuation() {
        // `a + // c\n2`：注释遮住的 + 仍延续语句（Go scanner 穿透语义）
        let v = run_src(
            "pub fn main() -> int {\n    let a = 1\n    let b = a + // c\n    2\n    return b\n}\n",
        )
        .expect("run");
        assert_eq!(v, Value::Int(3));
    }

    #[test]
    fn trailing_doc_comment_keeps_expression_continuation() {
        // `///` 同族（F0 前为既有潜伏缺陷：DocComment 不参与延续判定）
        let v = run_src(
            "pub fn main() -> int {\n    let a = 1\n    let b = a + /// c\n    2\n    return b\n}\n",
        )
        .expect("run");
        assert_eq!(v, Value::Int(3));
    }

    #[test]
    fn trailing_block_comment_keeps_expression_continuation() {
        let v = run_src(
            "pub fn main() -> int {\n    let a = 1\n    let b = a + /* c */\n    2\n    return b\n}\n",
        )
        .expect("run");
        assert_eq!(v, Value::Int(3));
    }

    #[test]
    fn inline_block_comment_in_expression() {
        let v = run_src("pub fn main() -> int {\n    let b = 1 /* mid */ + 2\n    return b\n}\n")
            .expect("run");
        assert_eq!(v, Value::Int(3));
    }

    #[test]
    fn comment_lines_around_statements() {
        let v = run_src(
            "// head\npub fn main() -> int {\n    // standalone\n    let a = 5 // tail\n    // between\n    return a\n}\n// tail of file\n",
        )
        .expect("run");
        assert_eq!(v, Value::Int(5));
    }

    // ---- 内建 ----

    // ---- 27 号 A3：format_value tag 表反查（归并 D-21） ----

    #[test]
    fn map_for_iter_yields_keys_vm() {
        // 决策 82 第一步：VM Cursor::Map 产出键快照（决策 31 同构）。
        let v = run_src(
            "pub fn main() -> int {\n    let mut m: [string: int] = [:]\n    m[\"b\"] = 2\n    m[\"a\"] = 1\n    let acc: string = \"\"\n    for k in m {\n        acc = acc + k\n    }\n    return len(acc)\n}\n",
        )
        .expect("run");
        assert_eq!(v, Value::Int(2)); // 两键各拼一次 → len 2（插入序 b,a）
    }

    #[test]
    fn opt_elem_list_lit_vm() {
        // [T?] 标注下 nil 元素运行期为 Nil（None）。
        let v = run_src(
            "pub fn main() -> int? {\n    let xs: [int?] = [1, nil]\n    return xs[1]\n}\n",
        )
        .expect("run");
        assert_eq!(v, Value::Nil);
    }

    #[test]
    fn format_value_resolves_tag_names() {
        let src = "tag MyErr = NotFound | Bad(string)\npub fn main() -> MyErr {\n    return Bad(\"x\")\n}\n";
        let mut vm = Vm::new();
        let v = vm.run(compile(src), None, vec![]).expect("run");
        // 载荷沿 display 语义（字符串不带引号，与 print 惯例一致；
        // repr 变体是已登记的 io.debug 设计债，06 号）。
        assert_eq!(vm.format_value(&v), "MyErr#1(x)");

        let src2 = "tag MyErr = NotFound | Bad(string)\npub fn main() -> MyErr {\n    return NotFound\n}\n";
        let mut vm2 = Vm::new();
        let v2 = vm2.run(compile(src2), None, vec![]).expect("run");
        assert_eq!(vm2.format_value(&v2), "MyErr#0");
    }

    #[test]
    fn raised_error_formats_with_tag_name() {
        // D-21：throw 传播到顶层的错误值此前显示 `<variant 0>`，无错误身份。
        let src = "tag MyErr = NotFound\nfn f() -> Result[int, MyErr] {\n    throw NotFound\n}\npub fn main() -> int {\n    return 0\n}\n";
        let mut vm = Vm::new();
        // 直接调 f 触发 raise：用 run 内 entry 覆盖（entry 参数指定函数名）。
        let err = vm.run(compile(src), Some("f"), vec![]).expect_err("raise");
        match err {
            VmError::Raised(v) => assert_eq!(vm.format_value(&v), "MyErr#0"),
            other => panic!("expect Raised, got {other:?}"),
        }
    }

    // ---- 26 号登记修复回归（D-06/07/08/09/17、D-05+20 表示统一）----

    #[test]
    fn field_assign_writes_back_slot() {
        // D-06：p.x = 9 曾静默 no-op（栈上副本被 Pop 丢弃）
        let v = run_src(
            "struct P { x: int, y: int }\n\npub fn main() -> int {\n    let mut p = P { x: 1, y: 2 }\n    p.x = 9\n    return p.x * 10 + p.y\n}\n",
        )
        .expect("run");
        assert_eq!(v, Value::Int(92));
    }

    #[test]
    fn field_compound_assign_operand_order() {
        // D-07：p.y += 3 曾操作数错位 Y-R9012
        let v = run_src(
            "struct P { x: int, y: int }\n\npub fn main() -> int {\n    let mut p = P { x: 1, y: 2 }\n    p.y += 3\n    return p.x * 10 + p.y\n}\n",
        )
        .expect("run");
        assert_eq!(v, Value::Int(15));
    }

    #[test]
    fn index_compound_assign_writes_back() {
        // D-08：xs[1] += 5 曾两临时互换 Y-R9013
        let v = run_src(
            "pub fn main() -> int {\n    let mut xs = [9, 2, 3]\n    xs[1] += 5\n    return xs[0] * 100 + xs[1] * 10 + xs[2]\n}\n",
        )
        .expect("run");
        assert_eq!(v, Value::Int(973));
    }

    #[test]
    fn chained_index_assign_writes_back_root() {
        // D-09：c[0][1] = 5 曾静默 no-op（内层 SetIndex 操作栈上副本）
        let v = run_src(
            "pub fn main() -> int {\n    let mut c = [[1, 2], [3, 4]]\n    c[0][1] = 5\n    return c[0][0] * 1000 + c[0][1] * 100 + c[1][0] * 10 + c[1][1]\n}\n",
        )
        .expect("run");
        assert_eq!(v, Value::Int(1534));
    }

    #[test]
    fn if_expression_yields_value() {
        // D-17：if 表达式分支值曾静默 nil（块尾表达式未留栈）
        let v = run_src(
            "pub fn main() -> int {\n    let y = if 2 > 1 { 7 } else { 3 }\n    return y\n}\n",
        )
        .expect("run");
        assert_eq!(v, Value::Int(7));
    }

    #[test]
    fn option_runtime_repr_is_nil_or_value() {
        // D-05：nil 收窄（决策 40）后直接算术——旧表示运行期仍是 Variant
        // D-20：收窄值进 Ok 不再泄漏 <variant>（try 解包后可直接算术）
        let v = run_src(
            "tag E = Bad\n\nfn half(s: string) -> Result[int, E] {\n    let n = parse_int(s)\n    if n == nil { throw Bad }\n    return Ok(n)\n}\n\npub fn main() -> int {\n    let a = parse_int(\"21\")\n    if a == nil { return -1 }\n    let b = try half(\"21\")\n    return a + b\n}\n",
        )
        .expect("run");
        assert_eq!(v, Value::Int(42));
    }

    // ---- 内建（续） ----

    #[test]
    fn builtins_and_print_output() {
        let mut vm = Vm::new();
        let v = vm
            .run(
                compile(
                    "pub fn main() -> int {\n    let xs = [5, 3, 9, 1]\n    let sorted = sort(xs)\n    print(\"sorted=${sorted[0]}${sorted[1]}${sorted[2]}${sorted[3]}\")\n    let total = xs |> map(fn(x) = x * 2) |> fold(0, fn(a, b) = a + b)\n    print(\"total=${total}\")\n    return len(xs)\n}\n",
                ),
                None,
                vec![],
            )
            .expect("run");
        assert_eq!(v, Value::Int(4));
        let out = String::from_utf8(vm.take_output()).expect("utf8");
        assert_eq!(out, "sorted=1359\ntotal=36\n");
        // 原列表不被 map/sort 改写（按值快照）
    }

    #[test]
    fn string_iteration_and_runes() {
        let mut inst = compile(
            "import string from \"std\"\npub fn main() -> int {\n    let rs = string.to_runes(\"你好\")\n    return len(rs)\n}\n",
        );
        demo::install_demo_host(&mut inst);
        let v = Vm::new().run(inst, None, vec![]).expect("run");
        let Value::Int(n) = v else { panic!("int") };
        assert_eq!(n, 2, "码点计数（决策 46）");
    }

    #[test]
    fn assert_failure_carries_source_offset() {
        let inst = compile("pub fn main() {\n    assert(1 == 2, \"boom\")\n}\n");
        let mut vm = Vm::new();
        let err = vm.run(inst, None, vec![]).unwrap_err();
        let VmError::Runtime(e) = err else {
            panic!("runtime")
        };
        assert_eq!(e.code, "Y-R9005");
        assert_eq!(e.message, "boom");
        assert!(e.src_off.is_some(), "debug 产物必须带源偏移（决策 70）");
    }

    // ---- 资源三限（决策 71）----

    #[test]
    fn instruction_budget_kills_infinite_loop() {
        let mut vm = Vm::new();
        vm.set_limits(Limits {
            max_instructions: 50_000,
            ..Limits::default()
        });
        let err = vm
            .run(
                compile("pub fn main() {\n    let mut i = 0\n    while true { i = i + 1 }\n}\n"),
                None,
                vec![],
            )
            .unwrap_err();
        assert!(matches!(err, VmError::Limit(LimitKind::Instructions)));
    }

    #[test]
    fn call_depth_limit() {
        let mut vm = Vm::new();
        vm.set_limits(Limits {
            max_call_depth: 32,
            ..Limits::default()
        });
        let err = vm
            .run(
                compile(
                    "fn rec(n: int) -> int {\n    return rec(n + 1)\n}\npub fn main() -> int {\n    return rec(0)\n}\n",
                ),
                None,
                vec![],
            )
            .unwrap_err();
        assert!(matches!(err, VmError::Limit(LimitKind::CallDepth)));
    }

    #[test]
    fn interrupt_hook_fires() {
        let mut vm = Vm::new();
        vm.set_limits(Limits {
            check_interval: 1,
            ..Limits::default()
        });
        vm.set_interrupt(Box::new(|_| true));
        let err = vm
            .run(
                compile("pub fn main() -> int {\n    return 1\n}\n"),
                None,
                vec![],
            )
            .unwrap_err();
        assert!(matches!(err, VmError::Interrupted));
    }

    // ---- 决策 72：观测 ----

    #[test]
    fn stats_snapshot_counts() {
        let mut vm = Vm::new();
        vm.run(
            compile("pub fn main() -> int {\n    let xs = [1, 2, 3]\n    return len(xs)\n}\n"),
            None,
            vec![],
        )
        .expect("run");
        let s = vm.stats();
        assert!(s.instructions > 0);
        assert!(s.heap_bytes > 0);
        assert!(s.calls >= 1, "len 内建调用计入 calls");
    }

    // ---- 22 号约束 6：debug/strip 差分一致 ----

    #[test]
    fn debug_and_strip_semantically_identical() {
        let src = "tag E = Bad(string)\nfn harm(n: int) -> Result[int, E] {\n    if n < 0 { return Err(Bad(\"neg\")) }\n    return Ok(n * n)\n}\npub fn main() -> string {\n    let a = try harm(9)\n    let b = try harm(0 - 3)\n    return \"${a}/${b}\"\n}\n";
        let full = compile(src);
        let main_idx = debuginfo::DebugInfo::from_module(full.module())
            .expect("debug")
            .proto_by_name("main", &full.module().strings)
            .expect("main");
        let mut stripped = compile(src);
        stripped.strip_debug(); // strip：仅删元数据
        let mut vm1 = Vm::new();
        let mut vm2 = Vm::new();
        let r1 = vm1.run(full, None, vec![]).expect("debug run");
        let r2 = vm2.run_at(stripped, main_idx, vec![]).expect("strip run");
        assert!(r1.deep_eq(&r2));
        assert_eq!(vm1.take_output(), vm2.take_output());
    }

    // ---- e2e：权威样例 summarize（demo 宿主 + Ctx）----

    #[test]
    fn authoritative_summarize_e2e() {
        let src = include_str!("../../yoru-syntax/examples/authoritative.yoru");
        let program = parse_source(src).expect("parse");
        let m = yoru_bytecode::lower(&program).expect("lower");
        yoru_bytecode::verify_module(&m).expect("verify");
        let mut inst = Instance::new(m);
        demo::install_demo_host(&mut inst);

        let module_strings = |sidx: u32| -> String {
            inst.module()
                .strings
                .get(sidx as usize)
                .cloned()
                .unwrap_or_default()
        };
        let find_fid = |name: &str| -> u32 {
            inst.module()
                .field_names
                .iter()
                .position(|sidx| module_strings(*sidx) == name)
                .map(|i| i as u32)
                .unwrap_or_else(|| panic!("field {name}"))
        };
        let size_fid = find_fid("size");
        let title_fid = find_fid("title");

        let mk_item = |size: i64, title: &str| {
            Value::Struct(Rc::new(RefCell::new(StructObj {
                struct_id: u32::MAX, // 宿主构造值：字段按 fid 自对齐
                fields: vec![
                    (size_fid, Value::Int(size)),
                    (title_fid, Value::Str(Rc::from(title))),
                ],
            })))
        };
        let items = Value::List(Rc::new(RefCell::new(vec![
            mk_item(2_000_000, "big"),
            mk_item(500, "small"),
        ])));

        let mut vm = Vm::new();
        let r = vm
            .run(inst, Some("summarize"), vec![demo::demo_ctx(), items])
            .expect("summarize runs");
        let Value::Variant(var) = &r else {
            panic!("Result variant")
        };
        // big 被 continue 过滤 → best=small → ctx.log("picked of 2")；
        // n = m["b"] 缺失键 → None → n == nil 成立 → Err(NotFound)（决策 55 语义）
        assert_eq!(var.variant, 1, "Err（n==nil 早返回）");
        let Value::Variant(notfound) = &**var.payload.as_ref().expect("Err payload") else {
            panic!("payload 应为变体")
        };
        assert_eq!(notfound.variant, 0, "NotFound = Err tag 首变体");
        let out = String::from_utf8(vm.take_output()).expect("utf8");
        assert!(out.contains("picked of 2"), "ctx.log 输出：{out}");
    }

    // ---- T2 硬化：堆限额 / 截止时刻 / 循环引用 ----

    #[test]
    fn heap_byte_limit_triggers() {
        let mut vm = Vm::new();
        vm.set_limits(Limits {
            max_heap_bytes: 32,
            ..Limits::default()
        });
        let err = vm
            .run(
                compile("pub fn main() {\n    let xs = [1, 2, 3, 4]\n}\n"),
                None,
                vec![],
            )
            .unwrap_err();
        assert!(matches!(err, VmError::Limit(LimitKind::HeapBytes)));
    }

    #[test]
    fn run_scoped_budgets_reset_between_runs() {
        // 决策 79：同一 Vm 连续 run，三限预算按 run 重置；单调累计入 total_*。
        // 回归意义：修复前指令/堆计数跨 run 累积，长驻宿主（REPL/服务器）
        // 会被历史消耗饿死（第二次 run 即 Limit）。
        let src = "pub fn main() -> int {\n    let xs = [1, 2, 3, 4]\n    return len(xs)\n}\n";
        let mut vm = Vm::new();
        vm.run(compile(src), None, vec![]).expect("first run");
        let first = vm.stats();
        assert!(first.instructions > 0 && first.heap_bytes > 0);
        // 限额设为「恰够一次 run」：跨 run 累积语义下第二次必挂
        vm.set_limits(Limits {
            max_instructions: first.instructions + 5,
            max_heap_bytes: first.heap_bytes + 5,
            ..Limits::default()
        });
        vm.run(compile(src), None, vec![])
            .expect("second run must not be starved by history");
        let second = vm.stats();
        assert_eq!(second.instructions, first.instructions, "run 作用域重置");
        assert_eq!(second.heap_bytes, first.heap_bytes, "run 作用域重置");
        assert!(
            second.total_instructions > second.instructions,
            "单调累计保留"
        );
        assert!(second.total_allocated > second.heap_bytes, "单调累计保留");
    }

    #[test]
    fn deadline_triggers() {
        let mut vm = Vm::new();
        vm.set_deadline(Instant::now() - std::time::Duration::from_secs(1));
        let err = vm
            .run(
                compile("pub fn main() {\n    let mut i = 0\n    while true { i = i + 1 }\n}\n"),
                None,
                vec![],
            )
            .unwrap_err();
        assert!(matches!(err, VmError::Limit(LimitKind::Deadline)));
    }

    #[test]
    fn cyclic_values_never_recurse_forever() {
        let mk = || {
            let l = Rc::new(RefCell::new(Vec::new()));
            l.borrow_mut().push(Value::List(l.clone()));
            l
        };
        let a = mk();
        let c = mk();
        // 两条不同的循环链：深度截断返回 false，不爆栈
        assert!(!Value::List(a.clone()).deep_eq(&Value::List(c.clone())));
        let _ = Value::List(a.clone()).display();
        // 同一循环链：Rc 指针短路
        assert!(Value::List(a.clone()).deep_eq(&Value::List(a.clone())));
    }

    // ---- S 线：长期运行稳定性 / 停机界 / 极限压测（25 号 §4-c）----

    #[test]
    fn long_run_deterministic_checksum() {
        // 长跑稳定性双契约：①百万级指令不间断运行无 panic/无状态劣化；
        // ②确定性——独立 VM 同输入逐位同结果且指令数一致（调度透明）。
        let src = "pub fn main() -> int {\n    let mut h = 0\n    let mut i = 0\n    while i < 200_000 {\n        h = (h * 31 + i % 97) % 1_000_000_007\n        i = i + 1\n    }\n    return h\n}\n";
        let mut vm1 = Vm::new();
        let mut vm2 = Vm::new();
        let v1 = vm1.run(compile(src), None, vec![]).expect("run 1");
        let v2 = vm2.run(compile(src), None, vec![]).expect("run 2");
        assert!(v1.deep_eq(&v2), "长跑结果逐位一致");
        let s1 = vm1.stats();
        let s2 = vm2.stats();
        assert_eq!(s1.instructions, s2.instructions, "确定性：指令数一致");
        assert!(
            s1.instructions > 1_000_000,
            "压测规模须达百万级指令，实际 {}",
            s1.instructions
        );
    }

    #[test]
    fn interrupt_latency_bounded() {
        // 停机保证的量化界：钩子返回 true 后立即终止——停止点距最后一次
        // 检查 ≤ check_interval 条指令（倒数计数器实现的触发点契约）。
        let interval: u64 = 64;
        let fires_to_stop: u32 = 8;
        // Rc<Cell>：Cell::clone 是值拷贝而非共享（探针实证 fire#8 instr=512
        // 而 stopped_at 仍 0）——共享观测必须走 Rc。
        let stopped_at = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let seen = stopped_at.clone();
        let mut vm = Vm::new();
        vm.set_limits(Limits {
            check_interval: interval,
            ..Limits::default()
        });
        let mut fires = 0u32;
        vm.set_interrupt(Box::new(move |s: &Stats| {
            fires += 1;
            seen.set(s.instructions);
            fires >= fires_to_stop
        }));
        let err = vm
            .run(
                compile("pub fn main() {\n    let mut i = 0\n    while true { i = i + 1 }\n}\n"),
                None,
                vec![],
            )
            .unwrap_err();
        assert!(matches!(err, VmError::Interrupted));
        // 检查点每 interval 条出现一次：第 8 次触发时指令数上界 = 8*interval + 1
        assert!(
            stopped_at.get() <= fires_to_stop as u64 * interval + 1,
            "停机时延越界：stopped at {} (bound {})",
            stopped_at.get(),
            fires_to_stop as u64 * interval + 1
        );
        assert!(stopped_at.get() > 0, "必须真实执行后停机");
    }

    #[test]
    fn arithmetic_extreme_edge_matrix() {
        // i64 极值矩阵：全部结构化 Y-R9001，绝不 panic（checked 语义面全覆盖：
        // Add/Sub/Mul/Div/Mod/Neg 六算子的 MIN 边界；MIN/-1 与 MIN%-1 是
        // debug 构建的经典 panic 面，必须由 checked_* 拦截为运行时错误）。
        let cases: [(&str, &str); 5] = [
            ("sub: MIN-1", "return 0 - 9223372036854775807 - 2"),
            (
                "mul: MIN*-1",
                "return (0 - 9223372036854775807 - 1) * (0 - 1)",
            ),
            (
                "div: MIN/-1",
                "return (0 - 9223372036854775807 - 1) / (0 - 1)",
            ),
            (
                "mod: MIN%-1",
                "return (0 - 9223372036854775807 - 1) % (0 - 1)",
            ),
            (
                "neg: -MIN",
                "let x = 0 - 9223372036854775807 - 1\n    return -x",
            ),
        ];
        for (label, body) in cases {
            let src = format!("pub fn main() -> int {{\n    {body}\n}}\n");
            let err = run_src(&src).unwrap_err();
            let VmError::Runtime(e) = err else {
                panic!("{label}: expected runtime error")
            };
            assert_eq!(e.code, "Y-R9001", "{label}: {}", e.message);
        }
    }

    #[test]
    fn degenerate_programs_fail_structured() {
        // 退化输入（空源 / 仅注释 / 缺 main）：全链结构化 Y-R9000，不 panic
        //——「跑别人给的脚本」的第一道冲击面。
        for src in [
            "",
            "// only a comment\n",
            "fn helper() -> int {\n    return 1\n}\n",
        ] {
            let err = run_src(src).unwrap_err();
            let VmError::Runtime(e) = err else {
                panic!("degenerate {src:?}: expected structured error")
            };
            assert_eq!(e.code, "Y-R9000", "degenerate {src:?}: {}", e.message);
        }
    }

    #[test]
    fn unbounded_string_growth_hits_heap_limit() {
        // 循环内持续分配：堆计账逐次检查（决策 72），触限即优雅停机。
        // 指令预算同时上保险：若堆检查失效，以 Instructions 而非死循环告终。
        let mut vm = Vm::new();
        vm.set_limits(Limits {
            max_heap_bytes: 1024 * 1024,
            max_instructions: 2_000_000,
            ..Limits::default()
        });
        let err = vm
            .run(
                compile(
                    "pub fn main() -> string {\n    let mut s = \"x\"\n    let mut i = 0\n    while i < 100_000 {\n        s = s + \"0123456789012345678901234567890123456789\"\n        i = i + 1\n    }\n    return s\n}\n",
                ),
                None,
                vec![],
            )
            .unwrap_err();
        assert!(
            matches!(err, VmError::Limit(LimitKind::HeapBytes)),
            "增长分配必须由堆限拦截，实际 {err:?}"
        );
    }

    #[test]
    fn const_fold_matches_runtime_semantics() {
        // 决策 44 一致性：同一表达式经 const 折叠与运行时求值结果必须逐位
        // 一致（floor 语义单一事实源在 VM——fold_const 用 checked_div_euclid /
        // checked_rem_euclid 对齐）。修复前 fold 用截断 checked_div/checked_rem，
        // 负被除数时 const 声明与 let 赋值结果分叉（-2 vs -3）。
        let src = "const CFOLD: int = (0 - 7) / 3\nconst MFOLD: int = (0 - 7) % 3\npub fn main() -> string {\n    let d = (0 - 7) / 3\n    let m = (0 - 7) % 3\n    return \"${CFOLD == d}/${MFOLD == m}\"\n}\n";
        let v = run_src(src).expect("run");
        let Value::Str(s) = v else { panic!("string") };
        assert_eq!(&*s, "true/true", "const 折叠必须与运行时语义一致");
    }

    // ---- 负面：strip 模块 assert 为 no-op（决策 70）----

    #[test]
    fn strip_assert_is_noop() {
        let src = "pub fn main() -> int {\n    assert(1 == 2)\n    return 7\n}\n";
        let mut inst = compile(src);
        let main_idx = debuginfo::DebugInfo::from_module(inst.module())
            .expect("debug")
            .proto_by_name("main", &inst.module().strings)
            .expect("main");
        inst.strip_debug();
        let mut vm = Vm::new();
        let v = vm.run_at(inst, main_idx, vec![]).expect("strip run");
        assert_eq!(v, Value::Int(7));
    }

    // ---- 阶段 4：宿主桥（register_host_fn / 决策 76 / panic 边界）----

    use yoru_typeck::{Env, HostSig, Ty};

    fn int_sig(n: usize) -> HostSig {
        HostSig {
            params: vec![Ty::Int; n],
            ret: Ty::Int,
        }
    }

    /// 全管线装配：typeck（含宿主签名面）→ lower（宿主名单）→ 验证 → Instance
    fn compile_with_hosts(src: &str, hosts: &[(&str, HostSig)]) -> Instance {
        let program = parse_source(src).expect("parse");
        let env = Env::core().with_host_fns(hosts.iter().map(|(n, s)| (n.to_string(), s.clone())));
        let ds = yoru_typeck::check_program(&program, &env);
        assert!(ds.is_empty(), "typeck: {ds:#?}");
        let names: Vec<String> = hosts.iter().map(|(n, _)| n.to_string()).collect();
        let m = yoru_bytecode::lower_with_hosts(&program, &names).expect("lower");
        yoru_bytecode::verify_module(&m).expect("verify");
        Instance::new(m)
    }

    #[test]
    fn host_fn_roundtrip() {
        let inst = compile_with_hosts(
            "pub fn main() -> int {\n    return double(21)\n}\n",
            &[("double", int_sig(1))],
        );
        let mut vm = Vm::new();
        vm.register_host_fn("double", |_vm, args| match args.first() {
            Some(Value::Int(n)) => Ok(Value::Int(n * 2)),
            other => unreachable!("typeck guarantees int, got {other:?}"),
        });
        let v = vm.run(inst, None, vec![]).expect("run");
        assert_eq!(v, Value::Int(42));
    }

    #[test]
    fn host_panic_bounded() {
        // 退出标准：宿主 panic 不穿透 VM 边界 → 结构化 Y-H9001
        let inst = compile_with_hosts(
            "pub fn main() -> int {\n    return boom(1)\n}\n",
            &[("boom", int_sig(1))],
        );
        let mut vm = Vm::new();
        vm.register_host_fn(
            "boom",
            |_vm: &mut Vm, _args: &[Value]| -> Result<Value, VmError> {
                panic!("host bug: disk on fire");
            },
        );
        let err = vm.run(inst, None, vec![]).unwrap_err();
        match err {
            VmError::Runtime(r) => {
                assert_eq!(r.code, "Y-H9001");
                assert!(r.message.contains("boom"), "{}", r.message);
                assert!(r.message.contains("disk on fire"), "{}", r.message);
            }
            other => panic!("expected runtime error, got {other:?}"),
        }
    }

    #[test]
    fn host_unregistered_fails_closed() {
        // 编译产物合法（lower 知道名单）但运行时未注入 → Y-H9000 fail closed
        //（覆盖未经 typeck 的直载字节码路径；能力未注入 = 编译期 Y-T0001 主防）
        let program =
            parse_source("pub fn main() -> int {\n    return ghost(1)\n}\n").expect("parse");
        let m = yoru_bytecode::lower_with_hosts(&program, &["ghost".to_string()]).expect("lower");
        yoru_bytecode::verify_module(&m).expect("verify");
        let err = Vm::new().run(Instance::new(m), None, vec![]).unwrap_err();
        match err {
            VmError::Runtime(r) => {
                assert_eq!(r.code, "Y-H9000");
                assert!(r.message.contains("ghost"), "{}", r.message);
            }
            other => panic!("expected runtime error, got {other:?}"),
        }
    }

    #[test]
    fn callhost_opcode_direct_encoding() {
        // 决策 76：CallHost 0x64 直接编码路径——`Const(Str(名))` + CallHost argc
        // 与 Call 同构（手工构造模块，绕过 lower 的 Call 发射）
        let m = Lbc0Module {
            version: yoru_bytecode::VERSION,
            flags: 0,
            imports: vec![],
            tags: vec![],
            structs: vec![],
            field_names: vec![],
            consts: vec![Const::Int(7), Const::Str(0)],
            protos: vec![yoru_bytecode::Proto {
                params: 0,
                locals: 0,
                max_stack: 2,
                code: vec![
                    Instr::with_arg(Opcode::Const, 0),
                    Instr::with_arg(Opcode::Const, 1),
                    Instr::with_arg(Opcode::CallHost, 1),
                    Instr::new(Opcode::Return),
                ],
                upvalues: vec![],
                const_refs: vec![0, 1],
            }],
            strings: vec!["triple".to_string()],
            debug: None,
        };
        yoru_bytecode::verify_module(&m).expect("verify");
        let mut vm = Vm::new();
        vm.register_host_fn("triple", |_vm, args| match args.first() {
            Some(Value::Int(n)) => Ok(Value::Int(n * 3)),
            other => unreachable!("CallHost passes 1 arg, got {other:?}"),
        });
        let v = vm.run_at(Instance::new(m), 0, vec![]).expect("run");
        assert_eq!(v, Value::Int(21));
    }

    #[test]
    fn unknown_name_runtime_is_host_miss() {
        // 内建与宿主分派收束：未知名走宿主注册表（原 Y-R9021 语义并入 Y-H9000）
        let program =
            parse_source("pub fn main() -> int {\n    return nope(1)\n}\n").expect("parse");
        let m = yoru_bytecode::lower_with_hosts(&program, &["nope".to_string()]).expect("lower");
        let err = Vm::new().run(Instance::new(m), None, vec![]).unwrap_err();
        match err {
            VmError::Runtime(r) => assert_eq!(r.code, "Y-H9000"),
            other => panic!("expected runtime error, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    fn run_src(src: &str) -> Result<Value, VmError> {
        let program = yoru_syntax::parse_source(src).expect("parse");
        let m = yoru_bytecode::lower(&program).expect("lower");
        yoru_bytecode::verify_module(&m).expect("verify");
        Vm::new().run(Instance::new(m), None, vec![])
    }

    /// 决策 77 对账：注册表每个 bare 名必须有 VM 分派臂——
    /// 空参分发不得落入 Y-H9000（未知名 = 宿主通道）。
    #[test]
    fn registry_dispatch_reconciled() {
        let mut vm = Vm::new();
        for name in yoru_stdlib::builtin_names() {
            match vm.dispatch_builtin(name, &[]) {
                Ok(_) => {}
                Err(VmError::Runtime(e)) => assert_ne!(
                    e.code, "Y-H9000",
                    "内建 `{name}` 无 VM 分派臂（落入宿主未知名）——注册表与 VM 漂移"
                ),
                Err(other) => panic!("内建 `{name}` 分发异常：{other}"),
            }
        }
    }

    /// 模块限定调用 e2e（决策 77）：`string.to_runes` 在 lower 归一为 bare 名，
    /// 无宿主注入亦可运行——修复此前 LoadImport(Nil)+GetField 断链。
    #[test]
    fn module_qualified_call_runs_without_host() {
        let src = "import string from \"std\"\n\npub fn main() -> int {\n    let rs = string.to_runes(\"hello\")\n    return len(rs)\n}\n";
        let v = run_src(src).expect("module-qualified call must run");
        let Value::Int(n) = v else { panic!("int") };
        assert_eq!(n, 5);
    }

    /// D1 值位归一 e2e（决策 78）：`map(xs, math.floor)` 在 lower 值位归一为
    /// bare 名 Str 值，VM 调用期按名分派——与裸名 `map(xs, floor)` 同一条通道。
    #[test]
    fn module_qualified_builtin_as_value_runs() {
        let src = concat!(
            "import math from \"std\"\n\n",
            "pub fn main() -> int {\n",
            "    let fs = map([2.7, 1.2, 3.9], math.floor)\n",
            "    return fs[0] * 100 + fs[1] * 10 + fs[2]\n",
            "}\n",
        );
        let v = run_src(src).expect("value-position module builtin must run");
        let Value::Int(n) = v else { panic!("int") };
        assert_eq!(n, 213); // floor: 2, 1, 3
    }

    /// print 变参面（决策 77）：任意值可打印，输出经 take_output 取走。
    #[test]
    fn print_variadic_any_values() {
        let mut vm = Vm::new();
        let out = vm
            .dispatch_builtin("print", &[Value::Int(42), Value::Bool(true)])
            .expect("print");
        assert_eq!(out, Value::Nil);
        assert_eq!(vm.take_output(), b"42 true\n");
    }

    /// 新收口的模块内建（决策 77）：sqrt/floor/ceil/upper/parse/stringify。
    #[test]
    fn module_builtins_behavior() {
        let mut vm = Vm::new();
        let f = |x: f64| Value::Float(x);
        assert_eq!(
            vm.dispatch_builtin("sqrt", &[f(9.0)]).unwrap(),
            Value::Float(3.0)
        );
        assert_eq!(
            vm.dispatch_builtin("floor", &[f(2.7)]).unwrap(),
            Value::Int(2)
        );
        assert_eq!(
            vm.dispatch_builtin("ceil", &[f(2.1)]).unwrap(),
            Value::Int(3)
        );
        assert!(vm.dispatch_builtin("floor", &[Value::Int(2)]).is_err());
        assert!(vm.dispatch_builtin("floor", &[f(f64::NAN)]).is_err());
        assert_eq!(
            vm.dispatch_builtin("upper", &[Value::Str(Rc::from("hi"))])
                .unwrap(),
            Value::Str(Rc::from("HI"))
        );
        // json.parse/stringify：T1 不透明透传（决策 28）
        let j = vm
            .dispatch_builtin("parse", &[Value::Str(Rc::from("{\"a\":1}"))])
            .unwrap();
        assert_eq!(
            vm.dispatch_builtin("stringify", &[j]).unwrap(),
            Value::Str(Rc::from("{\"a\":1}"))
        );
        assert!(vm.dispatch_builtin("stringify", &[Value::Int(1)]).is_err());
    }
}

// ── P0a③ 融合指令回归（25 号 §A 直方图驱动批次） ──

#[cfg(test)]
mod fused_tests {
    use super::*;
    use yoru_syntax::parse_source;

    fn compile(src: &str) -> Instance {
        let program = parse_source(src).expect("parse");
        let m = yoru_bytecode::lower(&program).expect("lower");
        yoru_bytecode::verify_module(&m).expect("verify");
        Instance::new(m)
    }

    fn run_capture(src: &str) -> Result<String, VmError> {
        let inst = compile(src);
        let mut vm = Vm::new();
        let r = vm.run(inst, None, vec![])?;
        let _ = r;
        Ok(String::from_utf8_lossy(&vm.take_output()).into_owned())
    }

    /// 融合条件跳转 + GetIndexLL 的语义锚：while + if-else + 序贯赋值
    /// （08_physics 回归的最小形态——Nil;Pop 窥孔需保守跳过跳转目标上的对）。
    #[test]
    fn fused_jump_and_index_semantics() {
        let src = r#"
fn main() -> int {
    let mut i = 0
    while i < 3 {
        if true {
            i = i + 1
        } else {
            i = i + 2
        }
        i = i + 1
    }
    print(i)
    return 0
}
"#;
        assert_eq!(
            run_capture(src).expect("run"),
            "4
"
        );
    }

    /// if 无 else 臂补位 Nil 是跳转目标——窥孔必须保守保留（删除错位实证）。
    #[test]
    fn peephole_preserves_jump_target_nil() {
        let src = r#"
fn main() -> int {
    let mut n = 0
    while n < 3 {
        if n == 1 {
            print("one")
        }
        n = n + 1
    }
    print(n)
    return 0
}
"#;
        assert_eq!(
            run_capture(src).expect("run"),
            "one
3
"
        );
    }

    /// GetIndexLL 融合路径的下标边界语义与独立 GetIndex 一致（Y-R9013）。
    #[test]
    fn getindex_ll_bounds_match_getindex() {
        let src = r#"
fn main() -> int {
    let xs = [1, 2, 3]
    let mut i = 5
    print(xs[i])
    return 0
}
"#;
        let err = run_capture(src).unwrap_err();
        let VmError::Runtime(e) = err else {
            panic!("runtime error expected");
        };
        assert_eq!(e.code, "Y-R9013");
    }
}
