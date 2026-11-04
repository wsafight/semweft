# Go / TypeScript / JavaScript 正式支持验收状态

本页不取代 `language-support.md` 5.2 的正式门槛。工程入口完成或 fixture 通过，不能自动标记为正式支持。

## 已有可执行路径

|语言|当前能力|明确边界|
|---|---|---|
|Go|go.mod/go.work 项目与包摄取，语法与名称候选、SCIP 源清单导入，授权后 scip-go 固定输入项目重建、配置标签、编辑失效、失败恢复和撤权取消|当前是项目重建而非 gopls 增量数据库；interface、外部环境文件/依赖、生成物和仓库外消费者有缺口|
|TS/JS|package.json/tsconfig 摄取，TS/TSX/JS 等语法，JSONC/本地单继承 paths/baseUrl 模块候选，相对导入、literal import()/require()，授权后的 scip-typescript 重建与固定源清单校验|真实类型/别名绑定依靠 SCIP，不把 L1 路径猜测提升为类型绑定；package exports 条件、package extends、多继承、运行时 specifier 和外部消费者未穷尽|

Go 执行器使用 `SEMWEFT_SCIP_GO`（默认 scip-go），TS/JS 使用 `SEMWEFT_SCIP_TYPESCRIPT`（默认 scip-typescript）。工具须预先安装，Semweft 不自动 npm install 或下载项目依赖，不运行 package scripts。启用仓库执行仍需 `trust --accept-build-execution`。普通 syntax-only 和外部 SCIP 导入不因新增刷新入口获得执行授权。

两种重建都先记录当前源清单，生产器输出必须与捕获输入匹配；编辑、撤权、失败或输入变化时不把旧结果发布为 current。Go 重建保留 GOPROXY/GOSUMDB=off。TS/JS 使用官方 CLI 的 `index --output`，不能声称是常驻 tsserver。

Go 配置 `[go] tags`、冻结构建环境与历史复用边界见[语言支持](language-support.md)。Go SCIP 源清单也携带冻结环境；与捕获环境不匹配的导入被拒绝。互斥标签夹具已经验证真实调用目标随配置切换、旧快照保留和冷启动一致性。

## 真实生产器集成验证

2026-11-03 在本机使用 Go 1.27.2 / Node 24.5.0、真实 `scip-go@0.2.9` 与 `@sourcegraph/scip-typescript@0.6.0` 验证了临时双文件项目的跨文件直接调用绑定、Unicode 坐标、修改后目标替换、冷启动结果一致性与撤权。TS 用例同时经过 tsconfig 路径别名。生产器安装在临时目录，不改变用户项目依赖和宿主设置。

真实 TS 生产器暴露了 fixture 未覆盖的问题：0.6.0 未填写 `Document.position_encoding`，实际 range 来自 TypeScript UTF-16 坐标。导入器仅对该名称与确切版本启用可审计兼容，返回 `scip_position_encoding_compatibility` 诊断；显式 document 编码优先，未知生产器、版本或编码仍返回缺口，不把 metadata 文本编码误当位置编码。单元测试与真实 Unicode 用例共同验证这一边界。

`.github/workflows/ci.yml` 新增 `semantic-producers`，固定工具链/生产器版本并执行同组 ignored 测试。运行命令：

```sh
cargo test --locked -p semweft-cli --all-features --test mvp producers:: -- --ignored --nocapture
```

须预先提供 PATH 中的工具或对应 `SEMWEFT_SCIP_*` 环境变量。该 CI 作业已配置并在本机执行了等价测试，尚无本次提交后的远程 CI 运行记录。临时项目验证不替代真实仓库质量评测。

## 九个真实仓库的源码标注样本

2026-11-04 更新 [固定语料与运行工具](../examples/language-acceptance/README.md) 和 [本机测量 JSON 摘要](language-acceptance-results.json)，新增三个 JS-only 项目。标注依据源码及导入/接收者类型编写，目标不由 SCIP 输出生成；JS 标注先于其首次索引。当前由 Codex 编写，属于便利样本，未做独立人工复核和随机抽样。生产源码投影、提交与每个文件的 Git blob 身份均固定。

|语言|仓库|固定提交|参与解析的生产源码（另含构建输入）|
|---|---|---|---|
|Go|google/uuid|0f11ee6918f41a04c201eceeadf612a377bc7fbc|16 文件 / 1323 行|
|Go|cespare/xxhash|998dce232f17418a7a5721ecf87ca714025a3243|5 文件 / 408 行|
|Go|spf13/pflag|2e9d26c8c37aae03e3f9d4e90b7116f5accb7cab|36 文件 / 5560 行|
|TS|jridgewell/resolve-uri|caa7a299d6c688570773c81732f77d3c7035ded6|1 文件 / 311 行|
|TS|jridgewell/sourcemap-codec|d8dc7fc3f7602043f56f3efa801569f983d756ec|4 文件 / 576 行|
|TS|jridgewell/trace-mapping|8ee48a950fb730823fc6bc661390444437e4ef98|9 文件 / 1063 行|
|JS|lukeed/clsx|925494cf31bcd97d3337aacd34e659e80cae7fe2|2 文件 / 56 行|
|JS|lukeed/mri|e73e9f9d5b02124d14ac17dac2c4801687d3e99a|1 文件 / 119 行|
|JS|lukeed/klona|6ad153073b7529769010ddbde1938372e1702f5b|4 文件 / 213 行|

Go 每仓库选择 10 个静态调用；TS 共 25 个 direct 调用、5 个虚方法声明绑定；JS 共 28 个 direct 调用。另有 Go/TS 各 1 个参数回调和 JS 2 个回调/动态构造不确定案例，总计 92 个标注。初次评测发现 Go 具体接收者方法与包 selector 调用被统一降为候选，现已增加仅限 Go 具体函数定义的静态绑定派生。interface 方法、函数值与 TS 可覆盖方法保持候选/未解析，另有方法/接口对照测试。TS 的五个方法样本按源码语义归类为 virtual，单独统计声明绑定，不进入 definite 执行目标召回分母；曾有一个 sort 文本锚点误匹配 sorted 变量，已核对源码位置修正。JS 样本促成匿名函数调用所有者修复。该语料已参与适配器调试，不能当作独立留出集。

|本机样本指标|Go|TypeScript|JavaScript|
|---|---|---|---|
|L3 direct 准确率 / 召回|30/30 / 30/30|25/25 / 25/25|28/28 / 28/28|
|对应 Wilson 95% 区间|88.65%–100%|86.68%–100%|87.94%–100%|
|L3 direct 候选错误率|null：无候选预测|null：无 direct 候选预测|null：无候选预测|
|全部已标注调用中 L3 未解析|1/31，3.23%|1/31，3.23%|2/30，6.67%|
|L1 direct 候选错误率|12/42，28.57%|0/25，0%|60/88，68.18%|
|L1/L3 声明绑定召回|均为 30/30|均为 30/30，包含 5 个 virtual|均为 28/28|
|不确定/虚方法误报 definite|0|0|0|

四个未解析样本均为参数回调/动态构造；这不表示其他未标注调用、仓库外依赖或生成物已解析。L1 正确名称候选也不构成语义确定性。各来源分母与置信区间、逐例目标和完整性缺口计数保留在 JSON；候选错误率只覆盖源码目标确定的 direct 标注，不评价不确定候选集合是否穷尽。另一个真实生产器对照证明 scip-typescript 0.6.0 未提供匿名默认导出函数的导入目标：函数体内调用可对齐，导入方调用保持未解析；具名默认导出可对齐。这一夹具不混入上述源码样本分母。

测量环境为 macOS Darwin 25.6.0、Mac15,7 / Apple M3 Pro、12 逻辑核、36 GiB RAM、Cargo Release 构建。生产器与工具链版本同上；TS/JS 使用报告中记录的 ES2018 src-only 配置，JS 开启 allowJs/checkJs，未安装被测仓库依赖，未覆盖原始完整构建。

|耗时（ms，P50 / P95 / P99）|Go|TypeScript|JavaScript|
|---|---|---|---|
|新解析器全文件分析，10 次/文件|0.699 / 2.496 / 12.156（570 样本）|0.510 / 3.516 / 3.584（140 样本）|0.387 / 1.748 / 1.838（70 样本）|
|编辑后显式扫描至语法发布|124.495 / 269.844 / 269.844（6 样本）|110.978 / 133.711 / 133.711（6 样本）|117.320 / 127.942 / 127.942（6 样本）|
|显式扫描至语义 current，含语法发布|998.111 / 1277.194 / 1277.194（6 样本）|1433.648 / 1544.096 / 1544.096（6 样本）|1384.147 / 1408.578 / 1408.578（6 样本）|

九仓库各两步编辑与同源码全新索引的关系、确定性及证据来源/确定性比较，18 步均无差异。Go 的 go.mod 修改语言版本为 1.22；TS/JS 修改 strict 配置。耗时不含文件监听/debounce。六个编辑样本的 P95/P99 均为最大值，不能代表生产尾延迟；Go 解析存在 158.660ms 的单次最大值，不能以 P99 达标掩盖该慢样本。此前 Debug 数据不能与 Release 混用。生产器输出哈希差异属于来源身份，原始哈希仍保留，不把它等同关系差异。

## 真实文件监听与编辑一致性

真实 CLI `index --watch` 在九仓库上依次执行函数体、跨文件辅助函数重命名、构建配置三步编辑，Go 切换 purego，TS/JS 切换 ES2020。只通过 status 观测新哈希、活跃 lease、存活 watcher 和匹配的语法/语义 epoch，随后固定快照查询所选根的三级调用关系，与独立临时路径的冷启动结果比较。27 步均通过；三个 Go 生产源码投影最终的离线 `go test ./...` 也通过。所选子图、哈希、观测状态、来源/确定性身份和构建输出保留在[监听结果摘要](language-watch-acceptance-results.json)。计时包含 50ms 轮询及 CLI/IPC 开销，只是本机观测上界，不代表所有关系和生产尾延迟。

|监听耗时（ms，P50 / P95 / P99；每种语言 9 步）|Go|TypeScript|JavaScript|
|---|---|---|---|
|写文件至观测到新语法|160.874 / 269.693 / 269.693|158.187 / 176.672 / 176.672|130.981 / 170.749 / 170.749|
|写文件至观测到语义 current|780.029 / 1845.597 / 1845.597|1012.290 / 1147.137 / 1147.137|953.932 / 1144.438 / 1144.438|

每组只有九步，P95/P99 均等于最大值；本机显式扫描评测与监听评测分别执行，不能把两次运行的差值当作固定监听开销。

首次真实监听测试暴露 TS/JS 源文件未进入 pending 集合，worker 因而跳过刷新；已补齐八种扩展名，并用常规核心回归和真实监听重跑验证。该问题的修复不等于此前独立的 Go lease/三秒启动偶发问题已修复。

`semantic-producers` CI 已加入固定提交下载、Release 样本质量断言、冷/热比较、真实监听及 JSON/日志工件；尚未取得这次更改的远程运行记录。

## 尚未完成的正式门槛

- 独立复核和扩大真实任务/仓库覆盖，包含更多构建条件、生成物和未解析机制；当前小型调试样本及其区间不足以证明总体 ≥99% / ≥95%。
- 真实生产器与九仓库作业的远程 CI 结果，本机执行不能代替远程运行记录；当前工作区没有配置 Git remote。
- 更广的固定编辑序列、完整原始项目构建和依赖替换；当前覆盖源码投影、上述配置与所选监听子图，不是任意工程全图验收。
- 尚未建模的语义机制和生成物的适用范围审核。

capabilities 继续返回 experimental，不宣布清单 18、19 的“正式支持”验收通过。当前是项目重建路径，未验证 gopls/tsserver 常驻后端；工程实现、源码样本与本机质量结果均保留其实际边界。

官方适配参考（核对日期 2026-11-03）：

- <https://github.com/scip-code/scip-go>
- <https://github.com/sourcegraph/scip-typescript>
- <https://www.typescriptlang.org/docs/handbook/modules/reference.html>
- <https://www.typescriptlang.org/tsconfig/paths.html>

## Go 生产器启动超时定位（2026-11-04）

本次复现定位到本机 macOS 直启新建可执行 shebang 脚本的启动路径：子进程已经创建，但停留在 dyld 入口或加载器同步通知路径，尚未执行脚本第一条语句，测试的 3 秒启动标记等待因此超时。该测试使用假生产器脚本，不运行真实 scip-go，也不执行 Go 构建。

没有在这些失败样本中观察到刷新请求丢失。具体哪个系统服务或观察者造成加载器等待，现有证据还不能确定。

### 引擎与进程证据

先保留原来的 3 秒启动、2 秒撤权限时，以 32 并发重复原用例：120 次中 76 次启动标记超时。失败时 status 已显示 `trusted`、`refreshing`，待处理文件为 `shop.go`，没有 `failure_reason`。首次批次失败后存在假生产器继续运行到自身结束的清理问题，因此此批次只用于确认可以复现，不作为独立样本的性能统计。

随后临时记录授权、发布持久化、刷新入队、worker 消费和子进程创建时序，并增加失败时撤权清理。64 次中仍有 15 次启动超时。一个失败样本中，刷新入队到 worker 消费仅 0.542ms，worker 消费到开始刷新约 88.8ms，`Command::spawn` 仅 1ms。进程已返回 PID，但放在脚本首行的内建 `printf` 尚未生成标记。原来的外部 `touch` 还没有机会执行。

另外 32 次带采样复现有 16 次超时。超时进程的 `ps` 状态为 `S`，命令为 `/bin/sh`，已存在约 3 秒；采样中有：

```text
Process: sh [45556]
Launch Time: 2026-11-04 13:19:59.691 +0800
Sample Time: 2026-11-04 13:20:02.652 +0800
903 Thread_60930388: Main Thread
  903 _dyld_start (in dyld) + 0
```

另一进程的采样同时包含以下加载器路径：

```text
dyld4::ExternallyViewableState::triggerNotifications
dyld4::RemoteNotificationResponder::blockOnSynchronousEvent
dyld4::RemoteNotificationResponder::sendMessage
mach_msg2_trap
```

这是执行到脚本正文之前的等待。不能仅凭这些符号把责任归到某个具体系统服务。把 Tokio worker 数设为 1 的对照也仍然超时，不能将默认线程数定为根因。

### 脱离 Semweft 的最小对照

最小脚本只执行内建标记写入，然后等待；每次生成新的私有临时目录和可执行脚本：

```sh
#!/bin/sh
printf entered > entered
sleep 30
```

比较直接执行脚本与显式执行 `/bin/sh producer.sh`，均使用相同进程组、输出捕获、3 秒首语句等待和终止回收方式。每批 32 并发，第二轮倒转两种模式的执行顺序，避免只用先后顺序解释差异。

|模式|执行次数|3 秒内进入正文|未进入正文|最大 spawn 耗时|
|---|---:|---:|---:|---:|
|直接执行新建脚本|64|36|28|14.113ms|
|显式调用已有 `/bin/sh` 读取脚本|64|64|0|12.889ms|

显式解释器模式的最大首语句等待为 19.458ms。直接执行模式的 28 个超时样本在终止前都没有退出；记录中的约 3,000ms 是观察截止时间，不是其最终启动耗时。此前单批最小对照也复现过直接模式 32/32 超时、显式解释器 0/32 超时。

最小对照不包含 Semweft、SCIP、Go、信任检查、索引持久化或刷新队列；当前可以将原用例的这类失败归为本机新建脚本直启路径的就绪超时。该结论不提供真实 scip-go 或其他操作系统的启动指标。

复现需要 POSIX 系统、`/bin/sh` 和 Python 3.11+：

```sh
python3 examples/diagnostics/producer-startup.py \
  --jobs 32 --rounds 2 --deadline-seconds 3 \
  --output /tmp/semweft-producer-startup.json
```

脚本按截止时间终止并回收自建进程，输出逐次观测和汇总。超时比例依赖本机状态，不要求其他机器复现相同比例。机器信息、128 次最小对照观测、引擎复现批次及采样摘要保存在 [结果 JSON](go-producer-startup-results.json)。

### 本轮代码处理与边界

新增上述独立诊断脚本；三个使用慢 Go 假生产器的测试增加失败时自动撤权清理，成功路径在显式撤权完成后解除清理。这样首次就绪超时不会留下 30 秒生产器，持续增加后续测试负载。临时引擎日志、进程 `ps/sample` 采样及实验环境变量均已清除。

本轮保留生产器执行方式和原来的 3 秒启动、2 秒撤权限时。已定位的是该超时发生的阶段与可独立复现的脚本启动路径，尚未消除 macOS 的加载器停顿，也没有把跨平台或真实生产器性能标为验收通过。后续测试边界应分别验证调度/进程创建、假生产器就绪和撤权回收，避免把整段主机加载器等待都当作引擎排队时间。

清理改动后的 CLI 全 feature 常规回归为 67 通过 / 10 忽略，无失败；忽略项未另行运行。工作区全目标/全 feature Clippy、格式和 diff 检查通过，独立诊断脚本已实际运行并核对 128 条观测与汇总一致。32 并发启动复现中的预期失败另行记录，不混入常规回归的通过统计。
