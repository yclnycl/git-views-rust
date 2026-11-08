# 性能与内存基线

基线用于回答「这个版本在同一台机器、同一组仓库上花多少时间和内存」。服务层与桌面进程分别测量；两者的范围不同，不能把服务层内存当成桌面应用内存，也不能把缓存预算当作进程内存上限。

## 一键运行服务层基线

Windows PowerShell 7、项目指定的 Rust 工具链和本机 Git 可用后，在项目根目录运行：

```powershell
pwsh -File scripts/performance-baseline.ps1 -Offline
```

脚本通过 `scripts/dev.ps1` 构建与便携发行版相同的 `x86_64-pc-windows-msvc` / static CRT release 服务层示例，使用本机 Git 生成无网络、无远端的合成仓库，再使用产品的 `RepositoryService<Git2Backend>`、预览缓存、文件索引和内容搜索实现测量。Git CLI 只负责生成夹具，不进入服务层计时或内存采样。夹具使用固定文件内容、作者、时间、分支、SHA-1 对象格式和提交历史；隔离用户及系统 Git 配置，使用空模板并关闭 hooks 和签名，不读取私人仓库。

| 夹具 | 已跟踪文件 | 提交 | 未暂存修改 | 未跟踪文件 |
| --- | ---: | ---: | ---: | ---: |
| small | 100 | 5 | 5 | 1 |
| medium | 1,000 | 20 | 50 | 10 |
| large | 10,000 | 50 | 500 | 100 |

文本文件约 2.3 KiB。夹具关闭自动 GC / maintenance，保留 Git 生成的 loose objects，避免后台打包影响测量。`large` 是本基线的相对命名，并不代表所有大型真实仓库。夹具不包含子模块、LFS、二进制大文件或复杂分支拓扑。

默认每次执行创建新的 `target/performance-baseline/<运行编号>/fixtures`，保留夹具与逐仓库原始报告，不会删除已有目录。主报告默认保存到 `dist/performance-baseline/current.json`。报告包括机器与构建信息、可执行文件 SHA-256、测量代码及构建输入 SHA-256、夹具 HEAD、文件数与字节数、每个阶段的原始样本、median、p95、缓存统计和进程内存采样。构建或测量期间输入发生变化时，脚本拒绝发布该次报告。

默认重新生成夹具；反复比较时可显式指定 `-FixtureDirectory target/performance-baseline/<已生成编号>/fixtures` 复用这一工具生成的夹具，减少准备时间。报告通过 `reused_fixtures` 记录此选项，实际状态和搜索工作量校验仍执行。生成过程可能需要几分钟；它不进入计时或内存统计。

## 计时与缓存口径

默认每个夹具运行 7 次。每次创建新的 service，预览缓存与内容搜索缓存从空开始；同一次迭代内再读取相同内容，取得缓存命中与重复调用数据。报告保留第一个进程样本，`observations[].first_in_process` 标记它。

**这里的 fresh/miss 代表新的服务对象或空应用缓存；操作系统文件缓存没有清空。** 夹具刚被写入，连第一轮也可能命中操作系统缓存。这不是冷启动磁盘性能测试，脚本不会要求管理员权限或清理系统缓存。

| 指标 | 包含的实际工作 |
| --- | --- |
| `metadata_fresh_service` / `metadata_repeat` | 仓库身份、HEAD 元数据 |
| `load_scan_fresh_service` / `load_scan_repeat` | 完整扫描：status、最近历史、分支/worktree 详情、复用 status 的文件树 |
| `status_history_repeat` | 单独读取 status 与最近历史；不包含文件树 |
| `tree_reusing_status` | 使用已有 status 构造文件树 |
| `source_cache_miss` / `source_cache_hit` | 同一已修改文件的源码预览，包含真实缓存签名校验 |
| `diff_cache_miss` / `diff_cache_hit` | 同一文件的未暂存 Diff 预览 |
| `path_index_build` / `path_search` | 文件搜索索引构造与固定路径查询 |
| `content_default_index_build` / `content_search_default_budget` | 使用产品默认的 3 秒时间预算查询固定不存在文本；保留每轮已扫描数、读取字节数、匹配数、超时/截断/取消/完整性标记 |
| `content_full_index_build` | 构建单独的全量遍历基准索引；只将时间预算明确改为 30 秒，其余文件、读取、缓存与线程预算沿用产品默认值 |
| `content_search_full_cache_miss` / `content_search_full_cache_hit` | **仅供基准的 30 秒预算**：查询相同不存在文本，验证遍历全部文件；第二次复用文本/指纹缓存，不能当成产品默认预算的延迟 |
| `preview_cache_pressure_96_files` | 连续打开 96 个源码文件，观察默认 48 项缓存的淘汰；报告另记录失效后统计 |

阶段存在包含关系，不能相加当成完整用户操作时长。例如 `load_scan_*` 已包含 status 和 tree。部分阶段短于 1 ms，易受调度噪声影响。程序使用高精度单调时钟，保留毫秒小数；median 在偶数样本时取中间两值平均，p95 使用 nearest-rank（向上取整）。默认 7 个样本的 p95 等于最慢样本；研究尾延迟时可使用 `-Iterations 21` 或更高，并重复完整运行。

夹具的 HEAD、状态、搜索候选数和读取字节会被校验。默认预算搜索可以如实返回部分结果，完整性标记位于 `observations[].content_default`；不能用扫描不完整的低延迟推断全量搜索更快。单独的 30 秒基准要求全量遍历成功，超时或部分结果会拒绝发布报告。**这只是测量工具的预算，产品的 3 秒预算没有改变。** 默认预算索引在创建全量基准索引前释放，避免同时保留两份内容缓存。缓存估算字节分别来自产品缓存和索引自身，未包含所有分配器/运行时开销。

## 内存口径

PowerShell 以请求间隔 10 ms 采样每个独立服务进程，记录：

- `working_set_bytes`：当时的进程工作集（驻留内存）。
- `private_bytes`：当时的进程私有提交字节；不是私有驻留集。
- `peak_working_set_bytes`：Windows 维护的进程峰值工作集。
- `sampled_peak_private_bytes`：采样中最大的私有提交值；采样间隙的尖峰可能遗漏。

实际采样间隔取决于调度，原始样本保存时间戳。服务进程包含 libgit2、搜索线程、缓存、串行执行的默认预算/全量预算工作负载、基线工具自身与结果分配；不包含 PowerShell、Cargo、生成夹具的 Git CLI、GPUI、GPU、窗口合成器或其他进程。两个内容索引不会同时保留。每个服务进程在工作结束后保留 200 ms，方便采样器读取 OS 峰值；这段时间不进入阶段计时或 `workload_elapsed_ms`。

## 桌面应用启动与空闲内存

完成服务层基线并构建桌面可执行文件后运行：

```powershell
pwsh -File scripts/app-performance-baseline.ps1 -Executable dist/GitViews.exe
```

也可将 `-Executable` 指向 `target/x86_64-pc-windows-msvc/release/gitviews.exe`。脚本为每个夹具执行 3 次独立启动，使用隐藏启动方式、全新的独立 `--config` / `--state-file`、独立日志目录，并在独立 `git-engine.toml` 中显式选择内置引擎。不会改动用户的日常配置与仓库目录列表。

产品在仓库扫描结果应用到 UI 模型后输出 `repository_ready`，包含 `load_ms`、文件数和变更数。脚本记录从进程启动到该事件的时间、发现日志的时间、事件内的加载耗时，以及事件后 2 秒内的工作集与私有字节；空闲值取最后 1 秒采样的中位数。事件时间避免把异步日志刷新延迟计入启动；日志观察时间保留用于识别采集延迟。这里的 ready 表示仓库模型已应用，**没有测量第一帧 GPU 呈现**。

每轮先对本轮创建的进程调用 `CloseMainWindow` 并等待退出；无法关闭时只终止本轮创建的进程，并在 `shutdown` 记录 `forced_after_close_attempt`。完整原始数据默认保存到 `dist/performance-baseline/app-current.json`，独立日志留在 `target/performance-baseline/app-<运行编号>`。

桌面内存是整个应用进程的观测值，包含窗口与 CPU 侧渲染资源；不覆盖 GPU 显存、系统窗口合成进程或后台系统服务。首次使用的引擎选择、克隆、认证与网络耗时不属于这组现成仓库基线。

## 与优化后的版本比较

```powershell
# 将已确认的报告放入版本记录；后续运行保持相同机器与 profile。
pwsh -File scripts/performance-baseline.ps1 -Offline -Iterations 7 `
  -CompareTo docs/performance/2026-11-08-baseline.json
```

`comparison` 输出每个阶段 median/p95 及进程内存的变化百分比，正值表示变慢或增加。夹具版本或 HEAD 不一致时拒绝比较；CPU 或构建 profile 不一致时提示比较口径不同。无需管理员权限。`-Profile debug` 适合开发验证，正式比较使用 release；`-SkipBuild` 仅用于已确认对应构建的现有二进制，并会明确记入报告。

测量前关闭高负载任务，使用相同电源模式和杀毒设置，确认基线代码/夹具版本一致，并重复运行确认变化超出本机波动。脚本不把单次变化自动判为性能回归；保留原始样本便于调查，不在用户启动流程中执行基准测试。

## 当前记录

2026-11-08 的服务层正式记录为 [2026-11-08-baseline.json](performance/2026-11-08-baseline.json)，采集时间 11:26（Asia/Shanghai）。机器为 Intel Core i7-10700 / 16 逻辑处理器 / 约 32 GiB RAM，Windows build 26300，PowerShell 7.6.5；使用便携目标的 static CRT release 构建、每夹具 7 次采样。执行时关闭其他构建与测试负载，复用本工具生成的夹具，操作系统文件缓存未清空。

| 夹具 | 完整加载 median / p95（ms） | 服务进程峰值工作集（MiB） | 服务进程采样峰值私有提交（MiB） |
| --- | ---: | ---: | ---: |
| small | 41.12 / 49.83 | 11.91 | 3.38 |
| medium | 75.25 / 82.43 | 16.96 | 9.00 |
| large | 346.77 / 379.92 | 45.94 | 40.29 |

内存列覆盖整个串行基准工作负载与工具自身，是服务进程观测值。源码缓存连续打开 96 个文件后均保留 48 项，失效后均为 0 项；这些计数不代表进程内存会回落到某个固定值。

实际默认预算内容搜索暴露了明确边界：10,100 个候选文件时，7 次查询均在约 3 秒时返回部分结果。

| 夹具 | 默认预算查询 median / p95（ms） | 完整查询次数 | 每次扫描候选数 |
| --- | ---: | ---: | ---: |
| small | 64.17 / 69.91 | 7 / 7 | 101 / 101 |
| medium | 640.66 / 645.82 | 7 / 7 | 1,010 / 1,010 |
| large | 3,001.11 / 3,001.35 | 0 / 7 | 4,418–5,369 / 10,100 |

以下是**仅供测量、30 秒预算**下完整遍历相同候选集的结果。产品仍使用默认 3 秒预算，不能将此表当成默认搜索完成时间承诺。

| 夹具 | 全量搜索缓存未命中 median / p95（ms） | 全量搜索缓存命中 median（ms） |
| --- | ---: | ---: |
| small | 62.92 / 73.46 | 36.55 |
| medium | 650.19 / 680.43 | 309.32 |
| large | 6,561.56 / 6,739.88 | 3,066.73 |

缓存命中查询均未重新读取文件正文（`read_bytes = 0`），仍需检查逐文件元数据。完整原始样本与完整性标记保留在报告中。另以每夹具 3 次采样执行了比较命令，生成 `dist/performance-baseline/comparison-validation.json` 的 60 行比较数据，引用校验和与二进制身份均通过；它用于验证比较流程，未将采样波动解释为优化效果。

桌面正式记录为 [2026-11-08-app-baseline.json](performance/2026-11-08-app-baseline.json)，采集时间 11:28（Asia/Shanghai），同一主机、每夹具 3 次独立隐藏启动。所测发行版的 SHA-256 为 `586dda25ab57ce891e6bd784e1f7b22472fcb9126eaf17c5335063e1b3fa8319`，与 `dist/p0-foundation/GitViews.exe` 一致。报告还记录了 NVIDIA GeForce RTX 3070 Ti、驱动版本与显示器分辨率，便于后续保持环境一致。

| 夹具 | 启动至仓库模型就绪 median / p95（ms） | 空闲工作集（MiB） | 空闲私有提交（MiB） | 峰值工作集 / 采样峰值私有提交（MiB） |
| --- | ---: | ---: | ---: | ---: |
| small | 679.85 / 3,389.60 | 105.23 | 247.46 | 108.38 / 251.03 |
| medium | 704.43 / 708.88 | 105.29 | 247.54 | 108.52 / 250.95 |
| large | 878.67 / 885.91 | 107.95 | 250.31 | 116.27 / 258.74 |

空闲列是 3 次独立运行的各自空闲中位数再取中位数；峰值列取 3 次运行最大值。保留了首次 small 样本的 3.39 秒启动延迟，没有剔除它。`ready` 事件代表模型应用完成，未测量第一帧呈现或 GPU 显存；隐藏窗口的数值也不能代表所有交互状态的内存。

本次 9 次隐藏启动均未通过 `CloseMainWindow` 结束，采样后使用了只终止本轮进程的兜底路径，报告逐轮记录 `forced_after_close_attempt`，所有本轮进程均已退出。因此这份启动/内存记录不作为应用正常交互关闭行为的验证。桌面测量使用产品默认预算；服务层的 30 秒全量基准预算不进入桌面应用。

正式记录以带日期的 JSON 副本保留，`dist/performance-baseline/current.json` 和 `app-current.json` 分别与对应副本逐字节一致。基线工具的定向 Clippy 检查日志为 `target/performance-baseline-example-clippy.log`；服务层正式采集、比较验证和桌面正式采集日志分别为 `target/performance-baseline-official.log`、`target/performance-baseline-comparison.log`、`target/app-performance-baseline-official.log`。
