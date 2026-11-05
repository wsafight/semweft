# gpui-rsx 实际使用反馈

当前环境：macOS，`gpui = 0.2.2`（`default-features = false`，启用 `font-kit,runtime_shaders`），`gpui-rsx = 0.11.0`（启用 `tailwind-scale`）。

当前没有待处理问题。已完成对应代码改造、针对性测试与原生桌面回归后，移除已解决条目。

2026-11-05 从 `0.10.2` 升级到 `0.11.0`，同步锁定 `gpui-rsx-core = 0.11.0`。现有消费代码通过编译，未发现需要改造的 API 或可移除的兼容写法。桌面 28 项测试（含 5 项 RSX 消费端回归）、Clippy 与格式检查通过；重新构建并签名的独立应用在受限 PATH、独立数据目录下通过冷启动和重启回归，覆盖十页视图、交互图、Base/Head 标签、异步分析、IME 与源码导航。IPC、历史源码与定义导航检查通过。

后续发现问题时，在此记录版本与 feature、最小复现、实际表现、影响及处理方案。升级时按“改造对应代码 → 针对性验证与桌面回归 → 移除已解决条目”的顺序处理，不能仅凭版本号清空记录。已有消费端回归见 [rsx_compat_tests.rs](../crates/semweft-desktop/src/ui/rsx_compat_tests.rs)。
