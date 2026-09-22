use crate::config::Config;
use crate::download::DownloadInfo;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Mutex, OnceLock};
use tauri::AppHandle;

#[derive(Clone, Serialize)]
pub struct Status {
    pub tunnel_up: bool,
    pub webui_ok: bool,
    pub last_error: Option<String>,
}

pub struct AppState {
    pub cfg: Mutex<Config>,
    pub status: Mutex<Status>,
    pub logs: Mutex<VecDeque<String>>,
    /// 我们自己的 cookie(用于探测 WebUI 真实状态)
    pub cookie: Mutex<Option<String>>,
    /// ssh 隧道子进程 PID(断开时用于杀掉)
    pub tunnel_pid: Mutex<Option<u32>>,
    pub stop: AtomicBool,
    pub running: AtomicBool,
    pub force_reauth: AtomicBool,
    pub need_webreload: AtomicBool,
    /// 主窗口是否正显示着 WebUI(单窗口方案)
    pub webui_shown: AtomicBool,
    /// 配置页(前端)的 URL,断开后导航回它
    pub panel_url: Mutex<Option<String>>,
    /// 日志文件(落盘,白屏/重启后也能回看案发现场)
    pub log_file: Mutex<Option<std::fs::File>>,
    /// 下载列表(最近 20 条,新的在前)
    pub downloads: Mutex<Vec<DownloadInfo>>,
    pub next_download_id: AtomicU64,
    /// 系统托盘图标句柄(状态推送用)
    pub tray: OnceLock<tauri::tray::TrayIcon>,
    pub handle: OnceLock<AppHandle>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            cfg: Mutex::new(Config::default()),
            status: Mutex::new(Status {
                tunnel_up: false,
                webui_ok: false,
                last_error: None,
            }),
            logs: Mutex::new(VecDeque::with_capacity(200)),
            cookie: Mutex::new(None),
            tunnel_pid: Mutex::new(None),
            stop: AtomicBool::new(false),
            running: AtomicBool::new(false),
            force_reauth: AtomicBool::new(false),
            need_webreload: AtomicBool::new(false),
            webui_shown: AtomicBool::new(false),
            panel_url: Mutex::new(None),
            log_file: Mutex::new(None),
            downloads: Mutex::new(Vec::new()),
            next_download_id: AtomicU64::new(1),
            tray: OnceLock::new(),
            handle: OnceLock::new(),
        }
    }
}

impl AppState {
    pub fn log(&self, msg: impl AsRef<str>) {
        let line = msg.as_ref().to_string();
        if let Ok(mut logs) = self.logs.lock() {
            if logs.len() >= 200 {
                logs.pop_front();
            }
            logs.push_back(line.clone());
        }
        // 落盘(白屏/崩溃/重启后也能回看)
        if let Ok(mut guard) = self.log_file.lock() {
            if let Some(file) = guard.as_mut() {
                use std::io::Write;
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
            }
        }
    }

    /// 直接往环形缓冲里塞一行(不写文件,用于预载历史日志)
    pub fn log_memory_only(&self, line: &str) {
        if let Ok(mut logs) = self.logs.lock() {
            if logs.len() >= 200 {
                logs.pop_front();
            }
            logs.push_back(line.to_string());
        }
    }

    // ---- 下载列表操作 ----

    pub fn push_download(&self, info: DownloadInfo) {
        if let Ok(mut v) = self.downloads.lock() {
            v.insert(0, info);
            if v.len() > 20 {
                v.truncate(20);
            }
        }
    }

    pub fn update_download<F: FnOnce(&mut DownloadInfo)>(&self, id: u64, f: F) {
        if let Ok(mut v) = self.downloads.lock() {
            if let Some(d) = v.iter_mut().find(|d| d.id == id) {
                f(d);
            }
        }
    }

    /// 用 Content-Disposition 给出的真名更新目标路径。
    pub fn rename_download(&self, id: u64, name: &str) {
        if let Ok(mut v) = self.downloads.lock() {
            if let Some(d) = v.iter_mut().find(|d| d.id == id) {
                let dir = std::path::Path::new(&d.dest)
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(std::env::temp_dir);
                let dest = crate::download::unique_path(&dir, name);
                d.file_name = name.to_string();
                d.dest = dest.to_string_lossy().to_string();
            }
        }
    }

    /// 浏览器托管的下载(blob 等)完成时,按 URL 把对应条目标记完成。
    pub fn finish_by_url(&self, url: &str, ok: bool, err: Option<String>) {
        if let Ok(mut v) = self.downloads.lock() {
            for d in v
                .iter_mut()
                .filter(|d| d.url == url && (d.status == "传输中" || d.status == "下载中"))
            {
                d.status = if ok { "已完成".into() } else { "失败".into() };
                d.error = err.clone();
            }
        }
    }
}
