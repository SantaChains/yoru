# Yoru 游戏数学示例套件（examples/math/）

> 一套**全部 `yoru run` 通过（exit 0）**的示例，覆盖「已确认语法」的线性代数、计算机图形学、游戏数学、高性能数学运行四大方向，并形成一条**宏观串联的叙事管线**。
> 配套缺陷证据：见设计文档 [`26-示例套件实测缺陷与规格落差登记.md`](../../L语言设计/26-示例套件实测缺陷与规格落差登记.md)。

## 0 运行方式

```bash
# 推荐用隔离构建，避免共享 target 被陈旧 rlib 链接
CARGO_TARGET_DIR=/tmp/yoru_fresh cargo build -p yoru-cli
/tmp/yoru_fresh/debug/yoru run examples/math/01_vec2_lin_alg.yoru
# 或已安装 yoru 后：
yoru run examples/math/01_vec2_lin_alg.yoru
```

- 入口恒以 **0 实参**调用；`main` 返回 `int`/`Result[...]`，CLI 末尾会打印其返回值（如 `= 0`），属正常。
- 所有 11 篇均**零 `.rs` 改动**产出；发现的语言缺陷全部登记于 26 号文档，**不在本仓库修源码**。

## 1 宏观叙事（两条主线）

```
渲染管线主线：  01 Vec2 线性代数 ─┐
                               ├─► 02 Mat2 2D 变换 ─► 03 多边形(面积/质心)
                               │       │
                 04 三角函数近似(Horner/象限约减, 高性能数学方法论)
                               │       │
                               └─┬─────┘
                                  ▼
                      05 Vec3 + 透视投影管线(把向量与变换合成 3D 渲染雏形)
                                  ▼
                      06 软件光栅器(距离场 ASCII 渲染, 消费 01 的距离概念)

交互/仿真主线：  07 游戏数学(clamp/lerp/smoothstep/LCG/状态机)
                      ▼
                  08 物理积分器(Euler/symplectic/定步长累加器)
                      ▼
                  09 计算几何谓词(orient/线段相交)
                      ▼
                  10 PIP 空间查询(把 09 谓词用于多边形内判定)

收尾：           11 语法总览(distinct/位运算/map/string/rune/Result/json/解析/切片)
```

两条主线共享 **01 的向量代数**与 **04 的高性能近似方法论**（Horner 展开、常量折叠、`const` 四分之一周期约减）。

## 2 特性覆盖表

| 文件 | 主题 | 覆盖的已确认属性 |
|---|---|---|
| 01 | 二维向量线性代数 | `struct`/字面量/spread 更新、自由函数、lambda/闭包、`\|>` 管道（`rhs` 为调用）、`map`/`sort_floats`/`fold`、闭包捕获、`length`/`normalize`/复数旋转 |
| 02 | 2×2 矩阵变换 | `struct`、`impl Add`（声明式，附注方法不可调用）、`Option` 收窄分支内**直接用名**（避 D-04）、`det`/`inverse`、复合运算 |
| 03 | 多边形（鞋带/质心） | `while`+floor-mod 环形索引、`cross`/`area`、`spread` 更新 `Vec2 { ..p, x: v }`、`slice [1..3]`、`for in list`、`Option` **分支外** `!` 解包 |
| 04 | 三角函数近似 | `const` 折叠、Horner 系数、四分之一周期约减 + 象限查表、`for`/`range`、`math.floor`、`sin/cos` 自洽；bench 量级合理 |
| 05 | Vec3 + 透视投影管线 | `Vec3`、`[float]` 列表、齐次投影直接公式、`for` 顶点循环、NDC 齐次除法、零平移不变量断言（mat4 字面量因 D-01 预算改为向量管线） |
| 06 | 软件光栅器（距离场） | `map`/`range`/`for`、嵌套 `map` 仅捕直接父级（避 D-24）、顶层 `any_near` fn、全 int 平方距离比较（免 sqrt/abs）、对称标定板输出 |
| 07 | 游戏数学 | `clamp`/`lerp`/`smoothstep`/`easing`、`tag` 状态机 + `match` 返回值、`LCG` PRNG（确定性）、`assert` |
| 08 | 物理积分器 | 显式 vs 辛 Euler 能量对比、`while` 定步长、`fold` 能量规约、固定步长累加器（spiral of death 防护） |
| 09 | 计算几何谓词 | `orient`（叉积符号）、线段相交（跨立试验）、`if` 语句 + 早返回（避 D-17） |
| 10 | PIP 空间查询 | 射线法奇偶判定、复用 09 `orient`、包装谓词为 `fn`、循环累加 |
| 11 | 语法总览 | `import json as j`/`import string`、`distinct` + `as` 双向、`tag Err`、`Result`+`Ok/Err`+`try` 调用点解包、`&`\|`^`<<`>>`、`[string:int]` map + `keys`/`values`、`string.to_runes`/`upper`、`json.parse`/`stringify`、`parse_int`/`parse_float`、切片、`hex`/`下划线`/`指数` 字面量、rune 迭代、`let _` 丢弃 |

## 3 预期输出（节选，已逐项人工验算）

| 文件 | 关键输出 |
|---|---|
| 01 | `len=5.0 unit=0.6000000000000001,0.8`（3-4-5 单位化） |
| 02 | `combo(1,0)=0.0,2.0 det=4.0`（R·S 复合 + 行列式） |
| 03 | `tri area=6.0 centroid=1.3333333333333333,1.0` |
| 04 | `sin(pi/6)=0.4999999999999643`（≈0.5）；`bench sum(65536 sin)=1905.6247045897967` |
| 05 | 8 顶点 NDC，`\|ndc\|<1`、深度随视距单调（Ry(0.6,0.8)·T(0,0,-5)·透视） |
| 06 | 11×20 对称标定板（横/竖/X 对角线） |
| 07 | `smoothstep(0.35)=0.28174999999999994`；骰子 4/3/6；状态机 `running→paused→running` |
| 08 | 显式 Euler `E=0.824`（指数增长）vs 辛 `E=0.489`（有界）；累加器 `2 steps, leftover 0.001` |
| 09 | `orient=-12`；`ab x cd=true`；`ac x de=false` |
| 10 | `inside(1,1)=true`；`inside(4,4)=false`；`inside(3,-1)=false` |
| 11 | `bits: 1 7 6 16 64`；`quadruple(21)=84`；`json: {"hp":100,"name":"yoru"}` |

## 4 float 显示说明

float 显示为 Rust `f64` 最短回显（如 `0.6000000000000001`、`0.28174999999999994`）。这是 `f64` 二进制↔十进制的最近表示，**非误差**；比较/断言请用容差（如 `abs(a-b) < 1e-9`），勿做 `==` 精确相等（见 04 的 `assert` 写法）。

## 5 自包含约束

CLI 当前**无用户模块 import** 通道（非 std 模块在 demo host 绑空模块表），故每篇**重声明所需核心类型**（`Vec2`/`Vec3`/`Seg`/`State` 等）。跨文件复用须复制类型定义，或待模块系统落地后改为 import。

## 6 已知缺陷与规避 idiom（索引）

示例为避开语言缺陷，统一采用 11 条 idiom（详见 26 号文档 §4）。最关键的几类：

- **静默错误结果（绝不要在真实代码踩）**：struct 字段赋值静默丢（D-06）、链式索引赋值静默丢（D-09）、`if` 表达式作值静默 nil（D-17）、窄化 `Ok` 运行期 Variant 泄漏（D-20）。
- **运行期崩溃**：`a+b` 运算符重载（D-03）、复合字段/索引赋值（D-07/D-08）、嵌套 lambda 传递捕获（D-24）。
- **合法代码被拒**：wrap 全局预算（D-01）、插值 `${x!}` 自旋（D-02）、for-kv map（D-10）、`match` Result（D-18）、`parse_int` 模块名（D-19）。

完整最小复现、根因 `文件:行`、严重级、workaround 与修复方向见 **[26-示例套件实测缺陷与规格落差登记.md](../../L语言设计/26-示例套件实测缺陷与规格落差登记.md)**。
