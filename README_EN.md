# dsh-connector

[English](#dsh-connector-1) · [中文](README.md) · [Design Doc](docs/DESIGN.md)

> ⚠️ **Disclaimer**: This is an **unofficial** third-party client for [DeepSeek Harness](https://www.npmjs.com/package/@deepseek-ai/dsh) (`dsh web`). It is not affiliated with, endorsed by, or sponsored by DeepSeek. DeepSeek Harness is independently distributed under the MIT License.

---

## ⚠️ Network prerequisites: read this first

The app ships **no proxy of its own** — it uses your system's network stack as-is. To reach a home server from the office or the public internet, **all** of the following (or their equivalents) must hold:

| # | Prerequisite | Notes |
|---|---|---|
| 1 | **Public IPv6 on the server** | Home broadband usually has it (`240e:…` / `2001:…`); the machine running dsh must actually hold that address |
| 2 | **IPv6 on the client machine** | Office networks and mobile 4G/5G usually have it; a TUN-mode proxy may also work |
| 3 | **Inbound port 22 not filtered** | IPv6 has no NAT, but the router/ONT firewall may still drop it; **some ISPs randomly block inbound ports on home lines — including 22** |
| 4 | **(Strongly recommended) a DDNS domain** | Home IPv6 is usually a dynamic /128 that changes on PPPoE redial; point an AAAA record at it |

**30-second self-check** (run on the machine that will run the app):

```powershell
ping -6 <server-address>            # replies → routing is fine
ssh -v <user>@<server-address>      # "Connection established" → port 22 is open, you're good
```

**No IPv6? Equivalent setups work too** (the app makes zero assumptions about the network layer — anything where terminal `ssh` works, works):

- Public IPv4 + router port forwarding (map 22)
- Tailscale / ZeroTier and similar overlay networks

## What it solves

The standard manual recipe (`ssh -L` + browser + copy the token from the server log) has three structural pain points: the tunnel dies with your terminal; dsh's launch token changes on every restart; and the token is printed to the log exactly once.

This app: the tunnel becomes a supervised background service with exponential-backoff reconnect; the daily credential is dsh's 30-day browser cookie (survives restarts); the token is fetched automatically over SSH only when the cookie is invalid — **the user never touches any of it**.

## Features

- SSH tunnel supervisor (1s→2s→4s…60s backoff), no console windows, no orphan processes
- Built-in key management: generate a keypair / fix permissions / one-line deploy command
- Self-healing credentials: on-disk cookie jar + three watchdogs (missing-cookie refetch, cross-site-landing same-site reload, lost-navigation recovery)
- Single-window UX: the config page *becomes* the WebUI
- WeChat/QQ-style tray: close = minimize to tray (tunnel keeps running), left-click toggle, right-click menu, status icon
- Download manager for in-WebUI downloads: percentage progress (falls back to an SSH `stat` size probe when the response has no Content-Length), live speed, cancelable
- Observable: live log window + one-click copy + on-disk log; one-click verbose SSH diagnostics
- Light/dark theme, persisted

## Dependencies

| Scope | Dependency |
|---|---|
| Core app | **No dsh plugin required.** Only the system OpenSSH client + WebView2 |
| In-WebUI downloads | Requires the dsh plugin **[dsh-better-sidebar](https://www.npmjs.com/package/dsh-better-sidebar)** (VSCode-like sidebar) — the stock dsh sidebar has no download entry. Install: `npx @deepseek-ai/dsh plugin --profile web add dsh-better-sidebar` |

## How it works (60 seconds)

dsh authentication is three-layered: a process-level **launchToken** (random per run), a **browser cookie** (30 days, signing key persisted — survives restarts), and **WebSocket credentials**. The cookie is bound to an authority (`127.0.0.1:port`), hence the fixed local port; the token is printed to the log exactly once at startup.

The app's strategy: **cookie first, token as fallback** — it only greps a fresh token over SSH when the cookie 401s. Full analysis and war stories (WebView2's SameSite cross-site landing, Tauri 2 ACL, Windows quirks): [docs/DESIGN.md](docs/DESIGN.md).

## Build

```bash
cd src-tauri
cargo tauri dev
cargo tauri build
```

Cross-compiling the Windows `.exe` from a Linux box (cargo-xwin + clang-cl, no root required): see [DESIGN.md → Build](docs/DESIGN.md#build).

## License

[MIT](LICENSE) © 2026 Dexter

## Acknowledgments

- [DeepSeek Harness](https://www.npmjs.com/package/@deepseek-ai/dsh) (MIT) — the system this client connects to
- [Tauri](https://tauri.app/) (Apache-2.0/MIT) — the app framework
