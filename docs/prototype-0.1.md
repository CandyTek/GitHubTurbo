# GitHubTurbo

Windows 上的轻量 GitHub 访问加速原型。默认监听 `127.0.0.1:18080`，复用已有 HTTP 代理 `127.0.0.1:10808`。项目采用 Rust + Tokio；Windows TLS 探测使用系统 SChannel。

它是 GitHub 专用的 **HTTP CONNECT 正向代理**。浏览器和 Git 继续访问原始域名、自行验证 GitHub 证书。完整 GitHub 网页涉及登录、Cookie、CSP、跳转和多个资源域名，直接映射成 localhost 反代不是本轮的主方案。

## 直接使用

在 PowerShell 中执行：

```powershell
& 'I:\RcProject\GitHubTurbo\dist\github-turbo.exe'
```

配置默认读取 exe 旁的 `config.json`；没有该文件才读取当前工作目录。若已有实例监听 18080，不必重复启动。Ctrl+C 停止前台实例。程序不会修改系统代理、hosts、根证书、Git 全局配置或开机启动项。

后台启动/停止可使用 `scripts/start.ps1` / `scripts/stop.ps1`。停止会立即关闭当前隧道。

状态地址：<http://127.0.0.1:18080/status>。

浏览器代理扩展/系统自动配置脚本可填：<http://127.0.0.1:18080/proxy.pac>。PAC 仅为所列 GitHub 域名使用此代理，其他域名直连；需保留其他代理规则时，将这些域名规则合并进现有客户端。PAC 没有自动安装。若已有 Xray TUN 或透明代理，所谓“直连”可能仍受它影响。

单次 Git 操作：

```powershell
git -c http.https://github.com/.proxy=http://127.0.0.1:18080 ls-remote https://github.com/octocat/Hello-World.git HEAD
git -c http.https://github.com/.proxy=http://127.0.0.1:18080 clone --depth=1 https://github.com/octocat/Hello-World.git
```

也可使用 `scripts/git-turbo.ps1 clone <URL>`。大型仓库按实际需求使用 `--filter=blob:none`、`--sparse` 或 `--depth=1`，这些选项会改变本地历史/文件可用性，不强制开启。

Release / Raw 下载：

```powershell
& 'I:\RcProject\GitHubTurbo\scripts\download.ps1' -Url 'https://raw.githubusercontent.com/octocat/Hello-World/master/README' -Output '.\README.download'
# 可选：使用本机 aria2 的 4 路分段和续传
& 'I:\RcProject\GitHubTurbo\scripts\download.ps1' -Url '<实际 Release HTTPS 链接>' -Output '.\package.zip' -Aria2 'D:\DengYuhua\Aria2\aria2c.exe'
```

下载脚本保持证书验证；大文件应额外对照发布者的 SHA256。分段只有在服务端支持 Range 且链路存在吞吐瓶颈时可能改善速度。aria2 是可选外部工具，主程序不依赖它。

## 已实现的策略

| 层面 | 行为 |
| --- | --- |
| 域名分流 | GitHub、API、Raw、头像、静态资源、codeload、Release CDN 及列出的历史 S3 域名；拒绝其他目的地及非 443 端口 |
| 自动选路 | 系统 DNS 的最多 8 个公网候选 IP 与 HTTP 上游并行探测，以验证证书的 TLS + HTTP HEAD 响应为依据 |
| 本机默认策略 | `github.com` 固定代理，其他允许域名 `auto`；基于本轮网页正文慢、API/codeload 直连快的观测 |
| DNS 故障 | 代理路线把域名交给上游解析；直连可配置精确域名的 `ip_overrides`，仍按原域名做 SNI 和证书校验 |
| 重连与回退 | `auto` 在返回 CONNECT 200 前，建连失败可尝试另一条通过探测的路线 |
| 资源上限 | 2 个 Tokio 工作线程、默认 64 个连接、4 组并行冷探测、128 个缓存条目；每隧道两个 16 KiB 复制缓冲区 |
| 流式传输 | 不缓存正文，支持双向流和半关闭；空闲计时会随传输刷新，不限制持续下载总时长 |
| 可观测性 | 本地 JSON 状态、字节统计、选路日志、诊断命令、可复跑的对比脚本 |

`host_modes` 是精确主机名匹配，不继承给子域名。模式可为 `auto`、`proxy`、`direct`。全局 `mode` 只作用于没有覆盖项的域名；需要全局固定某模式时，同时清空 `host_modes`。改配置后重启生效。

## 诊断与验证

```powershell
& 'I:\RcProject\GitHubTurbo\dist\github-turbo.exe' diagnose
python 'I:\RcProject\GitHubTurbo\scripts\benchmark.py'
& 'I:\RcProject\GitHubTurbo\scripts\dns-diagnose.ps1'
```

`diagnose` 主动对 6 个域名分别测直连和代理；CDN 根路径的 4xx 只代表 HTTPS 可达，不能证明某个文件存在。`benchmark.py` 真正 GET 网页/API/Raw/源码压缩包/官方 Release 文件，记录耗时、下载字节、退出码和内容哈希。它使用系统 curl，每项有超时与 4 MiB 体积上限，顺序执行，不是受控带宽测试。默认需要本地服务已运行。

DNS 脚本对照系统解析与经现有代理访问的 Cloudflare DoH，不改 DNS/hosts，也不把一次快照永久写成固定 IP。需要测试 `ip_overrides` 时，从诊断结果选择候选地址并及时更新，不长期硬编码过期 CDN IP。

## 构建

已验证环境：Rust/Cargo 1.96.0，MSVC x64 工具链，Windows。运行不需要 Rust、Python、Node 或 Go。编译依赖锁定于 `Cargo.lock`；四项直接依赖为 Tokio、tokio-native-tls、serde、serde_json。Windows 下无需附带 OpenSSL；其他平台构建可能需要系统 OpenSSL。

```powershell
cd I:\RcProject\GitHubTurbo
# 若下载 crates 需要现有代理，仅对当前终端设置
$env:HTTPS_PROXY = 'http://127.0.0.1:10808'
& .\scripts\build.ps1
cargo clippy --all-targets --locked -- -D warnings
python .\scripts\integration.py
```

构建脚本保留已有 `dist/config.json`，首次构建才复制默认配置。

## 原型边界

- HEAD 时延不等于大文件吞吐；自动模式可能选中握手快、正文慢的线路。可通过 `host_modes` 固定该域名，再复测完整下载。主程序没有宣称能提高现有代理本身的出口带宽。
- 选路探测是独立连接，实际隧道会重新建连；缓存过期后的首次请求有额外开销。已建立的 TLS 流无法无缝切换线路，也不会自动重放 Git push 或登录请求。
- 缓存到期后在下一次访问时重测；当前没有后台持续测速、吞吐学习、自动 DoH 缓存刷新或多上游节点池。
- 上游仅支持无认证的本机 HTTP CONNECT；不直接提供 SOCKS、TUN、UDP/QUIC、SSH、GUI 或 Windows 服务安装。浏览器通过此代理使用 TCP HTTPS；HTTPS 隧道可承载客户端协商的 HTTP/2。
- 普通 HTTP 明文请求返回 405，请使用 HTTPS URL。Git LFS、包管理器和第三方存储如跳转到未列出的域名会被拒绝；本轮未完成这些业务的端到端验证。
- TLS 由客户端端到端处理，因此不提供 HTTPS 正文缓存。公开不可变资源的磁盘缓存应另建下载服务；带 Cookie/Authorization 的内容不应被公共缓存。
- 本机已有代理是跨区域链路来源；若直连与该代理同时不可用，纯本地转发不能自行创造可用出口。

完整选型、依赖与资料见 `docs/research.md`；实测结论见 `docs/validation.md`。
