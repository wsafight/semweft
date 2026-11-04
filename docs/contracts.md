# Protobuf 契约影响分析

当前实现选择 Protobuf（清单允许 Protobuf、Thrift 或 OpenAPI 之一）；Thrift/OpenAPI 尚未实现。工具为 `contract_impact` / `semweft contract-impact --diff ...`，复用双侧输入、固定视图、分页与查询预算。

## 捕获与解析

Rust、Go 与 TS/JS 项目捕获纳入声明范围内的 `.proto` 文件，仍遵守排除、gitignore、symlink 与缓存边界。使用锁定的 `protox-parse@0.10.0`，不执行 protoc 或生成器；保留字段/RPC 的源码坐标及 file_hash。当前提供消息、嵌套消息、字段编号/类型/label/oneof/presence 与 RPC 请求/响应/streaming 的差异。

解析失败的文件在两侧比较中都排除，返回 gap，不能把失败侧的空解析结果解释为所有声明已删除。未执行跨文件描述符链接；枚举、扩展、自定义 options、生成 API 和 ProtoJSON 兼容性未建立。独立纯 IDL 工作区识别仍待接入，当前从语言项目分析契约与消费者。

## 兼容规则

- 同一消息中同名字段改变编号：`protobuf_field_number_changed`，确定的已声明 wire 兼容风险。
- 非 repeated 的标量字段改变成不同 wire family：`protobuf_wire_type_changed`。同 wire family 内的数值语义变化不自动判兼容。
- 删除 RPC：`protobuf_rpc_removed`，已声明 RPC 契约破坏，不推断是否存在运行中的消费者。
- 其他字段/消息/RPC 新增、删除、类型、presence 或 streaming 变化：`protobuf_contract_review`，要求进一步复核，不能判为行为兼容。

字段删除本身不直接宣称 binary-wire 不兼容；重新使用编号、生成源码、JSON 名称和业务语义需另外评估。空规则结果不代表覆盖全部兼容维度；整体结论保留 insufficient_evidence，除非有限规则已发现确定违规。CLI 退出码沿用 0/1/3。

规则依据：<https://protobuf.dev/programming-guides/proto3/#updating>（核对日期 2026-11-03）。

## 显式生成代码映射

```toml
[[contracts.mappings]]
idl = "proto/stock.proto"
contract = "demo.Stock.count"
symbol = "generated::Stock::count"
generator = "prost@0.14.4"
```

contract 为完整消息/字段/RPC 名称，symbol 为可唯一解析的已索引符号，generator 必须写明版本。字段映射也可通过消息级映射获得类型级候选。映射来自显式配置而不是生成器证明，因此不提升为 definite 字段读写。

逐侧返回 mapping、symbol、snapshot、完整反向关系及 witness path。只有精确字段映射且目标是 field 时标为 field_candidate；其余为 type_or_contract_candidate。透传不猜测实际读写了某个字段。缺映射、目标歧义、未索引生成物、运行时反射和外部服务均列 gap。超预算时宁可省略整个映射，也不截断证据链制造“完整路径”。

```sh
semweft contract-impact --diff WORKTREE --all-pages --format markdown
semweft contract-impact --diff BASE..HEAD --all-pages --format html > contract-review.html
```

`contract_summary`、verification 和 diff_identity 为分页前全量元数据；findings、contract_changes、contract_impacts 和复用 diff 的关系/模块列表共用分页 limit。生成器映射配置及证据来自各自固定快照，不随续页读取新磁盘内容。
