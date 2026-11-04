# 服务映射、依赖与路径

在 semweft.toml 声明服务拥有的捕获路径：

```toml
[[services]]
name = "orders"
paths = ["crates/orders/**"]

[[services]]
name = "inventory"
paths = ["crates/inventory/**"]
```

```sh
semweft map --dependencies service --cycles --all-pages
semweft path orders inventory --scope service --all-pages
semweft path src/handler.rs src/store.rs --scope module --max-depth 5
semweft impact --diff WORKTREE --all-pages --format html
```

MCP 分别使用 repo_map 的 `dependencies="service"` 和 find_paths 的 `scope="service"|"module"`。默认 scope=symbol 保留符号调用路径；module 的端点为捕获的完整相对文件路径。服务名字必须存在，路径不可越界，同一捕获文件不能属于多个服务；重叠会返回 invalid_argument。每组最多 256 服务、每服务最多 256 glob，名称唯一且不为空。

服务配置与分析快照一起冻结，工作区外的显式配置同样冻结；Git 版本读取版本内配置，不把当前映射套到历史。相同源文件可重用既有语义结果；映射上下文不同时发布新的冻结视图并保留 semantic_reused_from 来源，不为修复元数据而丢弃已验证的语义结果。响应 `service_mapping` 含配置、mapping_digest、映射文件数量及未映射关系数。服务依赖来自已索引跨服务符号关系，保留 relation_ids 和完整逐边 evidence；未映射文件不臆测归属，不通过包名猜测网络服务。

循环检测对这一冻结图计算强连通分量并保留分量内所有依赖。只有每个方向均有当前 definite 关系支持才返回 definite 分量，其他为 candidate。分量表示存在环，不枚举所有简单环；无环结论不覆盖未索引依赖。跨包依赖可以是配置候选，服务间 RPC/网络/消息依赖不会从字符串中自动补齐。

module/service 路径是有调用证据的聚合依赖路径，**每一跳都对应已索引调用，但相邻跳可来自不同中间函数，不能解释为一条实际执行调用栈**。保留过滤、深度/节点/路径预算、超时、frontier、固定续页与重查；candidate/pending 不升级成确定。显式映射不使实验语言自动成为正式支持。

diff 返回 `affected_services[]`（base/head 分开、变更文件、候选影响符号、映射摘要），`affected_modules` 增加 service，`service_impact_summary` 列出未映射变更。配置删除或重分配保留历史侧归属。仅聚合已捕获文件和候选影响，不声称掌握运行时消费者。

测试涵盖三服务环、逐边证据、模块/服务路径、深度/确定性过滤、固定分页、编辑后消环、diff 服务聚合、非法端点、重叠与越界拒绝。
