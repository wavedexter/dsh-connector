# dsh-connector

**非官方**的 DeepSeek Harness (`dsh web`) 远程连接器:内置 SSH 隧道 + 单窗口 WebView + 系统托盘,让你从办公室/外网**一键、稳定、免手动复制 token** 地访问家里 Linux 服务器上跑的 dsh WebUI。

[English](README_EN.md) · [设计文档](docs/DESIGN.md) · [使用说明](docs/使用说明.md)

> ⚠️ **免责声明**:本项目为非官方第三方客户端,与 DeepSeek 及其关联公司无任何关联,未获其认可或赞助。DeepSeek Harness 本身以 MIT 许可证独立分发。
> Not affiliated with, endorsed by, or sponsored by DeepSeek.

---

## ⚠️ 网络前提:不满足这些,装了也用不了

本 App **不自带代理**,用的就是你系统的普通网络。办公室/外网的电脑要连到家里的服务器,必须满足(或满足其等价条件):

| # | 前提 | 说明 |
|---|---|---|
| 1 | **服务器有公网 IPv6** | 家宽普遍有,地址长这样:`240e:…` / `2001:…`;dsh 所在机器要拿得到这个地址 |
| 2 | **运行 App 的电脑也有 IPv6** | 办公室、手机 4G/5G 一般都有;没有的话,开 TUN 全局模式的代理也可能通 |
| 3 | **路由器/光猫放行入站 22 端口** | IPv6 没有 NAT,但光猫/路由器防火墙会拦;**部分运营商家宽会随机封锁入站端口,22 也可能中招** |
| 4 | **(强烈建议)DDNS 域名** | 家宽 IPv6 通常是动态 /128,PPPoE 重播就变;配一个域名 + AAAA 记录,App 里填域名最省心 |

**30 秒自检**(在**要用 App 的电脑**上跑):

```powershell
ping -6 <服务器地址>            # 通 → 路由没问题
ssh -v <用户名>@<服务器地址>     # 出现 "Connection established" → 22 放行,可以用
ping 不通 / ssh 卡住 → 先用第 3、4 条排查,别急着装 App
```

**满足不了 IPv6?等价组合同样支持**(App 零网络层假设,凡是终端里 `ssh` 能通的网络它就能用):

- 公网 IPv4 + 路由器端口映射(把 22 映射出去)
- Tailscale / ZeroTier 等 overlay 网络(虚拟内网,推荐怕折腾的人)

---

## 它解决什么问题

手搓隧道访问 dsh 的标准流程,有三个绕不开的痛点:

| # | 痛点 | 根因 |
|---|---|---|
| 1 | 关掉终端,连接就断 | SSH 隧道生命周期绑在终端进程上 |
| 2 | dsh 每次重启 token 就失效 | launchToken 是进程级随机值,重启即变,旧的立即 401 |
| 3 | 要手动去服务器翻 token | token 只在 dsh 启动时打印一次到日志,随后被刷屏埋掉 |

本 App 的答案:隧道变成后台守护(断线指数退避重连)、日常凭据用 dsh 签发的 30 天 cookie(跨重启有效)、token 只在 cookie 失效时经 SSH 自动获取——**用户全程无感**。

## 功能特性

- 🔒 **SSH 隧道守护**:1s→2s→4s…60s 指数退避自动重连,ssh 子进程无黑窗、退出不残留
- 🔑 **密钥管理**:内置"生成密钥对 + 修复权限 + 部署命令"三件套,不用学 ssh-keygen
- 🍪 **凭据自愈**:cookie 落盘 + 三重看门狗(缺 cookie 自动取 / 跨站着陆自动同站重载 / 导航丢失自动补),**你永远不需要手动点"重新获取凭据"**
- 🪟 **单窗口**:配置页就是 WebUI 页,连接成功后同一窗口直接变成 WebUI;断开自动回配置页
- 📥 **系统托盘**(类微信/QQ):关窗=最小化到托盘,隧道继续跑;左键显隐,右键菜单;图标随状态变灰/绿
- ⬇️ **下载管理器**:接管 WebUI 内下载——百分比进度(无 Content-Length 时借 SSH `stat` 取真实大小)、实时速度、可取消
- 📋 **可观测**:实时日志窗 + 一键复制 + 日志落盘;一键 verbose SSH 诊断
- 🌗 **深浅色主题**:三个窗口一键切换、联动、持久化

## 依赖关系(重要,别漏看)

| 范围 | 依赖 |
|---|---|
| App 核心功能 | **零 dsh 插件依赖**;只需要系统 OpenSSH 客户端 + WebView2 |
| WebUI 内下载 | 需要 dsh 插件 **[dsh-better-sidebar](https://www.npmjs.com/package/dsh-better-sidebar)**(VSCode 风格侧边栏),原版 dsh 侧边栏没有下载入口。安装:`npx @deepseek-ai/dsh plugin --profile web add dsh-better-sidebar`,重启 dsh web |

## 工作原理(60 秒版)

dsh 的认证是三层:**launchToken**(进程级随机值,重启即变)→ **浏览器 cookie**(30 天,签名密钥持久化,跨重启有效)→ **WebSocket 凭据**。cookie 绑 authority(`127.0.0.1:端口`),所以本地端口必须固定;token 只在启动时打印一次到日志。

App 的策略:**cookie 优先,token 兜底**——日常只用 cookie,401 时才经 SSH `grep` 当前 token 换新的。完整分析、踩坑记录(WebView2 的 SameSite 跨站着陆、Tauri 2 ACL、Windows 细节)见 **[docs/DESIGN.md](docs/DESIGN.md)**。

## 界面

![配置与主界面](docs/screenshots/main.png)
![下载管理器](docs/screenshots/download.png)

## 使用

详见 [docs/使用说明.md](docs/使用说明.md)。最短路径:

1. App 配置页下方"密钥管理" → ① 生成密钥对 → 把给出的命令在终端跑一次(会问一次服务器密码);
2. 填主机地址 / 用户名,保存配置 → 连接;
3. 窗口自动变成 WebUI。之后关窗即托盘,日常只用托盘右键菜单。

## 构建

```bash
cd src-tauri
cargo tauri dev        # 开发运行
cargo tauri build      # 打包(Windows/macOS/Linux)
```

无 Windows 机器时在 Linux 上交叉编译出 `.exe`(cargo-xwin + clang-cl,零 root 安装)的完整步骤见 [docs/DESIGN.md §构建](docs/DESIGN.md#构建)。

## FAQ

**Q: 启动时 Windows 弹"SmartScreen 阻止了无法识别的应用"?**
未购买代码签名证书的独立 exe 都会这样(与安全性无关,是"没有签名+没有下载声誉")。点"更多信息 → 仍要运行",仅第一次提示。要彻底消除需 OV 代码签名证书(约几百元/年)。

**Q: 本地端口为什么固定 18080、不能改?**
dsh 的 cookie 绑 authority(地址),端口随机会导致 cookie 每次失效。固定即可,30 天内无感。

**Q: dsh 重启后要重新认证吗?**
不用。cookie 跨 dsh 重启有效(签名密钥持久化),App 全自动处理。

**Q: 支持多台服务器吗?**
当前单主机;配置结构已按可扩展设计,多主机在路线图里。

**Q: 为什么关窗口不退出?**
设计如此(类微信/QQ):隧道是后台服务,关窗只是隐藏。彻底退出请用托盘右键"退出"。

## 路线图

- [x] 隧道守护 / 凭据自愈 / 单窗口 / 托盘 / 下载管理 / 主题 / 密钥管理
- [ ] v1.1+ 托盘完成通知;多主机配置;更完善的打包(安装器/自动更新)
- [ ] Android 端(Tauri 2 同一套代码)

## 许可证

[MIT](LICENSE) © 2026 Dexter

## 致谢

- [DeepSeek Harness](https://www.npmjs.com/package/@deepseek-ai/dsh)(MIT)——被连接的一方
- [Tauri](https://tauri.app/)(Apache-2.0/MIT)——应用框架
