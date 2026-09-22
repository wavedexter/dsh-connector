# dsh-connector 设计文档

> 本文档记录完整的设计思路、关键决策与踩坑。阅读它可以理解"为什么是现在这个样子"。
> 所有认证机制的结论都经过实测验证(读 dsh 源码 + curl/真机双重确认)。

## 1. 目标与痛点

**目标**:一个桌面端(后续扩展到 Android)连接器 App,内置 SSH 隧道 + 单窗口 WebView,让用户在办公室/外网**一键、稳定、免手动复制 token** 地访问家里 Linux 服务器上运行的 DeepSeek Harness(`dsh web`)WebUI。

三个具体痛点:

| # | 痛点 | 根因 |
|---|---|---|
| 1 | 关掉终端,连接就断 | SSH 隧道生命周期绑定在终端进程上 |
| 2 | 每次 dsh 重启后 token 失效 | launchToken 是进程级随机值,重启即变 |
| 3 | 要手动去服务器翻 token | token 只在 dsh 启动时打印一次到日志 |

## 2. 核心洞察:dsh 的认证模型

整个方案建立在这个发现上(读 dsh 源码 `dsh-client-connection` 包 + curl 实测):

```
第一层:launchToken(进程级,易变)
  - 进程启动时 randomBytes() 生成,base64url,43 字符
  - 重启即变,旧的立即失效;只在启动时打印一次到 stdout/日志
  - 唯一作用:首次换取 cookie 的"引导凭证"

第二层:browser cookie(持久,30 天)   ← 用户日常真正依赖的
  - 访问 /?token=<launchToken> 且校验通过 → 303 + Set-Cookie
  - cookie 名:dsh-auth-<base64url(sha256(authority))>
  - 值:v1.<payload>.<HMAC-SHA256 签名>;Max-Age=2592000;HttpOnly;SameSite=Strict
  - 签名密钥持久化在 dsh 的 credentials store(~/.dsh),跨重启有效
  - payload 内含 authority(即请求的 Host:port)——所以 cookie 绑"地址"

第三层:WebSocket 凭据
  - /api/remote.mux 的 upgrade 同样校验 cookie
```

**推论(决定了 App 的策略):cookie 才是日常凭据,token 只是引导。**
App 的正确姿势:优先用 cookie,只在 cookie 失效(401)时才去取新 token。
这样 dsh 重启 10 次,用户只需要在首次配置时提供一次 token。

### 硬约束(实现时不要违反)

1. **本地端口必须固定**(默认 18080)。cookie 绑 authority(`127.0.0.1:18080`),端口每次随机 = cookie 每次作废;
2. **探测用 `GET /`,不要用 token 判断**。token 变了用户无从得知;cookie 失效才是真信号;
3. **`/api/*` 路由不存在**,别去探测;真实 API 走 WebSocket mux;
4. **不要把 token 打进 WebView 的日常 URL**。token 只用于换取 cookie 的那一次请求。

## 2.5 网络前提(部署前必读)

App 不自带代理,用的是系统的普通网络栈——**凡是终端里 `ssh` 能通的网络,App 就能用**。典型部署(家宽服务器 + 办公室客户端)需要:

1. 服务器有公网 IPv6,且 dsh 所在机器拿得到;
2. 客户端(办公室/手机)有 IPv6,或有 TUN 全局模式的代理;
3. 服务器侧光猫/路由器防火墙不放行入站 22(注意:部分运营商家宽会随机封锁入站端口,22 也可能中招,需实测);
4. 家宽 IPv6 常变 → 推荐 DDNS(域名 + AAAA 记录),App 主机地址填域名。

其他等价组合同样支持:IPv4 + 路由器端口映射、Tailscale/ZeroTier overlay。设计上 App 只负责"建隧道 + 管凭据 + 展示",传输完全委托系统 ssh,因此对网络层零假设。

## 3. 架构

```
┌──────────────────────────────────────────────┐
│ dsh-connector(Tauri 2,单窗口 + 托盘)           │
│                                              │
│  主窗口:配置页(tauri://) ⇄ WebUI(http://127.0.0.1:18080) │
│  辅助窗:日志 / 配置 / 下载(启动时预创建,隐藏)     │
│  托盘:左键显隐,右键菜单,图标/提示随状态变色       │
│                                              │
│  线程模型(Rust 侧):                            │
│  ┌──────────────┐ ┌──────────────┐ ┌────────┐ │
│  │ 隧道守护      │ │ 认证循环      │ │ guardian│ │
│  │ ssh -N -L    │ │ 每 3s 探测    │ │ URL/   │ │
│  │ 指数退避重连  │ │ 401→取token   │ │ cookie │ │
│  │ stderr→日志  │ │ 换cookie→导航 │ │ 看门狗 │ │
│  └──────────────┘ └──────────────┘ └────────┘ │
└──────────────────────────────────────────────┘
         │ SSH(22)                │ 本地 HTTP(18080→3080)
         ▼                        ▼
    家里服务器(dsh web,loopback-only)
```

**token 获取**:SSH exec `grep -oE 'http://127\.0\.0\.1:3080/\?token=[^ ]+' <dsh日志> | tail -1`。
日志是追加写的,token 行早被其他输出埋掉,所以必须 grep;取"最后一条"即当前进程的有效 token。

**为什么单窗口**:配置页就是初始页,连接成功后同一窗口 `location.replace` 到 WebUI。不需要弹新窗,断开时再导航回配置页。窗口标题栏、任务栏都只有一份,最干净。

## 4. 关键技术决策与踩坑

### 4.1 窗口必须在启动时预创建

**坑**:在 WebView 事件回调(如 `on_download`)里现场 `WebviewWindowBuilder::build()`,Windows 上会得到一个"建了但没初始化"的**白屏窗口**。
**决策**:四个窗口全部在 `setup()`(主线程)创建,辅助窗 `visible(false)`;运行时的任何路径只做 show/focus。辅助窗的 CloseRequested 一律 prevent_close + hide,永不销毁。

### 4.2 SameSite=Strict 跨站着陆(最隐蔽的一个坑)

**现象**:App 自动导航到 WebUI 后页面 401;但此时手动点"重新获取凭据"就好使;F5 也能好使。
**根因**:窗口从配置页(`tauri.localhost`)第一次跳到 WebUI(`127.0.0.1:18080`)是**跨站导航**,`SameSite=Strict` 的 cookie **不会随跨站导航及其 303 重定向发出**——页面"裸载"。而手动再点一次时窗口已经站在 WebUI 里,同站导航,cookie 随行。F5 是同站重载,同理。
**修法**:guardian 检测"页面已落地但没带上 cookie"→ **自动做一次同站重载**(把用户和 F5 的活都干掉了)。
**通用教训**:Web 技术栈里操作 cookie 认证的第三方站点时,务必区分"同站/跨站"两条路径,自动流要在两条路径上都验证。

### 4.3 凭据自愈(三层看门狗)

目标是"用户在凭据问题上永远零操作":

1. **cookie 落盘**(`cookie.txt`):重启后探测直接用上次的 cookie,有效则免去一次 SSH;
2. **缺 cookie → 自动重新获取**(guardian 每 ~3s 用 `cookies_for_url` 检查 WebView 罐子里有没有 `dsh-auth-*`,没有就触发取 token 流程,5 秒冷却);
3. **导航丢失 → 自动补**(`webui_shown` 与窗口实际 URL 不符超过 ~5s,重新导航)。

配套:探测循环用 Rust 侧自带 cookie 的极简 HTTP 客户端(不依赖第三方 crate),401 即触发刷新。

### 4.4 Tauri 2 的三个安全机制(每个都咬过)

| 机制 | 坑 | 解法 |
|---|---|---|
| **ACL 权限** | 自定义命令默认不被允许,远程页面 invoke 报 `Command xxx not allowed by ACL` | 在 `src-tauri/permissions/*.toml` 里声明 `allow-connector` 权限,capability 引用它 |
| **Capability 窗口白名单** | `"windows": ["main"]` 会把日志窗/配置窗/下载窗的 IPC 全挡掉 | 白名单列全四个窗口 label |
| **远程 URL IPC** | WebUI 是 http:// 远程页面,invoke 被静默拒绝 | capability 里加 `"remote": {"urls": ["http://127.0.0.1:18080/**"]}` |

**教训**:Tauri v2 的应用命令不是"默认放行"——加了 capability 文件反而成了唯一准入清单,要么不加,要么加全。

### 4.5 Windows 细节

- **ssh 黑窗**:GUI 程序拉起的 ssh.exe 会配一个控制台窗口。`cmd.creation_flags(0x0800_0000)`(CREATE_NO_WINDOW)解决,tunnel 和 exec 两条路径都要加;
- **WebView2 下载**:不接管时下载被静默丢弃。用 `on_download` 接管:http(s) 请求由 App 自己下载(见下),blob:/data: 只能交回 WebView;
- **私钥权限**:OpenSSH 拒绝 ACL 过宽的私钥(`UNPROTECTED PRIVATE KEY FILE`)。`icacls /inheritance:r` + `/grant:r %USERNAME%:R`;
- **孤儿进程**:App 退出不杀 ssh 子进程会残留并占用本地端口。退出钩子(RunEvent::Exit)里 kill,并 `taskkill /F /PID`。

### 4.6 自带下载管理器

不引 reqwest,用极简流式 HTTP(约 300 行):

- 重定向跟随(≤5)、`Content-Length` 总大小、`Transfer-Encoding: chunked` 解码、`Content-Disposition` 真名解析(`filename*=` UTF-8 优先);
- 16KB 分块写 `.part`,250ms 采样算速度,`AtomicBool` 取消(取消即删临时文件);
- 完成任务:`fs::rename` 到最终路径;重名自动加序号;
- 进度通过 `Vec<DownloadInfo>` + 前端 250ms 轮询展示(不依赖事件系统, bundless 前端也能用)。

**实测发现**:better-sidebar 的下载端点是 `Transfer-Encoding: chunked` 且**不带 Content-Length**(HEAD 405、不认 Range),所以总大小拿不到。解法很取巧:**URL 的 `path=` 查询参数就是服务器上的文件路径**,而 App 本来就有 SSH 通道——下载前先 `stat -c%s` 一把,百分比就有了(探测失败就退回“大小未知”的呼吸动画,不影响下载)。这顺手证明了“带着 SSH 通道写客户端”的额外红利。

### 4.7 密钥管理(小白的救命稻草)

调查发现小白用户 90% 卡在"公钥/私钥"而不在 App 本身。所以配置页内置了密钥管理三件套:

1. **生成密钥对**:shell 出到系统 `ssh-keygen`(OpenSSH 客户端自带,零新增依赖),ed25519、无密码短语;已存在文件时**拒绝执行**——避免 ssh-keygen 的交互式覆盖确认把线程挂住;
2. **修复私钥权限**:Windows 走 `icacls /inheritance:r` + `/grant:r %USERNAME%:R`(注意 PowerShell 里 `/grant:r` 会被参数解析坑,所以从 Rust 直接 spawn,不经 shell),Unix 走 `chmod 600`;
3. **部署命令生成**:把公钥 + 表单里的 user@host 拼成一条 `ssh ... "mkdir -p ~/.ssh && echo '<pubkey>' >> ~/.ssh/authorized_keys && chmod 600 ..."` 命令,用户粘到终端跑一次即可(会问一次服务器密码)。

没有做"全自动一键部署"(那需要 App 自己实现密码认证的 SSH,即引入 russh 级别的依赖)——用一条粘贴命令换取零重量依赖,是小而美路线上的合理取舍。

### 4.8 托盘状态推送不依赖页面 IPC

状态文字/颜色如果走页面 JS 轮询 invoke,远程页面的 ACL/remote.urls 一改就瞎。改为 **Rust 方向 eval 注入**:guardian 每秒 `eval` 一段 JS 直接改 DOM——Rust→页面方向永远可用,与 IPC 无关。(v1.0.4 起控制条虽已移除,此模式仍用于必要时。)

## 5. 构建

### 正常构建(Windows/macOS)

```bash
cd src-tauri
cargo tauri dev
cargo tauri build
```

### 无 Windows 机器:Linux 交叉编译

```bash
# 1. Rust + Windows 目标
rustup target add x86_64-pc-windows-msvc

# 2. cargo-xwin(MSVC 交叉编译)
cargo install cargo-xwin

# 3. clang-cl 与 llvm-rc(资源编译用;无需 root,解 deb 到用户目录)
apt-get download clang-18 libclang-cpp18 libllvm18 lld-18 llvm-18
for f in ./*.deb; do dpkg -x "$f" ~/.local/llvm; done
export PATH="$HOME/.local/llvm/usr/lib/llvm-18/bin:$PATH"
export LD_LIBRARY_PATH="$HOME/.local/llvm/usr/lib/llvm-18/lib"
export RC_x86_64_pc_windows_msvc=llvm-rc RC=llvm-rc

# 4. 国内镜像加速(可选)
#    ~/.cargo/config.toml:replace-with = 'rsproxy-sparse'(sparse+https://rsproxy.cn/index/)

# 5. 构建(首次会自动下载 Windows SDK/CRT)
cargo xwin build --release --target x86_64-pc-windows-msvc
# 产物:target/x86_64-pc-windows-msvc/release/dsh-connector.exe
```

踩过的坑:`llvm-rc` 在独立的 `llvm-18` deb 里;tauri-winres 找它靠 `RC` 环境变量;PowerShell 里 `icacls /grant:r` 会被参数解析坑,用 `cmd /c` 包一层。

## 6. 路线图

- [x] v1.0.0 隧道 + 凭据自管 + 单窗口 + 日志/诊断
- [x] v1.0.1~v1.0.3 ACL 修复、窗口预创建、下载管理器、SameSite 修法
- [x] v1.0.4 深浅色主题
- [ ] v1.1 托盘完成通知气泡;DDNS 域名直连说明
- [ ] v2.0 Android 端(`tauri android init`:cleartext 配置、Keystore 存密钥、Foreground Service 保活、网络切换重连)
- [ ] 远期:多主机支持、私钥密码短语托管

## 7. 已知边界

- 单主机配置(多主机是 schema 演进,非重构);
- Windows 优先(Linux/macOS 能构建,托盘/下载细节未充分打磨);
- WebView2 的 cookie 对 `SameSite=Strict` 的跨站限制是平台行为,App 只能绕过(同站重载)不能消除;
- 未做代码签名(见 README 的 SmartScreen 说明)。

## 8. 参考

- dsh 认证源码:`@deepseek-ai/dsh-client-connection`(MIT)
- Tauri v2 文档:tray / ACL(capabilities、permissions、remote urls)/ DownloadEvent
- WebView2:SameSite 行为、DownloadStarting、CREATE_NO_WINDOW
