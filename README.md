# Yoru

**AI 原生、宿主嵌入、面向数据的自制语言**——编译快 > AI 错误少 > 热重载 > 其他。

```text
yoru run hello.yoru        # 解释执行
yoru check hello.yoru      # 类型/语义检查，结构化诊断（Y-xxxx 错误码）
```

**嵌入 Yoru**（宿主注入 + 双向调用，阶段 4 宿主桥）：

```text
cargo run -p yoru-cli --example embed_host   # 完整嵌入示例（typeck 签名面 + VM 闭包 + Rust 调脚本）
```

## 语言速览（doctest 契约块）

以下 ` ```yoru ` 块由 `yoru doctest`（28 号 §2.3，C8 防线）在 CI 中**逐位执行验证**：
`// =>` 行是期望 print 输出，`// error:` 行断言诊断码集合——文档示例坏了 CI 红。

```yoru
fn main() -> int {
    let x = 21
    print(x * 2)
    return 0
}
// => 42
```

```yoru
fn main() -> int {
    let xs = [3, 1, 2]
    print(len(xs))
    return 0
}
// => 3
```

check 块只验诊断、不执行（码集精确比对，写全防漂移）：

```yoru
fn broken() {
    let x: int = "s"
}
// error: Y-T0002
```

## 仓库地图

| 路径 | 内容 |
|---|---|
| `crates/yoru-syntax` | 词法 + 语法分析（递归下降，防爆栈三重闸） |
| `crates/yoru-typeck` | 类型检查与诊断（单一签名闸口） |
| `crates/yoru-bytecode` | LBC0 字节码 lowering 与验证器 |
| `crates/yoru-vm` | 栈式 VM（资源三限 + 中断 + 观测） |
| `crates/yoru-ffi` / `yoru-stdlib` | 宿主桥与标准库（阶段 4） |
| `crates/yoru-cli` | `yoru` 命令行入口；`examples/embed_host.rs` = 嵌入示例 |
| `crates/yoru-fmt` / `yoru-fuzz` / `yoru-doctest` | 工具链：formatter（F 线）/ crash-fuzz（V0）/ doctest（D 线，28 号 §2.3） |
| `L语言设计/`（仓库外同级目录） | 设计文档体系 00-22 号（决策记录 11 号、路线图 20 号） |

## 文档排版硬规则（21 号 §7）

1. **头部三件套**：标题、状态行（定稿/被取代 + 来源轮次）、约束行（优先级向量）——新增文档一律套用；
2. **被取代必须留痕**：被取代文档头部加显式横幅指向取代者，永不静默删除；
3. **表格优先、来源可溯**：取舍用表、结论带决策号、调研带来源。

## 贡献与安全

- 提交纪律与 PR 检查单见 [CONTRIBUTING.md](CONTRIBUTING.md)；
- 安全漏洞**勿用公开 issue**，走 [SECURITY.md](SECURITY.md) 通道；
- 变更影响按用户视角记录于 [CHANGELOG.md](CHANGELOG.md)。

## 许可证

双许可证：[MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE)。
