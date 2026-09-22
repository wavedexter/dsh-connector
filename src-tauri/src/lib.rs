mod commands;
mod config;
mod download;
mod probe;
mod state;
mod tunnel;

use std::sync::Arc;
use state::AppState;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, WebviewUrl, WindowEvent};

/// 全局 state 句柄(给下载事件等回调用)
static APP_STATE: std::sync::OnceLock<Arc<AppState>> = std::sync::OnceLock::new();

/// 左键点托盘 / 菜单"显示/隐藏"→ 切换主窗口可见性。
fn toggle_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
}

/// 打开(或聚焦)一个辅助小窗口:日志窗 / 配置窗 / 下载窗。
/// 注意:所有窗口都在 setup 里预创建(主线程),这里只负责显示/聚焦——
/// 在 WebView 事件回调里现场创建窗口会得到空白窗。
fn open_window_by_label(state: &Arc<AppState>, label: &str) {
    if let Some(handle) = state.handle.get() {
        if let Some(w) = handle.get_webview_window(label) {
            let _ = w.show();
            let _ = w.set_focus();
        } else {
            state.log(format!("ui: 窗口 {label} 不存在(预创建失败?)"));
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> tauri::Result<()> {
    tauri::Builder::default()
        .manage(Arc::new(AppState::default()))
        .setup(|app| {
            let state = app.state::<Arc<AppState>>().inner().clone();
            let _ = state.handle.set(app.handle().clone());
            let _ = APP_STATE.set(state.clone());

            // ---- 主窗口(代码创建,以便挂下载事件处理)----
            // 先走配置页(前端面板);连接成功后本窗口直接变成 WebUI。
            let main_window = tauri::WebviewWindowBuilder::new(
                app.handle(),
                "main",
                WebviewUrl::App("index.html".into()),
            )
            .title("DSH Connector")
            .inner_size(1280.0, 860.0)
            .resizable(true)
            .on_download(|_webview, event| {
                let log = |msg: String| {
                    if let Some(s) = APP_STATE.get() {
                        s.log(msg);
                    }
                };
                let state = APP_STATE.get().cloned();
                match event {
                    tauri::webview::DownloadEvent::Requested { url, destination } => {
                        if url.scheme() == "http" || url.scheme() == "https" {
                            // App 自己下载:进度、速度、取消、下载窗,全都有
                            if let Some(s) = &state {
                                let cookie = s.cookie.lock().unwrap().clone();
                                match download::start_download(s, url.as_str(), cookie) {
                                    Ok(id) => {
                                        log(format!("download: #{id} 已接管 {url}"));
                                        open_window_by_label(s, "download");
                                        *destination =
                                            std::env::temp_dir().join("dsh-connector-handles-it");
                                        false
                                    }
                                    Err(e) => {
                                        log(format!("download: 接管失败({e}),回退内置下载"));
                                        true
                                    }
                                }
                            } else {
                                true
                            }
                        } else {
                            // blob:/data: 等,数据只在浏览器里,只能交给 WebView 自己写盘
                            if let Some(s) = &state {
                                let name = destination
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .filter(|n| !n.is_empty())
                                    .unwrap_or_else(|| "download".into());
                                let id = s
                                    .next_download_id
                                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                s.push_download(crate::download::DownloadInfo {
                                    id,
                                    url: url.to_string(),
                                    file_name: name,
                                    dest: destination.display().to_string(),
                                    done: 0,
                                    total: None,
                                    speed: 0.0,
                                    status: "传输中".into(),
                                    error: None,
                                    cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                                });
                                log(format!("download: #{id} 委托浏览器处理 {url}"));
                                open_window_by_label(s, "download");
                            }
                            true
                        }
                    }
                    tauri::webview::DownloadEvent::Finished { url, path, success } => {
                        if url.scheme() != "http" && url.scheme() != "https" {
                            if let Some(s) = &state {
                                let p = path
                                    .as_ref()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_else(|| "?".into());
                                s.finish_by_url(
                                    url.as_str(),
                                    success,
                                    if success { None } else { Some(format!("保存失败: {p}")) },
                                );
                                log(format!(
                                    "download: {} {url} → {p}",
                                    if success { "完成" } else { "失败" }
                                ));
                            }
                        }
                        true
                    }
                    _ => true,
                }
            })
            .build()?;
            if let Ok(url) = main_window.url() {
                let u = url.to_string();
                if !u.is_empty() && u != "about:blank" {
                    state.log(format!("ui: 初始主窗口 URL = {u}"));
                    *state.panel_url.lock().unwrap() = Some(u);
                }
            }

            // ---- 预创建辅助窗口(主线程创建;运行时只显示/聚焦)----
            // 在 WebView 事件回调里现场建窗口会得到空白窗,所以一律在 setup 里建好。
            let aux_windows: [(&str, &str, f64, f64); 3] = [
                ("log", "DSH Connector · 运行日志", 720.0, 480.0),
                ("config", "DSH Connector · 连接配置", 480.0, 640.0),
                ("download", "DSH Connector · 下载", 600.0, 440.0),
            ];
            for (label, title, w, h) in aux_windows {
                let page = format!("{label}.html");
                match tauri::WebviewWindowBuilder::new(
                    app.handle(),
                    label,
                    WebviewUrl::App(page.as_str().into()),
                )
                .title(title)
                .inner_size(w, h)
                .resizable(true)
                .visible(false)
                .build()
                {
                    Ok(_) => state.log(format!("ui: 窗口 {label} 已预创建(隐藏)")),
                    Err(e) => state.log(format!("ui: 窗口 {label} 预创建失败: {e}")),
                }
            }

            // 加载已保存的配置
            if let Ok(dir) = app.path().app_config_dir() {
                if let Ok(json) = std::fs::read_to_string(dir.join("config.json")) {
                    if let Ok(cfg) = serde_json::from_str::<config::Config>(&json) {
                        state.log(format!(
                            "config: 已加载 {}",
                            dir.join("config.json").display()
                        ));
                        *state.cfg.lock().unwrap() = cfg;
                    }
                }
            }

            // 打开落盘日志 + 预载上次运行的日志尾部(白屏/重启后也能回看)
            let mut log_path = None;
            if let Ok(dir) = app.path().app_config_dir() {
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join("dsh-connector.log");
                if std::fs::metadata(&path)
                    .map(|m| m.len() > 256 * 1024)
                    .unwrap_or(false)
                {
                    let _ = std::fs::write(&path, "");
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let lines: Vec<&str> = content.lines().collect();
                    let start = lines.len().saturating_sub(60);
                    if !lines.is_empty() {
                        state.log_memory_only("──── 上次运行的日志(尾部)────");
                        for l in &lines[start..] {
                            state.log_memory_only(l);
                        }
                    }
                }
                if let Ok(file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    *state.log_file.lock().unwrap() = Some(file);
                    log_path = Some(path);
                }
            }
            if let Some(p) = log_path {
                state.log(format!("log: 本次日志同时写入 {}", p.display()));
            }

            // 恢复上次的 cookie(探测用;仍然有效就免去一次 SSH 取 token)
            if let Ok(dir) = app.path().app_config_dir() {
                if let Ok(s) = std::fs::read_to_string(dir.join("cookie.txt")) {
                    let s = s.trim().to_string();
                    if s.contains('=') {
                        *state.cookie.lock().unwrap() = Some(s);
                        state.log("cookie: 已从磁盘恢复上次的 cookie");
                    }
                }
            }

            // ---- 系统托盘(右键菜单;左键 = 显示/隐藏主窗口)----
            let handle = app.handle();
            let show = MenuItem::with_id(handle, "show", "显示/隐藏主窗口", true, None::<&str>)?;
            let s1 = PredefinedMenuItem::separator(handle)?;
            let reauth = MenuItem::with_id(handle, "reauth", "重新获取凭据", true, None::<&str>)?;
            let disconnect = MenuItem::with_id(handle, "disconnect", "断开连接", true, None::<&str>)?;
            let s2 = PredefinedMenuItem::separator(handle)?;
            let test = MenuItem::with_id(handle, "test", "测试连接(诊断)", true, None::<&str>)?;
            let log = MenuItem::with_id(handle, "log", "运行日志", true, None::<&str>)?;
            let config = MenuItem::with_id(handle, "config", "连接配置", true, None::<&str>)?;
            let s3 = PredefinedMenuItem::separator(handle)?;
            let quit = MenuItem::with_id(handle, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(
                handle,
                &[
                    &show, &s1, &reauth, &disconnect, &s2, &test, &log, &config, &s3, &quit,
                ],
            )?;
            let tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("DSH Connector")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    let state = app.try_state::<Arc<AppState>>().map(|s| s.inner().clone());
                    match event.id().as_ref() {
                        "show" => toggle_main_window(app),
                        "reauth" => {
                            if let Some(s) = &state {
                                s.force_reauth.store(true, std::sync::atomic::Ordering::SeqCst);
                                s.log("ui: 托盘触发:重新获取凭据");
                            }
                        }
                        "disconnect" => {
                            if let Some(s) = &state {
                                s.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                                s.running.store(false, std::sync::atomic::Ordering::SeqCst);
                                let pid = s.tunnel_pid.lock().unwrap().take();
                                if let Some(pid) = pid {
                                    tunnel::kill_process(pid);
                                }
                                s.status.lock().unwrap().tunnel_up = false;
                                s.log("ui: 托盘触发:断开连接");
                            }
                        }
                        "test" => {
                            if let Some(s) = &state {
                                tunnel::run_key_test(s);
                            }
                        }
                        "log" => {
                            if let Some(s) = &state {
                                open_window_by_label(s, "log");
                            }
                        }
                        "config" => {
                            if let Some(s) = &state {
                                open_window_by_label(s, "config");
                            }
                        }
                        "quit" => {
                            if let Some(s) = &state {
                                s.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                                let pid = s.tunnel_pid.lock().unwrap().take();
                                if let Some(pid) = pid {
                                    tunnel::kill_process(pid);
                                }
                            }
                            app.exit(0);
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        ..
                    } = event
                    {
                        toggle_main_window(tray.app_handle());
                    }
                })
                .build(handle)?;
            let _ = state.tray.set(tray);
            state.log("ui: 系统托盘已就绪(左键=显示/隐藏窗口,右键=菜单)");

            // 已配置过则自动连接
            let has_cfg = {
                let c = state.cfg.lock().unwrap();
                !c.host.is_empty() && !c.key_path.is_empty()
            };
            if has_cfg {
                state.log("startup: 检测到已保存配置,自动连接");
                state.stop.store(false, std::sync::atomic::Ordering::SeqCst);
                state.running.store(true, std::sync::atomic::Ordering::SeqCst);
                let s = state.clone();
                tunnel::start_supervisor(s.clone());
                tunnel::start_auth_loop(s.clone());
                tunnel::start_guardian(s);
            } else {
                tunnel::start_guardian(state.clone());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // 所有窗口关闭 = 隐藏(不销毁):主窗口隐入托盘,辅助窗留着下次直接显示
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::save_config,
            commands::connect,
            commands::disconnect,
            commands::reauth,
            commands::test_key,
            commands::get_status,
            commands::get_logs,
            commands::get_downloads,
            commands::cancel_download,
            commands::open_downloads_dir,
            commands::get_theme,
            commands::set_theme,
            commands::default_key_path,
            commands::fix_key_permissions,
            commands::gen_keypair,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // 退出时清理隧道 ssh,避免孤儿进程占用本地端口
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app.try_state::<Arc<AppState>>() {
                    state.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    let pid = state.tunnel_pid.lock().unwrap().take();
                    if let Some(pid) = pid {
                        tunnel::kill_process(pid);
                        state.log("exit: 隧道已清理");
                    }
                }
            }
        });
    Ok(())
}
