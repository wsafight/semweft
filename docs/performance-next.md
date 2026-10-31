# 第二轮基础层开发

按顺序实现、测试，再更新记录。

1. [x] 冻结输入、多轮独立子进程基准、分位数、磁盘体积及同输入对照。冻结输入复用/不兼容对照拒绝测试通过；3 轮优化前记录为 `target/performance-before.json`，输入为 `target/performance-input.json`。
2. [x] 源码行索引接入发布校验和 SCIP；扫描参考实现逐边界对照、core 测试及 Unicode SCIP 集成通过。
3. [x] 采集与解析移出查询共享锁，保留写者顺序；发布前二次核对输入和配置。并发读锁可用、输入中途变化拒绝发布测试通过。
4. [x] Rust 监听刷新按元数据/事件复用文件内容，新增/改名/删除与完整采集对照通过。前台完整核对、监听每 5 秒强制校验；Go/Web/外部依赖继续完整采集。监听刷新在后台执行，不阻塞 IPC 接收。
5. [x] Rust/Go/Web 缓存完整文件事实，未变化文件不再提取 AST/骨架；编辑、改名、删除与冷构建对照及语言测试通过。
6. [x] 按文件缓存候选关系，目标声明集合变化跨文件失效；每次合并排序去重，缓存估计上限 64 MiB。与原完整扫描对照及跨文件目标变化测试通过。
7. [x] 版本化对象编码，旧原文/LZ4/Deflate 混读、解码长度限制、哈希校验；`snapshots compact` 原子迁移并可重复执行。143 对象测量：原文 20.94 MB，LZ4 3.75 MB/编码 11.7 ms，Deflate 2.36 MB/149.4 ms；默认 LZ4，小对象保留原文。存储全套及迁移测试通过。
8. [x] 分页载荷使用 Arc 共享，抽出分页数组，续页只复制当前切片；按唯一载荷估计字节和游标数量限制，TTL 清理、超大结果明确报错。嵌套关系覆盖/游标重放、动态禁用、预算与 context 遗漏续页测试通过。
9. [x] `snapshots usage`、`gc --dry-run --max-bytes N` 和 `index.max_disk_bytes`；按物理体积收缩可选历史，保护必要根，超标报告 budget_unmet。预览文件内容/mtime/序号不变、预览执行一致、固定版本/活动读者保护及 CLI 回归通过。
10. [x] 独立 `semweft-query` 承接查询、快照绑定、刷新与语言适配；CLI 保留命令/IPC/MCP/测量宿主。库内单测和直接查询、编辑、续页、恢复集成测试通过。

## 同输入实测

2026-10-31，本机 release 构建，59 个文件、约 801 KB 源码，10 次注释编辑；优化前后均用同一冻结输入、各 3 个新进程运行。输入为 `target/performance-input.json`，前后报告为 `target/performance-before.json` / `target/performance-after.json`，所有运行的查询校验与基线一致。以下为第 10 次刷新样本的 P50，MB 使用十进制：

|指标|优化前|优化后|
|---|---:|---:|
|解析与发布|441.4 ms|13.0 ms|
|持久化|169.6 ms|75.8 ms|
|该视图首次关系索引与查询|105.2 ms|69.9 ms|
|新进程恢复（OS 文件缓存温热）|148.4 ms|57.2 ms|
|本次对象写入|727,456 B|129,588 B|
|保留 10 个视图的磁盘体积|20.95 MB|3.75 MB|
|发布进程峰值 RSS|168.2 MiB|231.8 MiB|

缓存用内存换取重复刷新成本：本次峰值 RSS 增加约 38%，不能把字节预算等同于进程内存上限。首次进程冷查询 P50 为 123.3 → 124.1 ms，近似持平；第 10 个视图的首次查询可以复用前面视图的文件关系缓存，不能称为完全冷进程查询。关系缓存权重按结构体和字符串分配容量估计，避免为计量再次序列化全部边。三个样本的分位数仅供初步对照，不代表稳定性能门槛；本基准不覆盖构建、L3 或真实 rust-analyzer。

压缩比较使用同一批 143 个逻辑对象（20.94 MB）：LZ4 3.75 MB，Deflate 2.36 MB；默认选择编码更快的 LZ4。对压缩迁移也覆盖 rename 前后和目录同步后的进程退出/写入失败；5 种操作 ×3 个中断点 ×2 种故障方式均可恢复，并验证重试会忽略遗留临时文件。

## 最终回归

- `cargo fmt --all -- --check`：通过。
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`：通过。
- `cargo test --locked --workspace`：174 通过、0 失败、2 忽略。
- `cargo test --locked --workspace --all-features`：179 通过、0 失败、2 忽略。

忽略项需要外部 rust-analyzer/rust-src，本轮未运行这两项真实工具测试；SCIP 和语义持久化集成仍在默认回归中执行。日志位于 `target/performance-clippy.log`、`target/performance-tests-default.log`、`target/performance-tests-all.log`。
