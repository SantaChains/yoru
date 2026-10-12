# 安全策略（Security Policy）

## 支持版本

| 版本线 | 支持状态 |
|---|---|
| 0.1.x（dev 渠道，git main） | 仅最新 main 接收安全修复 |
| stable（tag 发布） | 当前 tag + 上一 tag |

pre-1.0 阶段无长期支持承诺；安全修复随 main 滚动，stable 线视严重程度 backport。

## 报告漏洞

**请勿通过公开 issue 报告安全漏洞。**

- 通道：仓库 SECURITY advisory（GitHub "Report a vulnerability"）或维护者私信邮箱（首次公开发布时补充具体地址）
- 响应时限：72 小时内确认收到；7 天内给出初步评估
- 处理纪律（决策 67 条 6）：先补丁、后披露；确认细节在补丁发布前不公开；受影响版本视严重程度 yank
- 范围：VM 资源限制逃逸（内存/指令/调用深度）、`unsafe_code = "forbid"` 之外的未定义行为、诊断信息泄漏宿主机密、LBC0 验证器绕过

## 威胁模型要点

Yoru 面向宿主嵌入场景：脚本是**不可信输入**。VM 三限（决策 71）、循环引用深度截断、LBC0 验证器 fail-closed 是安全边界；这些边界的绕过即安全漏洞。
