# Changelog

本文件按 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 格式维护。
**面向用户写影响，不罗列内部改动**（决策 63）；每次变更同步更新，不发布时补写。

## [Unreleased]

### Added
- 阶段 0-4 交付：解析（含防爆栈三重闸 Y-S9501/Y-S9502）、类型检查、LBC0 字节码、T1 VM（资源三限 + 中断 + 观测）、CLI `yoru check/run`、宿主自由函数桥（`register_host_fn`，宿主 panic 边界 Y-H9001）。
- 工程卫生套装：双许可证、SECURITY、CONTRIBUTING、CI（含 MSRV 验证 job）。
- stdlib 内建注册表统一收口（决策 77）：26 条内建签名由 `yoru_stdlib::BUILTINS` 单一事实源驱动，typeck/lower/VM 三面对账；`math.sqrt/floor/ceil`、`string.upper/to_runes`、`json.parse/stringify`、`parse_int/parse_float`、变参 `print`、`assert` 可选消息参数、`for i in range(n)` 迭代全部可用。

### Fixed
- `string.upper` 等模块函数被 Y-T0007 误拦（typeck 面缺员）；`print` 签名面与 VM 实际行为不符；`assert` 第二参数被当必选；`range(n)` 在 for 循环中被 Y-T0002 误拦（VM 早已支持 Range 迭代）。

[Unreleased]: https://github.com/yoru-lang/yoru/compare/HEAD
