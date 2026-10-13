# Changelog

本文件按 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 格式维护。
**面向用户写影响，不罗列内部改动**（决策 63）；每次变更同步更新，不发布时补写。

## [Unreleased]

### Added
- **泛型函数与数据类型（2026-10-13，30 号序 8 泛型批次）**：`fn f[T, U](xs: [T]) -> U` 泛型函数 call-site 实例化——声明期 `T` 解析为类型变量，调用位按实参 unify 绑定（`first([1,2])` → int、`first(["a"])` → string，同函数多实例化零代码膨胀：统一 Value 表示下单份字节码承载全部实例）；lambda 实参参与推断（`apply2(7, fn(n) = "s")` → U=string）；返回类型含未绑定泛型 → 结构化 Y-T0018。泛型 struct/tag 变体构造按载荷反推类型实参（`Boxed(P2<int> { .. })` → `Box2<int>`）+ 泛型变体 match 模式匹配全链工作。类型位方括号兼容形式 `Name[T]` 可用；表达式位类型实参用规范形式 `Name<int>`（`[T]` 与索引文法冲突，裁决限类型位）。
- **Map 迭代 KV entry（2026-10-13，决策 82 第二步）**：`for kv in m` 产出内建 `KV { key, value }` struct（插入序快照）——`kv.key`/`kv.value` 字段访问与 `${}` 插值全链工作；纯键通道迁移为 `for k in keys(m)`（keys 内建既有）。`KV` 为保留类型名（声明拒绝 Y-T0005）。**破坏性**：LBC0 major 2.0——旧 1.x 产物的键语义与本版不符，重新编译即可。
- **VM 取指热路径合并（2026-10-13，P1 首步，25 号 §A）**：step() 单次帧尾访问取 (proto, pc) 并推进 + `Frame.proto` 直存（省每条一次 Rc 解引用）。受控 A/B ~1%（严格减法保留）；同批证据裁决：Quickening 算术特化在当前负载形态下建模收益 ~1% 不抵架构成本，**诚实延后**（触发条件：算术密集负载 Add/Sub/Mul/Div 直方图占比 >25%）。
- **struct 字段访问静态槽位表 + 懒名字（2026-10-13，P0b，25 号 §A）**：字段序由 struct 声明表静态决定，run 装载期预计算 `(struct_id, fid) → slot` 槽位表——GetField/SetField 快路径 O(log k) 二分 + fid 复核（防宿主构造值自对齐漂移），未命中回退线性扫描（语义逐位保留）。成功路径不再为错误信息做 String 堆分配（懒构造）。struct 密集负载（新增 benchmarks/structs）median **1441.1 → 936.7ms（-35%）**，checksum 逐位一致；sort 基准零字段操作（直方图实证），基线持平。

### Changed
- **CLI flag 输入域收紧（2026-10-13，质量审计）**：`--heap-mb` 数值域改 u64——负值原会流入限额逻辑产生「一切分配静默失败」的荒谬状态，现解析层即拒绝（exit 2）；巨值饱和转换（实际无限，语义保真）。`--entry=`/`--out=` 空值解析层拒绝（空名永远非法、空路径只会产出不可读写错误）——fail-fast 优于运行时结构化错误。`.yoruc` 构建产物入 `.gitignore`（可由 build 再生，不入库）。
- **同型不可算术诊断更正**：`xs + xs`（List 等同型不支持算术）原报「期望 [T] 得到 [T]」自相矛盾，现报「类型不支持算术运算」（Y-T0022）。

### Added
- **`yoru run` LBC0 直载（2026-10-13，P0a④，25 号 §A）**：`yoru run foo.yoruc` 跳过前端全链（parse/typeck/lower）直入 VM——`build` + `run` 构成产物分发闭环。真实性/合法性双闸：deserialize 把关 magic/version/CRC32，verify_module 坚持 validate-before-use（不豁免自产字节码）；坏产物结构化拒绝 exit 1。`check`/`build` 误入 `.yoruc` 给诚实指引（替代 UTF-8 解码噪音）。e2e：sort 与 std-import 模块直载输出逐位一致；`--entry`/`--host`/`--stats`/停机三旋钮全部可用。启动实测：tiny min 36.6→28.9ms（-21%，地板 = 进程孵化 ~28ms）；原「32→16ms」预测证伪（小模块前端仅 0-5ms），真实收益面在嵌入进程内加载与大模块分发。
- **`yoru run` 停机安全三旋钮（2026-10-13，25 号 §4-c）**：`--max-instructions=N` / `--heap-mb=N` / `--deadline-ms=N`——「跑别人给的脚本」场景的收紧（沙箱化）与放宽（重负载）均可表达（默认写死 1 亿指令 + 64 MiB 堆不可调）；解析失败 = 用法错误 exit 2。e2e 实证：死循环脚本分别以 Instructions / Deadline / HeapBytes 优雅停机（exit 1），坏值 exit 2。
- **S 线稳定性与极限压测（2026-10-13，25 号 §4-c，测试 +6）**：①长跑确定性（200k 迭代 >1M 指令，双 VM 逐位同结果 + 指令数一致）；②停机时延量化界（钩子第 k 次触发时 instructions ≤ k·check_interval + 1）；③极值算术矩阵（MIN±1 / MIN×-1 / MIN÷-1 / MIN%-1 / -MIN 全部结构化 Y-R9001，debug 构建零 panic 面）；④退化输入（空源/仅注释/缺 main → 结构化 Y-R9000）；⑤无界增长（循环字符串拼接 → 堆限逐次拦截）。

### Fixed
- **const 折叠语义分叉（2026-10-13，决策 44 透明性）**：`fold_const` 原用截断 `checked_div/checked_rem`，与 VM 运行时 floor 语义（`div_euclid/rem_euclid`）分叉——同一表达式 const 声明与 let 赋值结果不同（`(0-7)/3`：折叠 -2 vs 运行时 -3；`(0-7)%3`：-1 vs 2，红证实锤）。修复为 `checked_div_euclid/checked_rem_euclid`，回归测试锁定 const 折叠与运行时逐位一致。

### Added
- **中断节流倒数计数器（2026-10-13，数学验证 + 多源调研批次）**：热路径 `instructions % check_interval`（运行时变量 64 位 div ~25 cycles/条，非二次幂不可强度削减）→ 倒数计数器（dec+cmp ~1 cycle），触发点逐位等价（第 k·interval 条）；`max(1,·)` 消除 interval=0 除零 panic 面。实测 sort median 540-567 → 496.6ms（**-8~9%**），checksum 逐位一致。max_instructions 保留每条精确检查（上界语义不弱化）。
- **激进优化三线调研取舍档（25 号 §4-b）**：computed goto/tail-call threading（Rust 稳定下 match≈computed goto，nightly 拒绝）；copy-and-patch stencils（CPython 三年 5-6%，登记远期）；GPU 后端 NVRTC/PTX/persistent kernel（launch 税 ~5μs + 嵌入可移植性铁律冲突，登记远期可选后端）。

### Added
- **CLI 全命令面同构统一（2026-10-13，一致性审计）**：① 退出码语义收敛——用法错误（unknown flag/缺参）= 2，执行错误（含 IO/编译/运行/断言）= 1（`check`/`build`/`run` 的 IO 错误原为 2，与 fmt/doctest 冲突，行为级实证修正）；② `doctest` 补齐同构 flag 解析骨架（原 `-x` 参数被静默当路径——接口不一致实证修复）；③ 新增 `-h|--help|help`（成功请求 exit 0，clig.dev），help 与未知命令共用 `print_help` 单一事实源并补全 `[--stats]`/`-l` 缺档；④ flag 语法约定成文：布尔 `--name`、带值 `--name=value`（不支持空格形式），fmt 保留 gofmt 系短形式 `-w`/`-l`。
- **诊断流统一（2026-10-13）**：诊断详情（severity [line:col] [code] message）→ **stderr**（rustc/gcc 惯例，clig.dev 日志类）；结果摘要与程序 print 输出 → stdout——`2>/dev/null` 后 stdout 只剩摘要行。

### Changed
- **doctest check 块同构复用 `check_source`**：此前手写 `parse_source + check_program` 管道存在接口差异——`parse_source` 词法错误提前返回只报**第一个**，`check_source` 累积全部诊断；统一后坏块断言面与 `yoru check` 逐位一致（同一诊断管线同一结果），并删除一层手写管道。

### Added
- **fuzz 双面分歧归档（2026-10-13，质量审计）**：`Outcome::Diverged` 独立承载「typeck 过但 lower 拒」（不再折叠进 `Reached(Typeck)`），长跑逐例归档 `fuzz-artifacts/diverg_case*.yoru`（含诊断码，V1 对拍直接素材）；报告剔除结构性恒零的 reached typeck/lower 死输出行。

### Removed
- **死 API 清理（2026-10-13，全量引用审计：156 个非测试 pub fn 逐一词边界计数）**：`yoru_stdlib::select_module`（零引用，与 `module_fn` 语义重复）、`Opcode::is_terminator`（零引用，且对融合跳转语义已过时）、`Value::heap_estimate`（零引用 + 文档声称「与 max_heap_bytes 共用」不实 + capacity 口径与 VM 实际 len 口径不一致——堆观测单一口径收敛于 `Vm::alloc(bytes)` 并补文档）、`CaptureLayer.proto_idx`（零读取 + `allow(dead_code)` 豁免，豁免即债）。

### Added
- **VM opcode 直方图 + `yoru run --stats`（2026-10-13，P0a① profile 释疑）**：`enable_op_histogram` 显式开启（默认零开销，热循环单分支）——Windows 无 perf/VTune 下以指令分布驱动超指令选型，不凭感觉。sort 负载实证：LoadLocal 30.8%、Nil/Pop 各 7.7% 为三大浪费源。
- **超指令第一批：融合条件跳转 `JmpIfNotLt/Le/Gt/Ge` + 双局部下标读 `GetIndexLL`（2026-10-13，P0a③，25 号 §A）**：lower 窥孔自动融合 `Xx; JmpIfNot` 与 `LoadLocal; LoadLocal; GetIndex`，比较/下标语义与独立指令逐位一致（共用 cmp_values 单一事实源，错误面 Y-R9007/Y-R9011/Y-R9013 不变）。sort 负载指令 58.5M → 36.0M（-38.4%）。
- **`Nil; Pop` 窥孔 pass（2026-10-13，P0a 配套）**：赋值语句尾「值补位 Nil + 语句级 Pop」两指令零功对，proto 收尾后处理统一消除（指令 -15.4%）。**发射期窥孔已证伪并撤销**：块尾值 Nil 可能同时是跳转目标（if 无 else 臂补位 = JmpIfNot 落点），删除导致跳转错位（08_physics 实证、最小 repro 固化为回归测试 ×3）；pass 以跳转目标集合保守判定 + 全量偏移重算保证正确性。

- **yoru-doctest crate + `yoru doctest` CLI + CI job（2026-10-13，D 线 doctest，28 号 §2.3 / 30 号序 7）**：文档示例机械化验证——C8 规格漂移的 CI 防线（文档示例坏了 CI 红，而非事后人工登记）。双语料共用同一提取器：markdown ` ```yoru ` 围栏块 + `.yoru` 源 `///` 文档注释内嵌块（spec 19.1「/// = doctest 源」兑现，决策 83 词法一等保留后零成本）。断言尾注：`// =>` 期望 print 输出逐位比对（连续多行 = 多行期望）；`// error:` 诊断码集合精确比对（check 块，只 parse+typeck 不执行）；无尾注 = 全链契约（typeck + lower + verify + run(main)）。块为完整编译单元。CI doctest job 第二步兑现 examples/math README §0 运行契约（11 篇全部 `yoru run` 通过）。范围修正：原定设计文档 01/06/19 内嵌示例——经盘点无可执行围栏且设计目录在仓库外 CI 不可达，改为仓库内播种（README 3 块 + examples/doctest_demo.yoru），设计文档侧标注 ` ```yoru ` 后可自动纳入。V1 最小对拍（examples 11 篇 typeck ok ⟺ lower ok）随 tests 落地；V2 语法驱动语料生成顺延（30 号 §6）。
- **yoru-fuzz crate（2026-10-13，V 线 V0 crash-fuzz，30 号 §6.1）**：全链 panic-free 契约机械化验证——`lex → parse → typeck → lower → run(有限步)` 五阶段 catch_unwind，token 段级语法变异（字典注入/删除/复制/换行扰动，强度分布化）+ 深度守卫 + 确定性 RNG（`(seed, case_id)` 可重放）。run 有限步由 VM 资源三限承接（零 VM 扩展）。验收：10 万 case panic-free（62s）；smoke 800 case 常驻回归。libFuzzer 不支持 Windows，自研 harness 可薄包装为未来 cargo-fuzz target。
- **`get` / `get_or` 内建（2026-10-13，28 号 R 线，27 号 §3.2 strict 取键）**：`get(m, k)` 命中返回值、缺键返回 None（`V?`），`get_or(m, k, d)` 缺键返回缺省值——缺键处理惯用法断层（get 侧）关闭。`map.get` 拼写与列表 map 函数撞名（决策 77 单一命名空间），按 keys/values 先例取裸名。类型闸实证：`int?` 赋 `int` 编译期拦截、键类型泛型冲突拦截。
- **`yoru fmt` CLI（2026-10-13，F 线 F2）**：gofmt 风格三形态——默认 stdout（管道组合）、`-w` 原地写入、`--check` 校验（有差异 exit 1，CI 可直接挂）。语义不变性以最强形态验收：examples 11 篇格式化前后 `yoru run` 输出逐位一致（diff 执行结果而非 diff 文本）；解析错误文件诚实拒绝（fmt 只对合法文件负责）。
- **yoru-fmt crate（2026-10-13，F 线 F1 核心，29 号裁决架构）**：token 空白重写器 + AST 区间指导布局 + span-blind 结构投影打印器 + `verify` 双断言（token 序列保真 + 结构投影保真 + 幂等）。注释/字符串/字面量经 span 逐字节保真；块体逐行缩进、struct_decl 字段与 match 臂逐行、行注释强制行尾、空行保留坍缩。三形态 spike + examples 11 篇全量双断言通过。CLI `yoru fmt` 属 F2。
- **30 号战略定夺（2026-10-13，文档）**：主轴转工具链（F→R→D→L），语言层只清 Critical 实现债穿插推进；D-03（struct 算术 typeck 放行→VM 崩）定夺 **fail-fast** 方向；新登记 **V 线语义差分测试基建**（cargo-fuzz crash-fuzz + 双模式对拍）。设计文档侧更新，无代码变更。

### Changed
- **解释器执行性能：sort 基准 median 730.9ms → ~540-567ms（-24%）**（2026-10-13，P0a① ② ③ 合并效应）：净执行 ~705 → ~520ms（-26%），对 QuickJS-ng 差距 ~2.9x → ~2.1x；启动 32ms 保持（编译/加载铁律无损）；checksum 与融合前逐位一致。**mimalloc A/B 实测反慢 ~9% 已撤销**（Windows 堆在 Rc 小分配负载下已足够优，数据留档于 25 号）。

### Fixed
- **遗留对账测试编译错误（2026-10-13，doctest 批次盘点时发现）**：`builtin_fns_mirror_locked` 测试仍引用 BUILTIN_FNS 派生化前的旧符号（31b8908 改造漏同步，workspace 全量测试实际红）——该测试在派生化后已退化为同义反复（镜像==注册表由构造保证），予以撤销并留痕；真正的对账面在 VM 侧 `registry_dispatch_reconciled`（继续承担漏注册/幽灵名防线）。
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
