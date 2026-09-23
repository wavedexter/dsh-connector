use crate::config::Config;
use crate::probe;
use crate::state::AppState;
use anyhow::{anyhow, Result};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tauri::Manager;

fn ssh_base_args(cfg: &Config, args: &mut Vec<String>) {
    args.push("-i".into());
    args.push(cfg.key_path.clone());
    args.push("-o".into());
    args.push("BatchMode=yes".into());
    args.push("-o".into());
    args.push("StrictHostKeyChecking=accept-new".into());
    args.push("-o".into());
    args.push("ConnectTimeout=10".into());
    args.push("-o".into());
    args.push("ServerAliveInterval=15".into());
    args.push("-o".into());
    args.push("ServerAliveCountMax=3".into());
}

/// Windows: 杀掉子进程的控制台窗口(否则每次 ssh 都会弹黑色 cmd 窗)
fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
}

/// 建隧道: ssh -N -L <本地端口>:127.0.0.1:<远程端口> user@host
pub fn ssh_tunnel_command(cfg: &Config) -> Command {
    let mut args: Vec<String> = Vec::new();
    ssh_base_args(cfg, &mut args);
    args.push("-o".into());
    args.push("ExitOnForwardFailure=yes".into());
    args.push("-N".into());
    args.push("-L".into());
    args.push(format!("{}:127.0.0.1:{}", cfg.local_port, cfg.remote_port));
    args.push("-p".into());
    args.push(cfg.ssh_port.to_string());
    args.push(format!("{}@{}", cfg.user, cfg.host));
    let mut cmd = Command::new("ssh");
    cmd.args(&args);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    hide_console(&mut cmd);
    cmd
}

/// 构造一条 "ssh 执行远程命令" 的命令(取 token / 密钥测试共用)。
pub fn ssh_exec_command(cfg: &Config, remote_cmd: &str, verbose: bool) -> Command {
    let mut args: Vec<String> = Vec::new();
    ssh_base_args(cfg, &mut args);
    if verbose {
        args.push("-v".into());
    }
    args.push("-p".into());
    args.push(cfg.ssh_port.to_string());
    args.push(format!("{}@{}", cfg.user, cfg.host));
    args.push(remote_cmd.to_string());
    let mut cmd = Command::new("ssh");
    cmd.args(&args);
    hide_console(&mut cmd);
    cmd
}

/// 跑一条命令并等待,带**硬超时**;超时杀进程并报错。
/// 教训:ssh 在对方网络异常时可能无限期卡住,曾把认证循环整个堵死、
/// 导致窗口永远停在配置页。任何 ssh 调用都必须有截止时间。
pub(crate) fn run_with_deadline(
    mut cmd: Command,
    deadline: Duration,
) -> Result<std::process::Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let start = std::time::Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map_err(anyhow::Error::from);
        }
        if start.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "ssh 超过 {} 秒未返回,已强制终止(网络异常或 ssh 卡死)",
                deadline.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// SSH 上去 grep 日志,拿当前有效的 token(日志里最后一条)。
///
/// 注意:远程命令**刻意不用引号、正则、管道**——只用最朴素的
/// `grep token= <文件>`。教训:带单引号/反斜杠的复杂命令在
/// Windows ssh.exe 的参数传递中会被损坏,bash 收到引号不闭合的
/// 半截命令会永远等更多输入,ssh 会话挂死(家里实测复现,
/// echo 简单命令却正常)。历史多行由 Rust 侧取最后一条。
pub fn fetch_token(state: &Arc<AppState>, cfg: &Config) -> Result<String> {
    // 铁律:日志路径空了就明明白白报错,绝不能让 grep 退化成读 stdin 死等
    // (曾因该栏被空值覆盖,ssh 会话全部挂死 20 秒被杀)
    if cfg.dsh_log_path.trim().is_empty() {
        anyhow::bail!(
            "服务器 dsh 日志路径未配置——请在配置页填写(如 /home/fn/dsh/dsh.log)"
        );
    }
    let remote_cmd = format!("grep token= {} < /dev/null", cfg.dsh_log_path);
    state.log(format!("auth: ssh 远程命令: {remote_cmd}"));
    let out = run_with_deadline(
        ssh_exec_command(cfg, &remote_cmd, false),
        Duration::from_secs(20),
    )?;
    if !out.status.success() {
        return Err(anyhow!(
            "ssh 执行失败(退出码 {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .map(str::trim)
        .rev()
        .find(|l| l.contains("token="))
        .ok_or_else(|| anyhow!("在 {} 中没有找到 token", cfg.dsh_log_path))?;
    let token = line
        .split("token=")
        .nth(1)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("无法解析 token: {}", line))?;
    Ok(token.to_string())
}

/// 一键诊断: 用当前配置跑一次 verbose ssh,完整输出进日志。
pub fn run_key_test(state: &Arc<AppState>) {
    let state = state.clone();
    std::thread::spawn(move || {
        let cfg = state.cfg.lock().unwrap().clone();
        if cfg.host.is_empty() || cfg.key_path.is_empty() || cfg.user.is_empty() {
            state.log("=== 诊断中止: 主机地址/用户名/私钥路径为空 ===");
            return;
        }
        state.log("=== 密钥测试开始(最长约 30 秒) ===");
        let cmd = ssh_exec_command(&cfg, "echo 密钥连接OK", true);
        match run_with_deadline(cmd, Duration::from_secs(30)) {
            Ok(out) => {
                let se = String::from_utf8_lossy(&out.stderr);
                for line in se.lines() {
                    let l = line.trim();
                    if !l.is_empty() {
                        state.log(format!("ssh: {l}"));
                    }
                }
                let so = String::from_utf8_lossy(&out.stdout);
                for line in so.lines() {
                    let l = line.trim();
                    if !l.is_empty() {
                        state.log(l);
                    }
                }
                state.log(format!("=== 密钥测试结束(退出码 {}) ===", out.status));
            }
            Err(e) => state.log(format!("=== 密钥测试失败: {e} ===")),
        }
    });
}

/// 隧道守护: ssh 挂了就以指数退避重连(1s→2s→4s→…→60s,成功后重置)。
pub fn start_supervisor(state: Arc<AppState>) {
    std::thread::spawn(move || {
        let mut backoff = 1u64;
        loop {
            if state.stop.load(Ordering::SeqCst) {
                break;
            }
            let cfg = state.cfg.lock().unwrap().clone();
            if cfg.host.is_empty() || cfg.key_path.is_empty() || cfg.user.is_empty() {
                state.log("tunnel: 配置不完整(主机地址/用户名/私钥路径),等待配置…");
                for _ in 0..25 {
                    if state.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                continue;
            }
            state.log(format!(
                "tunnel: 连接 {}@{}:{} …",
                cfg.user, cfg.host, cfg.ssh_port
            ));
            match ssh_tunnel_command(&cfg).spawn() {
                Ok(mut child) => {
                    if let Ok(mut pid) = state.tunnel_pid.lock() {
                        *pid = Some(child.id());
                    }
                    // 把 ssh 的 stderr 实时读进日志面板(排障关键)
                    if let Some(stderr) = child.stderr.take() {
                        let state2 = state.clone();
                        std::thread::spawn(move || {
                            use std::io::BufRead;
                            let reader = std::io::BufReader::new(stderr);
                            for line in reader.lines().map_while(Result::ok) {
                                let line = line.trim();
                                if !line.is_empty() {
                                    state2.log(format!("ssh: {line}"));
                                }
                            }
                        });
                    }
                    let started = std::time::Instant::now();
                    let status = child.wait();
                    let secs = started.elapsed().as_secs();
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Ok(mut pid) = state.tunnel_pid.lock() {
                        *pid = None;
                    }
                    if state.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let code = status.map(|s| s.to_string()).unwrap_or_else(|_| "?".into());
                    state.log(format!("tunnel: 断开(退出码 {code},本次存活 {secs} 秒)"));
                    if secs > 10 {
                        // 掉线较久,WebUI 的 WebSocket 大概率已死,恢复后需要刷新页面
                        state.need_webreload.store(true, Ordering::SeqCst);
                    }
                    backoff = if secs >= 3 { 1 } else { (backoff * 2).min(60) };
                }
                Err(e) => {
                    state.log(format!("tunnel: 启动 ssh 失败: {e} (系统里有 ssh 吗?)"));
                    backoff = (backoff * 2).min(60);
                }
            }
            let mut slept = 0.0f64;
            while slept < backoff as f64 && !state.stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(200));
                slept += 0.2;
            }
        }
        state.log("tunnel: 已停止");
    });
}

/// 认证循环: 每 3 秒探测本地 WebUI; 401 就自动换新 token。
pub fn start_auth_loop(state: Arc<AppState>) {
    std::thread::spawn(move || loop {
        if state.stop.load(Ordering::SeqCst) {
            break;
        }
        let cfg = state.cfg.lock().unwrap().clone();
        let cookie = state.cookie.lock().unwrap().clone();
        match probe::probe_status(cfg.local_port, cookie.as_deref()) {
            Ok(200) => {
                let mut st = state.status.lock().unwrap();
                st.tunnel_up = true;
                st.webui_ok = true;
                st.last_error = None;
                drop(st);
                if state.need_webreload.swap(false, Ordering::SeqCst) {
                    state.log("auth: 隧道已恢复,刷新页面");
                    reload_webview(&state);
                }
                // 启动后窗口还没到 WebUI:负责把窗口带过去(cookie 还有效就不用取 token)
                if !state.webui_shown.load(Ordering::SeqCst) {
                    state.log("auth: 隧道就绪,打开 WebUI …");
                    if let Err(e) = ensure_webui_visible(&state, &cfg) {
                        state.log(format!("auth: 打开 WebUI 失败: {e}"));
                    }
                }
            }
            Ok(401) => {
                state.status.lock().unwrap().webui_ok = false;
                state.log("auth: 凭据失效(401),自动获取新 token …");
                match refresh(&state, &cfg) {
                    Ok(()) => {
                        state.status.lock().unwrap().webui_ok = true;
                        state.log("auth: 新凭据已就绪");
                    }
                    Err(e) => {
                        state.log(format!("auth: 失败: {e}"));
                        state.status.lock().unwrap().last_error = Some(e.to_string());
                    }
                }
            }
            Ok(code) => {
                state.log(format!("probe: 异常 HTTP {code}"));
                state.status.lock().unwrap().last_error = Some(format!("HTTP {code}"));
            }
            Err(e) => {
                state.log(format!("probe: 本地服务不可达({e})"));
                let mut st = state.status.lock().unwrap();
                st.tunnel_up = false;
                st.last_error = Some(e.to_string());
            }
        }
        if state.force_reauth.swap(false, Ordering::SeqCst) {
            state.log("auth: 手动触发,重新获取凭据 …");
            let _ = refresh(&state, &cfg);
        }
        std::thread::sleep(Duration::from_secs(3));
    });
}

/// 取新 token → 本地换 cookie → 把主窗口导航到 WebUI(单窗口,不再弹新窗)。
fn refresh(state: &Arc<AppState>, cfg: &Config) -> Result<()> {
    state.log("auth: 正在通过 SSH 获取当前 token …");
    let started = std::time::Instant::now();
    let token = fetch_token(state, cfg)?;
    let last4: String = token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    state.log(format!(
        "auth: token 已获取({}…{}),SSH 耗时 {:.1}s,正在换 cookie …",
        probe::truncate(&token, 4),
        last4,
        started.elapsed().as_secs_f32()
    ));
    let resp = probe::http_get(
        "127.0.0.1",
        cfg.local_port,
        &format!("/?token={}", token),
        None,
    )?;
    if resp.status != 303 {
        state.log(format!(
            "auth: 换 cookie 期望 303,实际 HTTP {} body=\"{}\"",
            resp.status,
            probe::truncate(&resp.body, 120)
        ));
        return Err(anyhow!("token 换 cookie 期望 303,实际 {}", resp.status));
    }
    match probe::cookie_from_headers(&resp.headers) {
        Some(pair) => {
            state.log(format!("auth: 收到 Set-Cookie {}", probe::truncate(&pair, 46)));
            *state.cookie.lock().unwrap() = Some(pair.clone());
            save_cookie(state, &pair);
        }
        None => state.log("auth: 警告:303 响应里没有 Set-Cookie"),
    }
    let target = format!("http://127.0.0.1:{}/?token={}", cfg.local_port, token);
    state.log(format!("auth: 正在把主窗口导航到 {}", probe::truncate(&target, 60)));
    navigate_webview(state, &target);
    Ok(())
}

/// 把探测用的 cookie 落盘(下次启动免去一次 SSH 取 token)。
fn save_cookie(state: &Arc<AppState>, pair: &str) {
    if let Some(handle) = state.handle.get() {
        if let Ok(dir) = handle.path().app_config_dir() {
            let _ = std::fs::write(dir.join("cookie.txt"), pair);
        }
    }
}

/// WebView 的 cookie 存储里有没有 dsh 的登录 cookie。
fn webview_has_cookie(state: &Arc<AppState>, cfg: &Config) -> bool {
    if let Some(handle) = state.handle.get() {
        if let Some(w) = handle.get_webview_window("main") {
            if let Ok(base) = format!("http://127.0.0.1:{}/", cfg.local_port).parse::<tauri::Url>() {
                if let Ok(cookies) = w.cookies_for_url(base) {
                    return cookies.iter().any(|c| c.name().starts_with("dsh-auth"));
                }
            }
        }
    }
    false
}

/// 确保主窗口正显示着已登录的 WebUI。
/// cookie(探测用)仍有效 → 直接导航过去;否则走一次 token 流程。
pub fn ensure_webui_visible(state: &Arc<AppState>, cfg: &Config) -> Result<()> {
    let cookie = state.cookie.lock().unwrap().clone();
    let jar_ok = matches!(probe::probe_status(cfg.local_port, cookie.as_deref()), Ok(200));
    if jar_ok && webview_has_cookie(state, cfg) {
        state.log("auth: cookie 仍有效,直接打开 WebUI");
        navigate_webview(state, &webui_base(cfg));
        Ok(())
    } else {
        refresh(state, cfg)
    }
}

fn webui_base(cfg: &Config) -> String {
    format!("http://127.0.0.1:{}", cfg.local_port)
}

/// 在主窗口里执行一段 JS。
fn eval_main(state: &Arc<AppState>, js: &str) {
    if let Some(handle) = state.handle.get() {
        if let Some(w) = handle.get_webview_window("main") {
            let _ = w.eval(js);
        }
    }
}

/// 把主窗口导航到 WebUI(此函数跑在工作线程,安全)。
fn navigate_webview(state: &Arc<AppState>, url: &str) {
    let js = format!(
        "window.location.replace({});",
        serde_json::to_string(url).unwrap_or_default()
    );
    eval_main(state, &js);
    state.webui_shown.store(true, Ordering::SeqCst);
}

fn reload_webview(state: &Arc<AppState>) {
    eval_main(state, "window.location.reload();");
}

/// 守护线程: 监视主窗口 URL(诊断 + 配置页地址自愈)、断开后返回配置页、
/// 并持续把状态推送到系统托盘(tooltip + 图标颜色)。
pub fn start_guardian(state: Arc<AppState>) {
    std::thread::spawn(move || {
        let mut last_url = String::new();
        let mut tick = 0u32;
        let mut last_tip = String::new();
        let mut last_ok = false;
        let mut stuck_ticks = 0u32;
        let mut last_cookie_fix = std::time::Instant::now() - Duration::from_secs(60);
        let mut had_cookie = false;
        loop {
            if let Some(handle) = state.handle.get() {
                if let Some(w) = handle.get_webview_window("main") {
                    let cfg = state.cfg.lock().unwrap().clone();
                    let base = webui_base(&cfg);
                    tick += 1;
                    if let Ok(url) = w.url() {
                        let url_str = url.to_string();
                        let on_webui = url_str.starts_with(&base);
                        let on_webui_root = on_webui && !url_str.contains("token=");

                        // 自愈:看到"非 WebUI 且非空白"的地址就记下(断开后导航回它)
                        if !on_webui
                            && !url_str.is_empty()
                            && !url_str.starts_with("about:")
                            && !url_str.starts_with("data:")
                        {
                            let mut p = state.panel_url.lock().unwrap();
                            if p.as_deref() != Some(url_str.as_str()) {
                                state.log(format!(
                                    "ui: 配置页 URL 已更新 → {}",
                                    probe::truncate(&url_str, 80)
                                ));
                                *p = Some(url_str.clone());
                            }
                        }

                        if url_str != last_url {
                            state.log(format!(
                                "ui: 主窗口 URL → {}",
                                probe::truncate(&url_str, 90)
                            ));
                            // 每次进入 WebUI,诊断 WebView 里到底有没有 cookie
                            if on_webui {
                                if let Ok(base_url) =
                                    format!("http://127.0.0.1:{}/", cfg.local_port).parse::<tauri::Url>()
                                {
                                    match w.cookies_for_url(base_url) {
                                        Ok(cookies) => {
                                            let names: Vec<String> = cookies
                                                .iter()
                                                .map(|c| probe::truncate(c.name(), 20))
                                                .collect();
                                            state.log(format!(
                                                "ui: WebView 中 {} 的 cookie: [{}]",
                                                base,
                                                names.join(", ")
                                            ));
                                        }
                                        Err(e) => {
                                            state.log(format!("ui: 读取 WebView cookie 失败: {e}"))
                                        }
                                    }
                                }
                            }
                            // 新的页面加载:cookie 是否已就位要重新判断
                            if on_webui_root {
                                // 从配置页(tauri.localhost)第一次落到 WebUI 是跨站导航,
                                // SameSite=Strict 的 cookie 不会随行 → 需要一次同站重载
                                // (下面看门狗 2 负责执行;这里只重置判断状态)
                                had_cookie = false;
                            }
                            last_url = url_str;
                        }

                        // 看门狗 1:窗口该在 WebUI 却不在(导航 eval 丢了之类)→ 重新导航
                        if state.webui_shown.load(Ordering::SeqCst)
                            && !on_webui
                            && state.running.load(Ordering::SeqCst)
                        {
                            stuck_ticks += 1;
                            if stuck_ticks >= 15 {
                                state.log("ui: 窗口未到达 WebUI,重新导航 …");
                                let _ = ensure_webui_visible(&state, &cfg);
                                stuck_ticks = 0;
                            }
                        } else {
                            stuck_ticks = 0;
                        }

                        // 看门狗 2:WebUI 页面的登录 cookie 治理
                        // - cookie 已就位但页面没带上(第一次落地是跨站导航,
                        //   SameSite=Strict 不随行)→ 同站重载一次(代替你手动 F5);
                        // - 压根没有 cookie → 自动重新获取(代替你手动点按钮)。
                        if tick % 3 == 0 && on_webui_root && state.running.load(Ordering::SeqCst) {
                            let has_cookie = webview_has_cookie(&state, &cfg);
                            if has_cookie && !had_cookie {
                                had_cookie = true;
                                state.log("ui: 页面未带上登录 cookie(跨站着陆),同站重载一次 …");
                                let _ = w.eval("window.location.reload();");
                            } else if !has_cookie {
                                had_cookie = false;
                                if last_cookie_fix.elapsed() > Duration::from_secs(5) {
                                    state.log("ui: WebUI 页面缺少登录 cookie,自动重新获取 …");
                                    state.force_reauth.store(true, Ordering::SeqCst);
                                    last_cookie_fix = std::time::Instant::now();
                                }
                            }
                        }
                    }

                    // 已断开连接 → 从 WebUI 返回配置页
                    if state.webui_shown.load(Ordering::SeqCst) && !state.running.load(Ordering::SeqCst) {
                        if let Some(panel) = state.panel_url.lock().unwrap().clone() {
                            if panel.is_empty() || panel.starts_with("about:") {
                                state.log(format!("ui: 配置页地址无效({panel}),不导航(避免白屏)"));
                            } else {
                                state.log(format!(
                                    "ui: 返回配置页 → {}",
                                    probe::truncate(&panel, 80)
                                ));
                                let js = format!(
                                    "window.location.replace({});",
                                    serde_json::to_string(&panel).unwrap_or_default()
                                );
                                let _ = w.eval(js);
                            }
                            state.webui_shown.store(false, Ordering::SeqCst);
                        }
                    }

                    // 托盘状态(约每 1.5 秒,且仅在变化时更新)
                    if tick % 5 == 0 {
                        let (tunnel_up, webui_ok, tip) = {
                            let st = state.status.lock().unwrap();
                            let mut tip = String::from("DSH Connector · ");
                            tip.push_str(if st.tunnel_up { "隧道已连接" } else { "隧道未连接" });
                            if st.webui_ok {
                                tip.push_str(" · WebUI 就绪");
                            }
                            if let Some(e) = &st.last_error {
                                tip.push_str(" · ");
                                tip.push_str(&probe::truncate(e, 30));
                            }
                            (st.tunnel_up, st.webui_ok, tip)
                        };
                        let ok = tunnel_up && webui_ok;
                        if tip != last_tip || ok != last_ok {
                            last_tip = tip.clone();
                            last_ok = ok;
                            if let Some(tray) = state.tray.get() {
                                let _ = tray.set_tooltip(Some(&tip));
                                let bytes: &'static [u8] = if ok {
                                    include_bytes!("../icons/tray-green.png")
                                } else {
                                    include_bytes!("../icons/tray-gray.png")
                                };
                                let img = tauri::image::Image::new_owned(bytes.to_vec(), 32, 32);
                                let _ = tray.set_icon(Some(img));
                            }
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    });
}

pub fn kill_process(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status();
    }
}
