# GitHubTurbo 真实测速平台

需要 Python 3.8+、Git、curl；这些仅供测试平台使用，日常加速器不依赖 Python。

双击本目录 `Start.cmd`，打开 `http://127.0.0.1:18190/`。平台只监听回环地址，测试记录留在本机。首次建立基线可运行 `python bench/bench.py freeze`；已有基线时拒绝覆盖，并在每次测试前核对其 SHA256。

## 测试内容

- 网页 HTML、API、固定内容的 Raw/CSS、Release 小文件、1 MiB Range、13 MiB 完整 Release，以及真实 Git 浅克隆、fetch 和 fsck。
- 独立 DNS 直连、冻结基线、当前构建、当前配置的 GitPort、可选 HTTP 10808；未启动的外部程序不会由平台自动开启。
- 版本对照，以及同一 exe 的单项开关对照：TLS 探测复用、失败探测记忆、分段流水线、单流/分段竞速。
- 各次成功/失败/超时、内容校验、DNS/TCP/TLS/首字节/总耗时、有效吞吐、中位数、观测 P95、配对比值、引擎内存采样、实际二进制哈希与配置。
- 实时进度、停止、历史记录、JSON 和离线 HTML 报告。不同 CLI/网页运行共用文件锁，避免测试任务互相抢带宽。

## 命令行

0.4.1 已把自动分段改为默认关闭；新增“仅分段下载开关”实验。需要排除选路波动时，可通过 `--fixed-ips` 的 JSON 主机/IP 映射固定两边的公网候选，报告会记录这组控制变量。不要把测试用地址长期写入日常配置。本次 Release 回归记录见 `docs/release-regression-0.4.1.md`。

在项目根目录运行：

```powershell
python bench/bench.py serve --open
python bench/bench.py run --routes baseline,candidate,direct --cases raw,css,release-small,git-clone --rounds 3 --phases both
python bench/bench.py run --feature probe_reuse --cases raw,css,release-small,git-clone --rounds 3 --phases cold --require-success
python bench/bench.py run --feature adaptive_split --cases release-full --rounds 3 --phases cold --timeout 60
python -m unittest discover -s bench -p test_bench.py -v
```

`--candidate` 可以指定独立构建；`--baseline` 可指定另一版本。单项实验自动让双方使用同一个候选 exe，仅将对应选项分别设为 false/true。`--require-success` 令内容失败或超时返回退出码 2；默认命令完成并不表示所有网络请求成功，应查看成功率。

流水线实验会在两侧同时关闭首批竞速和探测记忆，确保实际进入分段；首批竞速实验在两侧同时关闭探测记忆。共同条件会写入报告，双方仍只有一个选项不同。旧构建可能尚不支持这些实验选项。自然复访仍可能遇到连接过期或缓存失效，以逐次命中及连接统计为准。

## 测量边界

加速器测试实例使用私有 CA 和空闲端口，以 `serve` 启动，不执行 enable，不导入系统证书，不改变 hosts、NRPT、用户 Git 或 GitPort 设置。curl 明确连接测试实例并验证其 CA；上游继续验证真实主机名。Git 使用隔离配置和 HTTP/1.1 前端，上游仍可协商 HTTP/2。clone 计时包含进程及对象接收；fetch 单独记录。

“直连”使用独立 DoH 的第一个公网 IPv4 并固定到 curl/Git，绕过本地解析接管；curl 最终对端必须匹配解析结果。DNS 准备单独计时。这不是 Windows 原始 DNS 污染状况的测试，也不是一组 IP 中的最佳直连成绩。

每轮顺序交替，轮次之间轮换先后。冷状态清除测试实例的线路、连接池及缓存；不会清除 OS TLS、解析器或远端 CDN 缓存。每次 curl 都是独立进程，热状态复用的是加速器内部的上游连接/缓存；直连热状态只是对同一资源再次请求，不复用 curl 的 TCP 连接。热状态重用该路线前一次同场景的连接/缓存，其他路线的样本可能穿插其间；单独热测试会先进行不计入结果的预热。

只有双方都成功且内容正确的同轮同场景同状态样本才计算配对耗时比值，失败次数始终保留。P95 在三轮时只是三次观测的最大值，不能当作长期尾延迟。缓存命中的有效吞吐不能当作公网带宽。HTML 请求不等于浏览器完整资源加载或 FCP。小仓库 clone 不代表大仓库吞吐。

完整 Release 每次约 13 MiB；双路线三轮冷热测试至少约 156 MiB，试探和重试会增加流量。界面默认只选小文件。可在 `cases.json` 维护带版本/哈希的公开测试集；资源发生变化时应更新清单，不能关闭校验来让样本“成功”。

`runs/<id>/report.json` 保存结果与测试清单，`report.html` 可离线查看。测试实例的 CA 私钥位于相邻 `private-*` 目录；分享时使用界面的报告导出。程序不会把请求 Cookie、认证头、签名跳转 URL 或私钥写入报告。原始临时下载正文和响应头在测量结束后删除。

## 本轮实验索引

### 不启动加速器的 IP 诊断

`python bench/ip_probe.py --url https://github.com/fastgh/fgit/releases/download/v1.2.0/fgit.exe` 可直接比较系统解析、当前 DNS 组及旧候选。这个专项脚本额外使用 Node.js 内置 HTTP/2（本机实测 v22.21.1），日常加速器和原有平台不增加 Node.js 依赖。它先筛查 1 MiB，再交替校验完整文件；保持本地加速器关闭，不改系统设置、不执行下载文件、不导出临时签名 URL。详细结果见 `docs/ip-selection-fgit.md`。

- A/A 与独立直连：`20261108-182005-137a68`，72/72 成功。
- 单项 TLS 探测复用：`20261108-183131-6c9ff3`，24/24 成功。
- 分段流水线：`20261108-183206-6c0981`，12/12 完整文件校验成功，公网收益不稳定，默认关闭。
- 探测记忆：`20261108-183410-ebed82`，保留超时及失败；主要用受控慢 Range 测试验证避免重复探测。
- 单流/分段竞速：`20261108-184109-fe70ff`，6/6 完整文件校验成功，三对中两对更快。
- 慢连接救援：`20261108-234528-468f33`、`20261108-235646-8c77aa`，对照冻结的 0.5.0，详见 `docs/flow-rescue.md`。可用 `--feature flow_rescue` 做单项对照。

早期调试运行 `181735` 使用固定先后顺序，仅留作调试记录；正式校准使用上面的交替顺序运行。GitPort 在本轮后续检查时没有监听 7810/7811，未将不可用的参考程序作为性能排名依据。


0.5.0 增加 `release-fgit`：用户给出的 v1.2.0/fgit.exe 完整下载，7,531,520 字节。固定 SHA256 来自 TLS 校验下载的本地一致性参考，不是发布者签名；只下载校验，不执行。报告包含网页/下载线路、来源刷新状态和完整传输学习记录。`direct` 代表独立 DoH 后直连，不等同于 Edge 的系统 DNS、HTTP/3 和缓存。

可用 `--baseline bench/binaries/v0.4.1.exe` 对照本轮保留的 0.4.1（如本地有该快照）；默认基线仍为不可覆盖的 0.3.0。基线配置随 exe 冻结。
