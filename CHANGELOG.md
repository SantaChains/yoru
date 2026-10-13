# Changelog

本文件按 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 格式维护。
**面向用户写影响，不罗列内部改动**（决策 63）；每次变更同步更新，不发布时补写。

## [Unreleased]

### Added
- **yoru-fuzz crate（2026-10-13，V 线 V0 crash-fuzz，30 号 §6.1）**：全链 panic-free 契约机械化验证——`lex → parse → typeck → lower → run(有限步)` 五阶段 catch_unwind，token 段级语法变异（字典注入/删除/复制/换行扰动，强度分布化）+ 深度守卫 + 确定性 RNG（`(seed, case_id)` 可重放）。run 有限步由 VM 资源三限承接（零 VM 扩展）。验收：10 万 case panic-free（62s）；smoke 800 case 常驻回归。libFuzzer 不支持 Windows，自研 harness 可薄包装为未来 cargo-fuzz target。
- **`get` / `get_or` 内建（2026-10-13，28 号 R 线，27 号 §3.2 strict 取键）**：`get(m, k)` 命中返回值、缺键返回 None（`V?`），`get_or(m, k, d)` 缺键返回缺省值——缺键处理惯用法断层（get 侧）关闭。`map.get` 拼写与列表 map 函数撞名（决策 77 单一命名空间），按 keys/values 先例取裸名。类型闸实证：`int?` 赋 `int` 编译期拦截、键类型泛型冲突拦截。
- **`yoru fmt` CLI（2026-10-13，F 线 F2）**：gofmt 风格三形态——默认 stdout（管道组合）、`-w` 原地写入、`--check` 校验（有差异 exit 1，CI 可直接挂）。语义不变性以最强形态验收：examples 11 篇格式化前后 `yoru run` 输出逐位一致（diff 执行结果而非 diff 文本）；解析错误文件诚实拒绝（fmt 只对合法文件负责）。
- **yoru-fmt crate（2026-10-13，F 线 F1 核心，29 号裁决架构）**：token 空白重写器 + AST 区间指导布局 + span-blind 结构投影打印器 + `verify` 双断言（token 序列保真 + 结构投影保真 + 幂等）。注释/字符串/字面量经 span 逐字节保真；块体逐行缩进、struct_decl 字段与 match 臂逐行、行注释强制行尾、空行保留坍缩。三形态 spike + examples 11 篇全量双断言通过。CLI `yoru fmt` 属 F2。
- **30 号战略定夺（2026-10-13，文档）**：主轴转工具链（F→R→D→L），语言层只清 Critical 实现债穿插推进；D-03（struct 算术 typeck 放行→VM 崩）定夺 **fail-fast** 方向；新登记 **V 线语义差分测试基建**（cargo-fuzz crash-fuzz + 双模式对拍）。设计文档侧更新，无代码变更。

### Fixed
- **struct 算术 fail-fast（2026-10-13，D-03 Critical，30 号定夺）**：已 `impl Add/Sub/Mul/Div` 的 struct 算术不再「typeck 放行 → 运行期 Y-R9007 崩」——协议算术执行管线未接入（随泛型批次）前，`check_arith` 对 Named 同型算术一律编译期拒绝（Y-T0022），诊断区分「已 impl 但管线未实现」与「未 impl」两情形并给出自由函数建议。strict 健全承诺（静态通过则运行期成立）恢复完整。
- **lower 内建名单漂移根修（2026-10-13，get/get_or 落地时实证）**：`BUILTIN_FNS` 手写 frozen 镜像（注释声称有 `registry_mirror_locked` 对账测试，实际不存在）与注册表脱节——新增内建裸名调用撞 Y-T0913。改为从 `yoru-stdlib` 注册表派生（LazyLock HashSet，单一事实源构造保证一致），对账测试不再需要。
- **嵌套 lambda 祖父捕获崩溃（2026-10-13，D-24 Critical 根修）**：内层 lambda 捕获非直接父级的变量不再报 `Y-I9004`——根因是 `lower_lambda` 以 clone 继承捕获层，内层经 resolve_capture 登记进中间层的传递 upvalue 条目随克隆体在恢复外层状态时丢失（中间层 `proto.upvalues` 落空）；改 move 语义后穿透捕获正确。for 循环变量作祖父捕获时每轮按值快照（决策 31）。回归测试 +2，examples 11/11，全仓 160 测试全绿。
- **parser `FnDecl.span` 从 `fn` 关键字起**（2026-10-13，F1 配套 API 缺口修正）：此前从函数名起，与 `ProtocolFn`（从 `fn` 起）不一致；诊断定位顺带覆盖签名头。全量测试回归通过。
- **决策 83（2026-10-12，F 线 formatter 前置 F0）**：注释进入词法流——`//`/`/* */`/`///` 全部词法可见（此前普通注释被静默丢弃，examples 70+ 行注释词法不可见）；换行延续判定对注释**穿透**，行尾注释不再截断延续表达式（`a + // c\nb` 与 `a + /// doc\nb` 此后者为既有潜伏解析错误，现正确求值）。注释文本经 span 逐字保留，为 `yoru fmt`（F1）与 LSP hover 提供地基。138 测试全绿。

### Fixed
- **决策 81（2026-10-12，27 号 §5 定夺落地）**：① `rune as int` / `int as rune` 不再被 Y-T0011 误拦——rune 即 distinct int 内建等价，码点域检查（越界 fail-loud）在运行期保留；② 错误变体身份可见：throw 未捕获的错误与 print 输出中的 tag 值显示 `Tag名#变体序号`（如 `MyErr#0`），不再是 `<variant 0>`。125 测试全绿，11 示例与 sort 基准回归通过。
- **26 号登记修复批次（2026-10-12 对抗审核后落地，8 项）**：① struct 字段简单/复合赋值与链式索引赋值不再静默 no-op 或崩溃（D-06/07/08/09——lower 统一链式回写：根副本→逐层下降→内层变异→逐层上升→根槽位回写）；② if 表达式分支值不再静默 nil（D-17，块尾值语义落地）；③ 插值 `${o!}` 以 `!` 结尾不再死自旋至 Y-S9502（D-02，子解析器补 Eof 哨兵）；④ nil 收窄分支内 `o!` 不再被 Y-T0004 误拦（D-04，恒等解包）；⑤ `T?` 运行期表示统一为 `Nil | T`（D-05/D-20——收窄值进 Ok/返回值不再泄漏 `<variant N>`，spec 19 无 Some/None 匹配承诺故无观测破坏面）。回归测试 +8（VM×6/syntax×1/typeck×1），122 测试全绿；11 示例与 sort 基准全量回归通过。审核裁决与分歧（D-01 存疑/D-11 与决策 25 冲突/D-22 方向修正）见 26 号 §5。
- 局部数组下标赋值不再触发整表拷贝（决策 80 `SetIndexLocal` 快路径）——插入排序基线实测从不可完成（堆限额 0.15s 触顶）到 0.7s 净执行。

### Added
- 性能基线 v0（benchmarks/sort/，插入排序 × LuaJIT 对照，方法论与 harness 入库）。
- **游戏数学示例套件（2026-10-12，examples/math/，11 篇全 `yoru run` 通过）**：线性代数（Vec2/Vec3/Mat2、鞋带面积/质心、三角函数 Horner 近似）、计算机图形学（透视投影管线、距离场软件光栅器）、游戏数学（clamp/lerp/smoothstep/LCG/状态机）、物理（显式 vs 辛 Euler 能量对比、定步长累加器）、计算几何（orient/线段相交/PIP）、语法总览（distinct/位运算/map/string/rune/Result-try/json/解析/切片）。两条宏观叙事主线（渲染管线 + 交互仿真）共享向量代数与高性能近似方法论；每篇自包含（CLI 无用户模块 import）。配套设计文档 **26-示例套件实测缺陷与规格落差登记.md**（D-01..D-26，26 条实证缺陷 + 根因 `文件:行` + 严重级 + workaround + 修复方向）。
- 阶段 0-4 交付：解析（含防爆栈三重闸 Y-S9501/Y-S9502）、类型检查、LBC0 字节码、T1 VM（资源三限 + 中断 + 观测）、CLI `yoru check/run`、宿主自由函数桥（`register_host_fn`，宿主 panic 边界 Y-H9001）。
- 工程卫生套装：双许可证、SECURITY、CONTRIBUTING、CI（含 MSRV 验证 job）。
- stdlib 内建注册表统一收口（决策 77）：26 条内建签名由 `yoru_stdlib::BUILTINS` 单一事实源驱动，typeck/lower/VM 三面对账；`math.sqrt/floor/ceil`、`string.upper/to_runes`、`json.parse/stringify`、`parse_int/parse_float`、变参 `print`、`assert` 可选消息参数、`for i in range(n)` 迭代全部可用。
- **嵌入示例与嵌入面硬化（2026-10-12，外部消费者构建实证）**：`cargo run -p yoru-cli --example embed_host`——完整嵌入管线可运行范例（typeck 签名面 `Env::with_host_fns` + lower 宿主名单 `lower_with_hosts` + VM 闭包 `register_host_fn` + Rust→脚本反调 + 双闸口语义）。`register_host_fn` 现接受裸闭包（`move` 捕获宿主状态，无需 `Rc::new` 包装）；`Value`/`VmError`/`HostFn` 等在 `yoru_vm` crate 根可直接导入。完整嵌入管线三处配对：**typeck 签名面 + lower 宿主名单 + VM 闭包**，缺一不可（未注入名字 = 编译期 Y-T0001；有签名无闭包 = 运行期 Y-H9000 fail-closed）。

### Changed
- **资源三限改 run 作用域信封（决策 79）**：`Vm::stats()` 的 `instructions`/`calls`/`max_call_depth`/`heap_bytes` 现为单次 `run` 作用域（`load` 时重置）——此前为 Vm 生命周期累计，长驻宿主（同一 Vm 反复 run：REPL/服务器）会被历史消耗饿死；新增 `total_instructions`/`total_allocated` 单调累计字段（观测用，不参与限额）。run 内累计仍为保守代理（无 GC 下 peak live ≤ cumulative），非精确 live 记账。
- **Rust 工具链现代化（2026-10-12，用户裁决）**：edition 2021→2024 + MSRV 1.82→1.85（= edition 2024 语言下限）。两处 edition 2024 析构时序变化已显式绑定消除（`yoru-vm` call_host、`yoru-cli` run）；MSRV 声明经 1.85.0 实测 107/107 全绿验证（旧 1.82 声明从未验证）；CI msrv job 同步 1.85.0。开发机仅保留 stable 1.99.0 + nightly 两套工具链。
- **诊断 JSON 边界契约**：`Diagnostic::to_json` 返回 `String`（JSON 文本）而非 `serde_json::Value`——对齐 rustc `--error-format=json` / gopls / clangd 的行业契约，serde_json 退出 `yoru-typeck` 生产依赖（仅测试 dev-deps）；schema v1 内容不变（快照测试锁定）。

### Fixed
- `string.upper` 等模块函数被 Y-T0007 误拦（typeck 面缺员）；`print` 签名面与 VM 实际行为不符；`assert` 第二参数被当必选；`range(n)` 在 for 循环中被 Y-T0002 误拦（VM 早已支持 Range 迭代）。
- **模块限定内建名作值（决策 78）**：`map(xs, math.floor)` 此前级联 4 条误诊（「模块不能作为值」误导根因），现与裸名 `map(xs, floor)` 完全同构；std 模块字段缺失在值位发单条 Y-T0007。import 语义显式契约化为平坦全球名面模型（内建裸名恒可用 + import = 限定拼写开关，结构先例 = GDScript @GlobalScope，W-T0003 卫生提醒自洽）。
- **诊断重复发射**：同一错误曾以同 code + 同 span + 同 message 双发（如未 import 时 `math.floor` 双 Y-T0001、`[any]` 双 Y-T0007）——Field 分支探针改走无副作用 `lookup_opt`，收集点增设精确重复去重（rustc 级联抑制同款）。软类型 `Num/Coll/Any` 不可用户书写由测试锚定。

[Unreleased]: https://github.com/yoru-lang/yoru/compare/HEAD
