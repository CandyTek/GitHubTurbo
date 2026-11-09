# GitHubTurbo 0.6.0

面向 Windows 的 GitHub 本地加速器测试版。默认独立运行：本地 DNS、IP 优选、HTTPS 反代和 SNI 兼容策略共同改善 GitHub 网页、Raw、静态资源、Release、源码压缩包及 Git HTTPS 访问。额外 HTTP 代理是可选项，默认 `upstream: null`。

## 普通用户使用

解压发行包到固定目录，双击 **Start.cmd**（或 github-turbo.exe），接受 Windows 管理员提权后启用。启用过程会安装本机专属 CA、配置 GitHub 域名接入规则和当前用户 Git/SSH 的证书及转发设置。完成后继续使用原来的 GitHub 地址，不需要给每次 clone 加代理参数。

本地状态面板：<http://127.0.0.1:18080/>。

- **Stop.cmd**：停止引擎，恢复 hosts、Git/SSH 配置，移除本程序创建的域名规则、证书信任和恢复任务。
- **Recover.cmd**：启动失败、异常中断后的独立恢复入口。
- **系统托盘**：双击打开面板，右键可停止加速并恢复设置；关闭面板不停止加速。
- **管理面板**：实时切换 HTTP/2、公共资源缓存、分段下载、内置 IP 候选；查看连接复用、线路冷却与事件，导出诊断。
- 引擎意外退出时，原生守护进程自动执行恢复。断电/重启后，在同一用户下次登录时执行恢复任务。当前不会自动开机启用加速。

启用期间保留整个目录，尤其是 `state/session.json`。该目录保存恢复记录及本机私钥，不应发布、同步或与他人共享。发行包不包含它。

## 默认做了什么

| 层面 | 实现 |
| --- | --- |
| 系统接入 | Windows NRPT 为 GitHub 域名及子域名指定本地 DNS；常见域名与既有冲突 hosts 条目做可恢复映射 |
| 独立解析 | Cloudflare、Google、阿里、DNSPod DoH 并发获取 IPv4/IPv6，阿里 DoH 额外采样不同 EDNS 子网；固定解析器启动地址，保留 TLS 校验，避开被接管的系统解析 |
| IP 优选 | 实时 DoH 优先，hosts 与内置候选延迟兜底，每组最多探测 8 个分散候选；验证原域名证书；标准 SNI 优先，无 SNI 延后竞争 |
| 线路健康 | 按域名及线路记录延迟、完整大响应吞吐与失败冷却；421、429、5xx 和连接错误分别处理；限流不触发重放；网页线路短期落盘，下载每次启动重新学习 |
| 本地 HTTPS | 每台机器独立 CA；客户端到本机、到真实 GitHub 两段 TLS；上游始终校验证书链与原始主机名 |
| HTTP 转发 | 前后端 HTTP/2，复用已验证的 TLS 探测连接，跨客户端复用同域名上游连接；不支持时使用 HTTP/1.1；流式请求/响应、Range、Cookie、认证头与独立 HTTP/1.1 WebSocket 升级；强制 Host 与 TLS 目标一致 |
| 连接与缓存上限 | 最多 96 条共享上游连接、64 个池；网页会话池最多 1 条连接，下载/Git 池最多 4 条；网页连接闲置 300 秒回收，下载连接闲置 90 秒回收；公共静态缓存最多 16 MiB、128 项、单项 2 MiB，不缓存带 Cookie/认证/签名参数的请求或账号网页 |
| 失败回退 | 真实 GET/HEAD 收到 5xx 后，有限尝试其他 IP/TLS 方式；请求有正文或为 POST 时不自动重放 |
| Release | 标准下载入口 5xx 时可匿名尝试官方 Release API；可手动开启实验性 4 路直连 Range 下载，默认保持普通单流，逐段检查强 ETag、Content-Range 与长度；不使用公共第三方镜像 |
| Git | 普通 HTTPS clone/pull/push 走本地反代；当前用户 OpenSSH 为 github.com/ssh.github.com 设置专用 ProxyCommand，连接官方 SSH 443 入口 |
| 可选上游 | 配置本机 HTTP CONNECT 出口后，与直连共同参与选路；上游故障不影响未指定 proxy 模式的直连尝试 |
| 恢复 | 持久化会话记录、原生进程守护、登录恢复任务；hosts 无并发变化时按字节恢复，有变化时撤回本程序标记并保留外部编辑 |

HTTP 明文的 GitHub URL 会重定向到 HTTPS。本地 DNS 对 GitHub 域名返回 IPv4 回环，真实上游仍可使用 IPv6，但只在确认 IPv6 可用之后。本地检查不发送任何数据：路由必须使用原生全局地址，Teredo、6to4、唯一本地和链路本地地址都不算。通过后，还需一次 TCP 握手（最多 1.5 秒，每 30 分钟重复）连通公共 IPv6 解析器；在此之前只使用 IPv4。连续三次 IPv6 连接失败会停用 IPv6 十分钟。停用期间不发 AAAA 查询，也不尝试任何 IPv6 地址（包括缓存中的）。面板底部和 `/status` 的 `ipv6` 字段显示当前状态。HTTP/2 开关控制上游协商，前端同时支持 HTTP/2 和 HTTP/1.1。

## 0.6.0 更新内容

- **慢连接救援**（`flow_rescue`，默认开启）：大文件下载卡住时，从中断处改用新连接继续。见下文“慢连接救援与缓存”。
- **连接预热**（`prewarm`，默认开启）：启动时预先连接常用 GitHub 域名，使用期间保持一条连接就绪。
- **Git 克隆竞速**（`git_race`，默认关闭）：实验功能，见下文。
- **`github-turbo.exe git <参数>`**：不启用系统接入，单独让一条 git 命令走加速引擎。
- **IPv6 检测**：只有确认本机网络 IPv6 可用后才使用，连续失败会暂停。没有 IPv6 的电脑不再发 AAAA 查询，也不再尝试 IPv6 连接。
- **DoH 更快**：超时更短，失败的解析服务跳过 60 秒，结果在后台刷新。
- **面板**：新增“开源许可与致谢”面板（默认折叠，页脚有入口），页脚显示 IPv6 状态。
- 发行包现在包含 `README_zh.md`，每个打包的依赖都附带许可证文本。

0.5.0 的配置文件可以继续使用，新选项取默认值。

## 0.5.0 选路调整

实时 DoH 优先开始连接，400 ms 后才启动 hosts 与内置候选兜底；拿到可用解析后立即连接，慢解析服务在后台继续补全。候选分散取样，避免表中前一段 IP 挤掉后面的地址。

后台每小时更新 [jianboy/github-host](https://github.com/jianboy/github-host)、[GitHub520](https://github.com/521xueweihan/GitHub520)、[maxiaof/github-hosts](https://github.com/maxiaof/github-hosts)。只采纳 GitHub 范围内的公网地址，成功数据最多保留 24 小时；来源表不直接写入系统 hosts，连接仍校验 GitHub 原域名证书。

网页按连接延迟选路；Release、objects、codeload 和 Git 使用独立连接池与线路缓存。下载先尝试 DNS 首选地址，其他地址每个延后 150 ms 接替，避免微小握手差异决定下载出口。只采纳完整且不少于 4 MiB 的 200 响应学习吞吐，忽略短片段和 206；下一次请求避开已观察到的慢线；传输变慢时可改用新连接继续（见下文）。0.4 的旧线路缓存自动失效，下载每次启动重新学习；网页线路只短期落盘。

新增 `host_sources`、`adaptive_routing` 开关，默认开启。学习结果反映当前文件和下游读取速度下的实际端到端吞吐，不代表 IP 的固定带宽。历史仅保留在进程内六小时，不保存 Cookie、Token 或下载签名 URL；更换网络后可点“重新选路”清除。

旧的静态 GitHub hosts 表可能固定到过期线路。停止其他加速器后，可用管理员 PowerShell 执行：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\hosts.ps1 -Action clean
```

完整备份与 SHA256 记录保存在 `state/hosts-backups`，其余 hosts 条目保留。恢复使用同一脚本的 `-Action restore -Backup 完整的备份.json路径`；若清理后又有编辑则拒绝覆盖。此清理与启停时临时接入、恢复 hosts 分开。

## 慢连接救援与缓存

在测试网络上，下载速度取决于单条连接而不是 IP。新增 `flow_rescue` 开关，默认开启：符合条件的下载（至少 4 MiB、强 ETag、无内容压缩、客户端未指定 `Range`）先按普通单个 GET 进行，正常时不增加任何请求。上游低于 512 KiB/s 或连接出错时，从当前偏移改用四条新连接继续，并替换其中的慢连接。输出保持字节顺序，每个 Range 响应都必须与原 ETag 和确切区间一致。Git pack 下载无法使用此功能。

DoH 超时已缩短（连接 800 ms，单次查询 1.5 秒），出错的解析服务跳过 60 秒，解析结果保留更久并在后台刷新。详情、测量结果和局限见 `docs/flow-rescue.md`。

## 连接预热

新增 `prewarm` 开关，默认开启：引擎启动时连接 `github.com`、`api.github.com`、`github.githubassets.com`、`avatars.githubusercontent.com` 和 `raw.githubusercontent.com`，这些连接上不发送任何请求。其中最近一小时用过的站点会保持一条可用连接，不再使用后停止。“重新选路”会取消以上全部操作。本机没有路由的地址（例如没有 IPv6 的网络上的 IPv6 地址）不再查询和探测。设计理由与测量结果见 `docs/dev-sidecar-review.md`。

## Git 克隆竞速（实验）

新增 `git_race` 开关，默认关闭：请求对象的 Git fetch 会同时通过两条新连接发往两个不同的 GitHub 地址，先收到 1 MiB（或完整回复）的那条被保留，另一条关闭。git 始终只收到一份完整回复。代价是 GitHub 处理两次同一请求，最多重复下载 1 MiB。十组中型仓库对比结果互有胜负，小仓库克隆反而慢约 0.35 秒，因此默认关闭。详情见 `docs/git-race.md`。

## 免配置执行单条 Git 命令

不启用系统接管时，也可以让单条 git 命令经过 GitHubTurbo（思路来自 fgit）：

```powershell
.\github-turbo.exe git clone https://github.com/OWNER/REPO.git
.\github-turbo.exe git push origin main
```

无需管理员权限，不修改任何设置：不改 `hosts`、NRPT、证书或 git 配置。程序在随机本地回环端口启动一个私有引擎，并以单次命令参数 `-c http.https://github.com/.proxy=...` 运行 git，git 启动的子进程会继承该参数。引擎只负责选择 GitHub 地址并原样转发加密数据，git 仍校验 GitHub 自己的证书，push 和私有库照常可用。若系统接管已在运行，git 将原样执行。返回 git 的退出码；设置 `GITHUB_TURBO_DEBUG=1` 可查看引擎日志。仅作用于 `https://github.com/` 远程（不含 SSH 和 Git LFS 对象域名），反向代理类功能（HTTP/2 复用、救援、竞速、缓存）不适用。对 curl/curl 浅克隆的四组对比中，速度差异不可靠。详情及与 fgit 的对比见 `docs/fgit-review.md`。

## 可选配置

编辑 exe 旁的 `config.json`，重新启用生效。例如已有本地代理时：

```json
"upstream": "127.0.0.1:10808"
```

全局 `mode` 可选 `auto`、`direct`、`proxy`，`proxy` 必须配置上游。`host_modes` 为精确主机名覆盖，例如 `{"raw.githubusercontent.com":"proxy"}`。默认没有强制代理域名。

`ip_overrides` 可给指定域名提供临时候选公网 IP；不要长期保留过期 CDN 地址。默认先用实时 DoH，内置清单与三个 hosts 来源提供延迟启动的兜底候选；每次上游 TLS 连接仍验证原始主机名。

`optimization` 中的 `http2`、`public_cache`、`seed_routes`、`probe_reuse`、`range_memo`、`adaptive_split`、`host_sources`、`adaptive_routing`、`flow_rescue`、`prewarm` 默认开启；`segmented_downloads`、`range_pipeline`、`git_race` 为实验选项，默认关闭。分段子选项只在手动开启分段后生效。可在面板即时修改并保存；切换时已开始的请求继续完成。清空线路会同时清除缓存并重新选路。

只需要显式代理时，可在独立配置中设置 `transparent.enabled: false` 后运行 `github-turbo.exe serve <配置路径>`；此时不修改系统、不终止客户端 TLS，也不具备改写客户端 SNI 的能力。

## Release 与克隆

启用后可直接用浏览器下载 Release，或执行普通 `git clone https://github.com/OWNER/REPO.git`。自动接管改善网络路径，不改变仓库历史与文件语义。

下载脚本支持 curl 续传和可选 aria2 四路分段：

```powershell
.\scripts\download.ps1 -Url '实际的 Release HTTPS 地址' -Output '.\package.zip'
.\scripts\download.ps1 -Url '实际的 Release HTTPS 地址' -Output '.\package.zip' -Aria2 'C:\Tools\aria2\aria2c.exe'
```

内置分段针对至少 4 MiB、具有强 ETag、无内容压缩的 Release/objects 文件及图片视频；并发最多两个分段下载，每段 256 KiB。保留原始下载连接，同时比较单流和四路分段的首批 1 MiB；单流先完成、分段验证失败或三秒内未完成时，继续原来的单流下载。对同一线路、对象版本暂存十分钟的不利探测结果，避免重复试探；正文开始后发生错误会中断，下载器可重试续传。客户端已指定 Range 时按原请求转发。ETag 一致性检查不能代替发布者 SHA256。

Git 传输保持原始仓库语义。只需要当前代码时可以自行使用 `git clone --depth=1`；大型仓库可按需求采用 `--filter=blob:none --sparse`，这些选项不强制开启。

## 构建与验证

环境：Windows x64、Rust/MSVC。发行构建静态链接 MSVC CRT。运行无需 Python、Go、Node、BoringSSL 或另装 .NET。PowerShell 用于启停时的 Windows 设置变更；正常运行只有引擎和带托盘的原生守护进程，无 WebView 常驻进程。aria2 路径示例需替换为本机实际位置。

```powershell
cargo test --locked --target x86_64-pc-windows-msvc
cargo clippy --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings
.\scripts\build.ps1
python .\scripts\integration.py
```

制作候选版（RC）：运行 `.\scripts\package-rc.ps1`（加 `-NetworkSmoke` 可同时测试经 GitHub 的 `git ls-remote`）。要求工作区无未提交改动，且 `Cargo.toml` 中的版本尚无正式包。脚本依次运行 rustfmt、clippy 和测试，生成 `packages\rc\GitHubTurbo-<版本>-rc.<n>-windows-x64.zip`、`.sha256` 文件及记录提交和逐文件哈希的清单，并运行一次包内程序确认能启动。RC 编号自动递增，已有包不会被覆盖。

默认命令是 `enable`；其他命令为 `disable`、`recover`、`serve`、`diagnose`、`pac`、`git <参数>`。`init-ca` 和 `watch` 是内部管理命令。exe 旁配置优先；手动 serve 可指定配置文件。

## 当前边界

这是 Windows 首个系统接入版本，尚未完成跨机器兼容、长期运行和安装器验收。它覆盖遵循 Windows 解析规则的 GitHub 域名及常见显式 hosts 映射；自带 DoH、固定 IP、远程代理、自带证书库/证书固定、另一个用户或服务账户的程序，可能绕过接管或需要独立配置信任。Firefox 自定义证书库、非 OpenSSH 的 SSH 客户端需要单独验证。

对 GitHub Pages 自定义域名不自动接管；不代理与 GitHub 无关的网站。端口 80/443/53 被其他本地服务占用时，启用会失败并恢复已做改动。SSH 使用 ProxyCommand，不占用本机 SSH 服务端口。

IP/TLS 优选不等于持续吞吐最优；实际请求回退不能保证所有线路故障都可恢复。只有符合 `flow_rescue` 条件的下载可以在传输中切换连接；其他情况（包括 Git）下载中断后的续传由下载器完成。当前没有 Release 文件磁盘缓存和后台下载队列。官方 API 回退仅适用于可匿名查询的公开 Release，也受 API 限额约束。

即使独立直连优化全部失败，纯本地软件也不能创造新的远端出口；此时可配置可选上游。Web 面板显示运行状态，不等价于所有目标都已验证可达。

开发资料留在源码目录：历史选型 `docs/research.md`，本次参考吸收与验证 `docs/gitport-integration.md`；旧版报告保留。便携包只包含程序、配置、使用说明和依赖许可，不包含开发机诊断资料或 state 目录。内置候选数据的 GitPort Apache-2.0 许可与来源说明位于 `third-party/gitport`。

## 真实测速平台

双击源码目录或独立测速平台包内的 `bench/Start.cmd` 打开本机测速实验室（默认端口 18190）。它使用独立端口与私有 CA 对照冻结旧版、当前构建、独立 DNS 直连和可用的 GitPort，支持真实网页、Raw、Release、Git 克隆与内容校验，输出实时统计和离线报告。测试平台需要 Python 3.8+、Git、curl；日常加速器不增加运行时依赖。

测量方法、命令和流量预算见 `bench/README.md`；本轮逐项实验与取舍见 `docs/validation-0.4.md`。默认不开启没有稳定公网收益的分段流水线。

0.4.1 修正 Release 回归：固定同一组 IP 的四轮测试中，启用分段比关闭时更慢且出现一次超时，因此默认停止分段试探。TLS 复用与 IP 优选继续启用，详见 `docs/release-regression-0.4.1.md`。
