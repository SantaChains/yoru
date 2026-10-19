# 29 F 线 formatter 设计审核（对 28 号 §2.1 的对抗审核与修订）

[目录](README.md)

> 状态：**设计裁决定稿**（2026-10-12，F 线开工前置审核）。
> 象限：**Normative**（F 线 formatter 的架构裁决、前置缺口、验收协议、布局铁律）。
> 方法：全部主张锚定源码/文法/CLI 实证（lexer.rs / token.rs / parser.rs / 19 号规范 / 实测三例），无凭印象判断。
> 纪律：零 `.rs` 改动——本文档只裁决设计，代码按 §7 阶段化落地。

---

## 0. 结论速览

28 号 §2.1 的 formatter 设计 **3 项推翻、2 项修正、主体方向保留**：

| # | 28 号原设计 | 裁决 | 依据 |
|---|---|---|---|
| D1 | 管线 `parse → AST → 格式化打印 → src'` | **推翻**：改为 **token 流空白重写 + AST 结构指导**（§2） | 换行终止文法下 AST 打印 = 半个逆向 parser；注释/字面量保真在 token 层免费 |
| D2 | 「注释与字符串字面量逐字节保真」 | **前置缺口实锤**：lexer 丢弃全部普通注释，`///` 也仅 skip 不挂流（§3）——F0 必须先扩 lexer | lexer.rs `slash()` 非 doc 分支读完即弃；examples 现有 70+ 行注释全部词法不可见 |
| D3 | 往返验收「AST 保真往返」 | **修正**：Debug 比较方案不可用（span 污染），改为 **token 序列保真 + 结构投影双断言**（§4） | Span（字节偏移）参与全部 AST 节点，重排后必然不等 |
| D4 | 风格基线（尾随逗号/空行/100 列） | **修正**：补两条文法级铁律（运算符行尾、struct_decl 换行分隔），尾随逗号规则限定作用域（§5/§7） | `continues_line()` 全表 + 19:41/95 文法 |
| D5 | spike 验证「AST→文本→同 AST」 | **换靶**：spike 验证「重排文本 → re-parse → 同结构投影」（§6 F1） | D1 裁决的连带换靶 |
| — | 依赖闭包定位（F 仅 syntax，第 1 位）、`yoru fmt` + `--check` CLI、幂等验收 | **保留** | 无争议 |

---

## 1. 证据链（代码锚点）

| 事实 | 锚点 | 对设计的影响 |
|---|---|---|
| 换行即语句终止，产出 `Newline` token | lexer.rs 模块文档 + `emit_newline()` | fmt 的输出必须让 re-lex 产出正确的 Newline 序列——这是核心不变式 |
| 抑制规则三条：`depth>0`（`(`/`[` 未闭合）；行尾 `continues_line()`；行尾 `{`（仅下一次） | lexer.rs `emit_newline()` L117-133 | 布局发射器的合法性判据，全部 token 级、确定性 |
| `continues_line()` 全表：二元/赋值/比较/逻辑/位运算符、`->` `.` `..` `,` `:` `?` `else` | token.rs L108-138 | 行首任何这些记号 = 前语句被终止 → 解析错误（f2 实证） |
| **普通 `//` 与 `/* */` 读完即弃**，仅 `///` 产 `DocComment(text)` | lexer.rs `slash()` L141-163 | D2 前置缺口 |
| DocComment 仅被 `skip_newlines()` 当噪音跳过，**不挂 AST 任何位置** | parser.rs L114-122 | AST 打印连 `///` 都会丢 |
| AST 全部 `#[derive(Debug, Clone)]`，节点内嵌 `span: Span`；`Span` 是字节偏移 `PartialEq` | ast.rs / diagnostic.rs L6 | D3：Debug 与 derive PartialEq 都被 span 污染 |
| struct_decl 字段**换行分隔**（无逗号）；struct_lit 用逗号 | 19 号 L41 vs L95 | D4：尾随逗号规则不能覆盖 struct/tag 声明 |
| match 臂 `pattern "->" (expr | block)`，臂间换行分隔 | 19 号 L89-90 | 多行 match 天然合法，臂间不发空行 |
| 插值内禁止换行 | 19 号 L21 | fmt 不碰字符串 token 文本即天然满足 |
| 单行 block 合法（`if x == 1 { return 5 }` check 通过） | 实测 f1 | 块压缩风格可行；`{` 行尾抑制语义支撑 |
| 行首运算符确凿非法（`a\n+ 2` → Y-S 双解析错误） | 实测 f2 | 布局铁律 #1 的实证 |
| examples 现有 70+ 行普通注释，词法层全部不可见 | rg 统计（06 篇 24 行、11 篇 21 行…） | F0 是全工具链共性缺口（LSP hover/fmt 共用） |

---

## 2. 裁决 D1：抽象层级——推翻 AST 打印，采用 token 空白重写

### 2.1 为什么 AST 打印在换行终止语言里是错误抽象

AST 打印器要把每个节点重新编码为文本，而文本合法性由 lexer 的换行抑制规则判定。打印器在**每个可能发换行的位置**都必须回答「这个换行会不会被 lexer 吞掉/断句」——这等于在打印器里重建半个 lexer 的逆向逻辑，且每条规则都要与 lexer 保持同步（`continues_line()` 加一个 token，打印器就要同步改）。

反过来，**token 流重写方案把这个问题消解掉**：formatter 的输入就是 lexer 眼中的世界，它只重排空白（换行/缩进/空格），token 文本按 span 从原文逐字节切片。发射的不变式只有一条：

> **fmt 输出 re-lex 后，非空白 token 序列（含注释文本）与原 token 序列相等，且解析出的 AST 结构投影相等。**

布局规则全部落为「在哪些 token 之间发多少空白」的局部决策，与 lexer 抑制规则天然同源（都是 token 级确定性规则）。

### 2.2 混合架构：AST 给结构，token 给文本

纯 token 重排缺两样东西，由 AST 补齐：

| 需求 | 来源 | 用法 |
|---|---|---|
| 块区间与缩进层级 | AST 声明/块节点 span | `fn`/`struct`/`match`/`if` 的 `{...}` 区间 → 深度 +1 |
| 语句/臂/字段边界 | AST 语句与臂节点 span | 每语句/臂/字段起一行 |
| 表达式折行边界（超 100 列） | AST 表达式树 | 参数列表/二元链按节点粒度折行 |

### 2.3 保真收益对照

| 关注点 | AST 打印 | token 重写 |
|---|---|---|
| 普通注释 | 必须 invasive trivia 挂 AST | lexer 产 Comment token 即可，parser skip |
| 插值字符串 | 递归重建 StrPart（风险点） | span 切片零风险 |
| `1.0` vs `1` 等字面量原文 | 打印器规范化（如 Float 转 Debug 可能变文本） | span 切片保真 |
| 与 lexer 规则同步成本 | 每条抑制规则双份维护 | 同源，零同步 |

---

## 3. 裁决 D2：F0 前置——lexer 保留注释（含回归风险 R2）

### 3.1 缺口

lexer `slash()` 对非 doc 注释（`//` 单斜杠、`/* */` 块注释）读完即弃；`///` 产出的 `DocComment` 也只是 parser 的 skip 噪音。**fmt 若今天开工，examples 的 70+ 行注释会在格式化输出中全部消失。**

### 3.2 F0 最小扩展（文法零变更）

1. lexer `slash()` 非 doc 分支产出 `TokenKind::Comment(String)`（行注释含文本、块注释含全文与 span）；
2. parser `skip_newlines()` 加 `Comment(_)` 分支（改名 `skip_trivia`）；
3. **R2 回归风险（必须同批修）**：行尾注释会吞延续判定——`a + // c\nb` 中 emit_newline 的 last token 是 Comment（不 `continues_line`）→ 产出 Newline → 语句被注释截断。而当前语义下该注释不存在、last 是 `+`、换行被抑制、语句延续。**若不处理，F0 扩展将破坏既有合法程序**。修复：`emit_newline()` 遇 last 为 Comment 时穿透回溯到最后有效 token 再判延续。块注释跨行同理（span 覆盖多行，穿透判定一致）。
4. 块注释内部换行维持现状（不产出 Newline token）——当前跳过逻辑即如此，保持。

### 3.3 附带收益

LSP（L 线）的 hover 注释、语义高亮、折叠区间全部依赖注释可见性；doctest 提取文档注释同理。F0 是四线共享的地基，成本一次性。

---

## 4. 裁决 D3：验收协议修订

28 号「AST 保真往返」不可直接断言——AST 节点全部内嵌字节偏移 Span，格式化必然移动 span，`Debug` 字符串比较与 derive `PartialEq` 都会在 span 处恒假。修订为**双断言**：

1. **token 序列保真**（文本层）：fmt 输出 re-lex 后，非空白 token 的 `(kind, 原文切片)` 序列与原 token 流逐项相等——含 Comment 文本、Int/Float 值、Ident 名、字符串 token 的结构。kind 已是全值语义（`Int(i64)`/`Float(f64)`/`Ident(String)`/`Str(Vec<StrPart>)`），天然 span-blind。
2. **结构投影保真**（语义层）：手写 ~50 行递归投影打印器（span-blind，只输出节点结构），fmt 前后投影字符串相等。这一条专抓 token 断言抓不到的「换行歧义重排」——fmt 发出的换行若被 lexer 断错句，投影立变。
3. **幂等**：`fmt(fmt(x)) == fmt(x)` 字节级。
4. **语义不变**：examples 11 篇 + benchmarks，fmt 后 `run` 输出逐位一致、`check` 0 诊断。
5. **fuzz 抽样**：既有 fuzz 语料无 panic。

双断言中 1 是构造性保证的校验（fmt 不动 token 顺序），2 是核心验收（布局歧义防线），不可互相替代。

---

## 5. 布局铁律（文法反推，全部实证）

formatter 发射换行/空白时必须遵守的硬规则，违反即产生解析错误或语义变化：

| # | 铁律 | 证据 |
|---|---|---|
| L1 | **二元/赋值/比较运算符、`->` `.` `..` `,` `:` `?` `else` 永远放行尾或与操作数同行；永远不放行首** | `continues_line()` 全表 + f2 实测（行首 `+` → Y-S 双错） |
| L2 | 语句、struct_decl 字段、match 臂以换行分隔——每个结构成员起一行 | 19 号 L21/L41/L89 |
| L3 | `struct_decl`/`tag_decl` 字段**无逗号可用**，多行声明靠换行分隔；尾随逗号规则**仅适用**于 struct_lit/参数表/list/map 字面量 | 19:41 vs 19:95；Comma ∈ continues_line |
| L4 | `(` `[` 未闭合时换行自由（depth 抑制）——参数表/下标折行天然安全 | lexer `emit_newline` depth 分支 |
| L5 | `{` 行尾时下一换行被吞（仅一次）——块体首条目前不发换行，靠发射器自身布局 | emit_newline LBrace 分支 |
| L6 | 字符串 token（含插值）整体不换行不重排 | 19:21 插值禁换行 + span 切片保真 |
| L7 | 行尾注释后若原文是延续记号，fmt 必须保持延续结构（F0 穿透修复保证 re-lex 一致） | §3.2 R2 |

---

## 6. 修订后管线与阶段

```
src ──lex（F0 后含 Comment）──▶ token 流 ─┐
                                          ├─▶ 空白重写器（AST 区间指导布局）──▶ src'
src ──parse──▶ AST（块区间/语句边界）─────┘
```

| 阶段 | 内容 | 验收 |
|---|---|---|
| **F0**（前置，动 lexer/parser 最小面） | Comment token + `emit_newline` 穿透 + `skip_trivia` 改名 | 全量回归测试不红；新增注释保留单测 + R2 行尾注释延续单测 |
| **F1**（核心） | crate `yoru-fmt`（或 yoru-syntax 内 mod）：token 空白重写器 + AST 区间指导 + 结构投影打印器 + 风格规则（§7） | §4 双断言 + 幂等；先 spike：单文件三形态（紧凑/多行/注释密集）往返 |
| **F2**（CLI） | ~~`yoru fmt <path>`（原地）+ `--check`（diff + exit 1）；examples 11 篇 + benchmarks 全量验收~~ ✓ **F2 已落地（2026-10-13）**：`yoru fmt [--check \| -w] <file.yoru>...`（gofmt 风格三形态：默认 stdout / `-w` 原地 / `--check`；clig.dev 契约 0/1/2）；验收 = examples 11 篇格式化前后 `yoru run` 输出**逐位一致 11/11** + `-w` 后在盘幂等 + 解析错误 rc=1 | §4 全协议过；CI 可挂 `fmt --check` |
| **F3**（让位） | LSP formatting 复用（range 格式化） | L 线开工时 |

风格基线执行顺序：先 80%（缩进/换行/运算符空格/冒号空格），折行策略（100 列参数表折行 + 尾随逗号）第二批——折行是唯一需要表达式树粒度决策的部分。

---

## 7. 风格基线修正表（对 28 号 §2.1）

| 项 | 28 号原文 | 修订 |
|---|---|---|
| 缩进/花括号/空行/运算符空格/冒号 | 4 空格、`{` 不换行、顶层 1 空行、`:` 后空格前 0 | 保留，与 examples 惯例一致 |
| 尾随逗号 | 「多行参数/结构体字面量加」 | 限定：仅 struct_lit/参数表/list/map（L3）；struct_decl/tag_decl 无逗号文法 |
| 注释保真 | 「fmt 不碰 token 文本」 | 升级：注释进 token 流后，行注释随语句缩进对齐，独立行注释保持独立行；doc 注释（`///`）吸附到后随声明（当前 skip 语义允许自由漂移，fmt 定桩消除漂移） |
| 折行 | 「fn 签名单行超 100 换参数」 | 保留，第二批实现；**运算符折行一律行尾**（L1 铁律） |
| 换行敏感 spike | 「AST→文本→同 AST」 | 换靶：重排文本 → re-lex token 序列相等 + re-parse 结构投影相等 |

---

## 8. 风险登记

| 风险 | 级别 | 缓解 |
|---|---|---|
| R2 行尾注释破坏延续判定（F0 引入） | 高（回归既有程序） | F0 同批修穿透 + 专项单测（`a + // c\nb` 编译不变） |
| DocComment 吸附改变现状漂移语义 | 低 | 仅 fmt 输出定桩，parser 语义不变 |
| 结构投影打印器遗漏新 AST 节点 | 中 | 投影器与 AST 同文件放置 + fuzz 抽样；新节点 PR 模板提醒 |
| 折行决策与 L1 冲突（如管道链 `|>` 行首） | 中 | `Pipe`/`PipeGt` 已在 continues_line 表——管道链折行同样行尾；单测覆盖 |
| 块注释内含 `${` 等 token 样文本 | 低 | 注释整体一个 token（span 切片），无词法歧义 |

---

## 9. F0 开工清单（下轮入口）

> **【F0 已落地，2026-10-12】** 全部五项完成，架构裁决升级为 **rust-analyzer 分层微缩**：`parse()` 入口 `is_trivia` 一次性预过滤（备选「parser 各循环逐一加 Comment 臂」被否——维护面大一个数量级），`skip_newlines` 的 DocComment 死臂随之清理。**附带修复既有潜伏缺陷**：`a + /// doc\nb` 此前即断句（DocComment 不参与延续判定）。穿透语义 = Go scanner 同款（回溯最后有效 token）。测试 +9（syntax 4/vm 5），138 全绿；CLI 注释全形态端到端 ✓。见决策 83。

1. ~~`TokenKind::Comment` 变体 + `slash()` 非 doc 分支产出（行/块两形态，span 精确）~~ ✓
2. ~~`emit_newline()` 尾部 Comment 穿透判定~~ ✓（含 DocComment——潜伏 bug 修复）
3. ~~`skip_newlines` → `skip_trivia`（+Comment 分支），全量测试回归~~ ✓（升级为 parse() 入口预过滤，skip_newlines 反而简化）
4. ~~新单测：注释保留 / 行尾注释延续 / 块注释跨行 / 插值旁注释~~ ✓（+位置族与 inline 中缀）
5. ~~F1 spike 三形态语料（从 examples 选：紧凑 03 / 多行 01 / 注释密集 06）~~ ✓ **F1 已落地（2026-10-13，crate `yoru-fmt`）**
   - **plan.rs（AST 半边）**：`block_opens`（块式 `{` 偏移——Block.span.start 直取 = LBrace 精确偏移；match 臂区/struct/impl/protocol 体由「内容首 token 前最近 LBrace」回扫；空体不登记 → 行内 `{ }`）+ `breaks`（顶层 decl / 块内 stmt / struct 字段 / match 臂 / impl·protocol fn）。断点首 token 全为词/字面量 → L1 由构造保证。
   - **rewrite.rs（token 半边）**：逐 token 发射，间距表（紧排例外显式列出、默认空格；一元 `-`/前缀 `!` 右侧紧排走 tight_next；后缀位 Comma/RParen/RBracket/Question/Colon/Dot 贴靠；`a..b` 紧 vs `{ ..p,` 空格由 prev_operand_end 区分）；行注释/文档注释后强制行尾（L7 延续语义由决策 83 穿透判定承接）；独立行注释按原文换行归属（go 同款）；空行保留并坍缩（≥2 换行 → 1 空行）。
   - **projector.rs**：span-blind 结构投影（穷尽 match，AST 新节点编译期强制同步——29 号风险登记的机械化缓解）。
   - **lib.rs `verify`**：D3 双断言机械化（token 键 = span 归零 kind + 原文切片；**发现 D3 盲区修正**：`StrPart::Interp(Vec<Token>)` 内嵌 span，重排后 kind 必不等——token_key 递归归零）+ 幂等。
   - **parser 单点扩展（API 缺口反向驱动，§2.1 既定原则）**：`FnDecl.span` 从名字起改 `fn` 关键字起（与 ProtocolFn 同口径）——impl 体内 fn 行首定位依赖；诊断定位顺带覆盖签名头。
   - **验收**：三形态 spike + examples 11 篇全量双断言 + 幂等 ✓；专项 19 条（规格化/块布局/空行坍缩/注释四形态/延续注释/struct_decl/管道/一元后缀间距/else 同行/空体行内/插值保真）；workspace 全绿（syntax 29 / fmt 19 / 全仓 158）；clippy 零告警；CLI 端到端回归 ✓。折行（100 列 + 尾随逗号）按 §6 留第二批。
6. ~~F2 CLI 接入~~ ✓ **F2 已落地（2026-10-13，`yoru fmt`，gofmt 风格三形态）**
   - `yoru fmt <file...>` 默认 stdout（gofmt 同款，管道组合）；`-w/--write` 原地写入；`--check`（别名 `-l`）只校验——有差异列出 `path: needs formatting` 并 exit 1（CI 直接可挂）；`-w` 与 `--check` 互斥、无文件 → usage rc=2（clig.dev 决策 65 契约）。
   - 解析错误文件诚实拒绝（`fmt only handles valid files`），打印诊断 rc=1——fmt 不掩盖非法源码。
   - **验收**：examples 11 篇格式化前后 `yoru run` 输出**逐位一致 11/11**（语义不变的最强形态——不是 diff 文本而是 diff 执行结果）；`-w` 后在盘幂等（`fmt -w` → `--check` rc=0 → `fmt` stdout 与盘上文件逐字节相同）；解析错误路径 rc=1 ✓；workspace 全绿 + clippy 零告警。
