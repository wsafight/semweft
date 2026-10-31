# gpui-rsx 实际使用反馈

当前环境：macOS，`gpui = 0.2.2`（`default-features = false`，启用 `font-kit,runtime_shaders`），`gpui-rsx = 0.10.2`（启用 `tailwind-scale`）。

当前没有待处理问题。已完成对应代码改造、针对性测试与原生桌面回归后，移除已解决条目。

后续发现问题时，在此记录版本与 feature、最小复现、实际表现、影响及处理方案。升级时按“改造对应代码 → 针对性验证与桌面回归 → 移除已解决条目”的顺序处理，不能仅凭版本号清空记录。已有消费端回归见 [rsx_compat_tests.rs](../crates/semweft-desktop/src/ui/rsx_compat_tests.rs)。
