# Yoru

**AI 原生、宿主嵌入、面向数据的自制语言**——编译快 > AI 错误少 > 热重载 > 其他。

```text
yoru run hello.yoru        # 解释执行
yoru check hello.yoru      # 类型/语义检查，结构化诊断（Y-xxxx 错误码）
```

## 仓库地图

| 路径 | 内容 |
|---|---|
| `crates/yoru-syntax` | 词法 + 语法分析（递归下降，防爆栈三重闸） |
| `crates/yoru-typeck` | 类型检查与诊断（单一签名闸口） |
| `crates/yoru-bytecode` | LBC0 字节码 lowering 与验证器 |
| `crates/yoru-vm` | 栈式 VM（资源三限 + 中断 + 观测） |
| `crates/yoru-ffi` / `yoru-stdlib` | 宿主桥与标准库（阶段 4） |
| `crates/yoru-cli` | `yoru` 命令行入口 |
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
