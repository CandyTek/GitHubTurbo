# GitHubTurbo 0.3

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
| 独立解析 | Cloudflare、Google、阿里 DoH 并发获取 IPv4/IPv6，阿里 DoH 额外采样不同 EDNS 子网；固定解析器启动地址，保留 TLS 校验，避开被接管的系统解析 |
| IP 优选 | 内置候选与 DoH 并行竞争，各最多 8 个公网候选；精确域名规则优先；验证证书后再评估真实响应；标准 SNI 优先，无 SNI 延后竞争 |
| 线路健康 | 按域名及线路分别记录延迟与失败冷却；421 拒绝、429 限流、5xx 和连接错误分别处理；限流不触发请求重放；候选线路落盘，过期后重新选择 |
| 本地 HTTPS | 每台机器独立 CA；客户端到本机、到真实 GitHub 两段 TLS；上游始终校验证书链与原始主机名 |
| HTTP 转发 | 前后端 HTTP/2，跨客户端复用同域名上游连接；不支持时使用 HTTP/1.1；流式请求/响应、Range、Cookie、认证头与独立 HTTP/1.1 WebSocket 升级；强制 Host 与 TLS 目标一致 |
| 连接与缓存上限 | 最多 96 条共享上游连接、64 个池；会话域名单池 1 条连接，内容域名最多 4 条；闲置 90 秒回收；公共静态缓存最多 16 MiB、128 项、单项 2 MiB，不缓存带 Cookie/认证/签名参数的请求或账号网页 |
| 失败回退 | 真实 GET/HEAD 收到 5xx 后，有限尝试其他 IP/TLS 方式；请求有正文或为 POST 时不自动重放 |
| Release | 标准下载入口 5xx 时可匿名尝试官方 Release API；符合条件的大文件自动 4 路直连 Range 下载，逐段检查强 ETag、Content-Range 与长度；不使用公共第三方镜像 |
| Git | 普通 HTTPS clone/pull/push 走本地反代；当前用户 OpenSSH 为 github.com/ssh.github.com 设置专用 ProxyCommand，连接官方 SSH 443 入口 |
| 可选上游 | 配置本机 HTTP CONNECT 出口后，与直连共同参与选路；上游故障不影响未指定 proxy 模式的直连尝试 |
| 恢复 | 持久化会话记录、原生进程守护、登录恢复任务；hosts 无并发变化时按字节恢复，有变化时撤回本程序标记并保留外部编辑 |

HTTP 明文的 GitHub URL 会重定向到 HTTPS。本地 DNS 对 GitHub 域名返回 IPv4 回环，真实上游仍可使用 IPv6。HTTP/2 开关控制上游协商，前端同时支持 HTTP/2 和 HTTP/1.1。

## 可选配置

编辑 exe 旁的 `config.json`，重新启用生效。例如已有本地代理时：

```json
"upstream": "127.0.0.1:10808"
```

全局 `mode` 可选 `auto`、`direct`、`proxy`，`proxy` 必须配置上游。`host_modes` 为精确主机名覆盖，例如 `{"raw.githubusercontent.com":"proxy"}`。默认没有强制代理域名。

`ip_overrides` 可给指定域名提供临时候选公网 IP；不要长期保留过期 CDN 地址。默认内置精简候选清单，并与实时 DoH 竞争；每次上游 TLS 连接仍验证原始主机名。

`optimization` 中的 `http2`、`public_cache`、`segmented_downloads`、`seed_routes` 默认均为 `true`。可在面板即时修改并保存；切换时已开始的请求继续完成。清空线路会同时清除缓存并重新选路。

只需要显式代理时，可在独立配置中设置 `transparent.enabled: false` 后运行 `github-turbo.exe serve <配置路径>`；此时不修改系统、不终止客户端 TLS，也不具备改写客户端 SNI 的能力。

## Release 与克隆

启用后可直接用浏览器下载 Release，或执行普通 `git clone https://github.com/OWNER/REPO.git`。自动接管改善网络路径，不改变仓库历史与文件语义。

下载脚本支持 curl 续传和可选 aria2 四路分段：

```powershell
.\scripts\download.ps1 -Url '实际的 Release HTTPS 地址' -Output '.\package.zip'
.\scripts\download.ps1 -Url '实际的 Release HTTPS 地址' -Output '.\package.zip' -Aria2 'C:\Tools\aria2\aria2c.exe'
```

内置分段针对至少 4 MiB、具有强 ETag、无内容压缩的 Release/objects 文件及图片视频；并发最多两个分段下载，每段 256 KiB。保留原始下载连接，首批 1 MiB 分段验证失败或三秒内未完成时，继续原来的单流下载；正文开始后发生错误会中断，下载器可重试续传。客户端已指定 Range 时按原请求转发。ETag 一致性检查不能代替发布者 SHA256。

Git 传输保持原始仓库语义。只需要当前代码时可以自行使用 `git clone --depth=1`；大型仓库可按需求采用 `--filter=blob:none --sparse`，这些选项不强制开启。

## 构建与验证

环境：Windows x64、Rust/MSVC。发行构建静态链接 MSVC CRT。运行无需 Python、Go、Node、BoringSSL 或另装 .NET。PowerShell 用于启停时的 Windows 设置变更；正常运行只有引擎和带托盘的原生守护进程，无 WebView 常驻进程。aria2 路径示例需替换为本机实际位置。

```powershell
cargo test --locked --target x86_64-pc-windows-msvc
cargo clippy --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings
.\scripts\build.ps1
python .\scripts\integration.py
```

默认命令是 `enable`；其他命令为 `disable`、`recover`、`serve`、`diagnose`、`pac`。`init-ca` 和 `watch` 是内部管理命令。exe 旁配置优先；手动 serve 可指定配置文件。

## 当前边界

这是 Windows 首个系统接入版本，尚未完成跨机器兼容、长期运行和安装器验收。它覆盖遵循 Windows 解析规则的 GitHub 域名及常见显式 hosts 映射；自带 DoH、固定 IP、远程代理、自带证书库/证书固定、另一个用户或服务账户的程序，可能绕过接管或需要独立配置信任。Firefox 自定义证书库、非 OpenSSH 的 SSH 客户端需要单独验证。

对 GitHub Pages 自定义域名不自动接管；不代理与 GitHub 无关的网站。端口 80/443/53 被其他本地服务占用时，启用会失败并恢复已做改动。SSH 使用 ProxyCommand，不占用本机 SSH 服务端口。

IP/TLS 优选不等于持续吞吐最优；实际请求回退不能保证所有线路故障都可恢复。已返回的响应正文不能无缝切换连接，下载中断后的续传由下载器完成。当前没有 Release 文件磁盘缓存、后台下载队列或自动吞吐学习。官方 API 回退仅适用于可匿名查询的公开 Release，也受 API 限额约束。

即使独立直连优化全部失败，纯本地软件也不能创造新的远端出口；此时可配置可选上游。Web 面板显示运行状态，不等价于所有目标都已验证可达。

开发资料留在源码目录：历史选型 `docs/research.md`，本次参考吸收与验证 `docs/gitport-integration.md`；旧版报告保留。便携包只包含程序、配置、使用说明和依赖许可，不包含开发机诊断资料或 state 目录。内置候选数据的 GitPort Apache-2.0 许可与来源说明位于 `third-party/gitport`。
