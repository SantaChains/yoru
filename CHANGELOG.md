# Changelog

本文件按 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 格式维护。
**面向用户写影响，不罗列内部改动**（决策 63）；每次变更同步更新，不发布时补写。

## [Unreleased]

### Fixed
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
