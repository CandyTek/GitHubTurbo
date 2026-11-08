# GitHub 本地加速选型

调研日期：2026-11-08。目标环境是 Windows x64，已有 HTTP 出口 `127.0.0.1:10808`。当前目录原为空目录、无 Git 仓库或既有实现。

## 工具比较

以下资源占用判断是架构选型判断，不是不同软件的同机实测排行。没有测到的数据不填写内存数字。

| 方案 | 适合用途与依赖 | 本项目判断 |
| --- | --- | --- |
| [3proxy](https://github.com/3proxy/3proxy) | C 实现，Windows 二进制；HTTP CONNECT、SOCKS、SNI 转发、DNS 缓存及父代理链 | 现成极简核心首选候选；需要另写健康探测与域名策略控制器。尝试获取官方 1.0.3 x64 包时返回 504，未完成同机内存/吞吐实测 |
| [GOST v3](https://github.com/go-gost/gost) | Go 独立程序，代理链、转发、DNS、路由、多协议；[selector](https://gost.run/en/concepts/selector/) 支持备选与竞争策略 | 希望快速复用成熟功能时优先；功能面大于当前需求，不能未经测试声称其内存占用低于其他候选 |
| [Caddy](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy) | Go；反向代理支持连接池、上游 TLS、超时和网络代理配置 | 独立公开文件下载反代合适；完整 GitHub 页面涉及跨域与登录，不能仅改一条 upstream 就保证体验 |
| [Nginx Windows](https://nginx.org/en/docs/windows.html) | C；HTTP 反代成熟，但官方 Windows 文档标为 beta，使用 select/poll，提示扩展性限制 | 本机找到旧版本；不把 Linux 下的性能印象套用到 Windows，未启动已有安装 |
| [Pingora](https://github.com/cloudflare/pingora#system-requirements) | Rust 代理框架；官方主要面向 Linux，Windows 为社区初步支持 | 不适合作为本轮 Windows 原型的首选 |
| Rust + Tokio 小型核心 | Tokio 异步 I/O，serde 配置，系统 TLS 仅用于健康探测；无额外守护进程 | 本轮实现。功能范围较小，容易设定硬性并发/缓存上限；生产化维护成本由项目承担 |

上表中的“反代”与“代理链”用途不同。对浏览器完整 GitHub 和 Git HTTPS，采用原域名 CONNECT 隧道，保留登录与 TLS 语义。对公开、固定 URL 的下载缓存，可以后续单独加入 Caddy 下载网关。

## 本机环境与依赖

通过用户提供的 Everything CLI 搜索开发工具和相关二进制，没有遍历私人文档。精确文件名应使用 `wfn:`，例如：

```powershell
& 'D:\Download3\ES-1.1.0.30.x64\es.exe' /a-d -n 20 'wfn:aria2c.exe'
```

- Rust/Cargo 1.96.0：`C:\Users\Administrator\.cargo\bin`。
- Visual Studio 2022 Community：`E:\Microsoft Visual Studio\2022\Community`，MSVC 构建已成功。
- Git 2.45.1.windows.1；Windows curl 8.13.0（Schannel）；Python 可用于测试脚本。
- 用户提供的 10808 由 Xray 监听：`D:\DengYuhua\v2rayN-windows-64\bin\xray\xray.exe`。
- aria2：`D:\DengYuhua\Aria2\aria2c.exe`，另有 Aria2Manager 副本。
- Nginx：`E:\nginx 1.23.3.3\nginx.exe`，另有软件自带的 1.16.0 副本。
- 未找到 Go、3proxy、Caddy、GOST 可执行文件。不据此断言磁盘上绝无未索引版本。

主程序直接依赖：Tokio 1.x、tokio-native-tls 0.3、serde 1、serde_json 1。实际解析版本在 Cargo.lock 中固定；本轮 Tokio 1.54.1、native-tls 0.2.18、serde 1.0.229、serde_json 1.0.154。锁文件还包含其他平台的条件依赖，Windows 构建不需 OpenSSL。

[native-tls 官方说明](https://docs.rs/native-tls/latest/native_tls/)：Windows 后端使用 SChannel。[tokio-native-tls](https://docs.rs/tokio-native-tls/latest/tokio_native_tls/) 提供异步封装，默认验证主机名。运行目录只需 exe 与配置，主程序不依赖 Python/Node/Go 运行时。选定发行许可前应审阅直接与传递依赖的许可证；本轮未替用户指定项目发行许可证。

## 多层加速方案与进度

| 层面 | 本轮完成 | 后续范围 |
| --- | --- | --- |
| DNS | 代理端解析；系统 DNS 与 DoH 对照脚本；可选 IP 覆盖且保留 SNI/证书验证 | 内置 DoH、TTL 缓存、多解析器健康检查 |
| 线路 | 直连候选与现有 HTTP 代理竞争；TLS+HEAD 探测、按域名模式、TTL 缓存、建连失败回退 | 吞吐与失败率评分、迟滞控制、多节点池 |
| 网页 | PAC 精确分流；资源域名覆盖；保留原站登录与 TLS | 浏览器自动化加载瀑布、登录态和更多 GitHub 功能验证 |
| Git | 单次 Git 代理脚本；建议按用途减少 clone 数据量 | Git LFS、私有仓库、push、大型仓库长期测试 |
| 下载 | 重定向隧道、流式转发、curl 续传、可选 aria2 4 路分段 | 不可变公开文件的磁盘缓存、校验清单与限额 |
| SSH | 记录 GitHub 官方 `ssh.github.com:443` 备用入口 | 独立 SSH ProxyCommand 接入；当前代理不自动转发 SSH |

[Git clone 文档](https://git-scm.com/docs/git-clone)说明浅克隆、部分克隆与稀疏检出选项；必须按用户是否需要完整历史选择。[GitHub SSH 文档](https://docs.github.com/en/authentication/troubleshooting-ssh/using-ssh-over-the-https-port)说明 `ssh.github.com:443`，这不等于普通 HTTPS；当前健康探测不适用于它。

[aria2 文档](https://aria2.github.io/manual/en/html/aria2c.html)说明分段、并发、续传和校验选项；更多连接并不保证更快。[Cloudflare DoH 文档](https://developers.cloudflare.com/1.1.1.1/encryption/dns-over-https/make-api-requests/dns-json/)说明 JSON GET 查询与 TTL，脚本保留 TTL 供观察，不永久改写系统解析。

本轮不引入第三方公共 GitHub 镜像。已有私有代理承载加密隧道；无需把 GitHub Cookie、Token 或私有仓库内容交给不明镜像站。
