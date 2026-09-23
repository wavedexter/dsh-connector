use crate::config::Config;
use crate::state::AppState;
use crate::tunnel;
use std::sync::atomic::Ordering;
use tauri::{Manager, State};

#[tauri::command]
pub fn get_config(state: State<'_, std::sync::Arc<AppState>>) -> Config {
    state.cfg.lock().unwrap().clone()
}

#[tauri::command]
pub fn save_config(
    state: State<'_, std::sync::Arc<AppState>>,
    cfg: Config,
) -> Result<(), String> {
    // 合并规则:文本栏位为空时保留旧值。
    // 教训:表单某栏忘了填就保存,曾把"日志路径"清空,导致 grep 无文件参数
    // 退化成读 stdin 死等,ssh 会话全部挂死(实测复现)。空值永不清除已存配置。
    let merged = {
        let old = state.cfg.lock().unwrap();
        Config {
            host: pick(&cfg.host, &old.host),
            user: pick(&cfg.user, &old.user),
            key_path: pick(&cfg.key_path, &old.key_path),
            dsh_log_path: pick(&cfg.dsh_log_path, &old.dsh_log_path),
            ..cfg
        }
    };
    *state.cfg.lock().unwrap() = merged.clone();
    if let Some(handle) = state.handle.get() {
        if let Ok(dir) = handle.path().app_config_dir() {
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(json) = serde_json::to_string_pretty(&merged) {
                let _ = std::fs::write(dir.join("config.json"), json);
                state.log(format!(
                    "config: 已保存到 {}",
                    dir.join("config.json").display()
                ));
            }
        }
    }
    Ok(())
}

fn pick(new: &str, old: &str) -> String {
    if new.trim().is_empty() && !old.trim().is_empty() {
        old.to_string()
    } else {
        new.to_string()
    }
}

#[tauri::command]
pub fn connect(state: State<'_, std::sync::Arc<AppState>>) -> Result<(), String> {
    state.log("connect: 正在启动隧道与认证循环 …");
    if !state.running.swap(true, Ordering::SeqCst) {
        state.stop.store(false, Ordering::SeqCst);
        let s = (*state).clone();
        tunnel::start_supervisor(s.clone());
        tunnel::start_auth_loop(s.clone());
        tunnel::start_guardian(s);
    } else {
        state.log("connect: 已在运行中");
    }
    Ok(())
}

#[tauri::command]
pub fn disconnect(state: State<'_, std::sync::Arc<AppState>>) -> Result<(), String> {
    state.stop.store(true, Ordering::SeqCst);
    state.running.store(false, Ordering::SeqCst);
    let pid = state.tunnel_pid.lock().unwrap().take();
    if let Some(pid) = pid {
        tunnel::kill_process(pid);
    }
    state.status.lock().unwrap().tunnel_up = false;
    state.log("connect: 已断开");
    Ok(())
}

#[tauri::command]
pub fn reauth(state: State<'_, std::sync::Arc<AppState>>) -> Result<(), String> {
    state.force_reauth.store(true, Ordering::SeqCst);
    Ok(())
}

/// 一键诊断: 用当前配置跑一次 verbose ssh,完整输出进日志面板。
#[tauri::command]
pub fn test_key(state: State<'_, std::sync::Arc<AppState>>) -> Result<(), String> {
    tunnel::run_key_test(&state);
    Ok(())
}

#[tauri::command]
pub fn get_status(state: State<'_, std::sync::Arc<AppState>>) -> Result<crate::state::Status, String> {
    Ok(state.status.lock().unwrap().clone())
}

#[tauri::command]
pub fn get_logs(state: State<'_, std::sync::Arc<AppState>>) -> Result<Vec<String>, String> {
    Ok(state.logs.lock().unwrap().iter().cloned().collect())
}

// ---- 下载 ----

#[tauri::command]
pub fn get_downloads(
    state: State<'_, std::sync::Arc<AppState>>,
) -> Result<Vec<crate::download::DownloadInfo>, String> {
    Ok(state.downloads.lock().unwrap().clone())
}

#[tauri::command]
pub fn cancel_download(
    state: State<'_, std::sync::Arc<AppState>>,
    id: u64,
) -> Result<(), String> {
    let found = state
        .downloads
        .lock()
        .unwrap()
        .iter()
        .find(|d| d.id == id)
        .map(|d| d.cancel.store(true, std::sync::atomic::Ordering::SeqCst));
    if found.is_some() {
        Ok(())
    } else {
        Err("下载不存在".into())
    }
}

#[tauri::command]
pub fn open_downloads_dir() -> Result<(), String> {
    let dir = crate::download::downloads_dir().ok_or("找不到下载目录")?;
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(&dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open")
            .arg(&dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---- 主题 ----

#[tauri::command]
pub fn get_theme(state: State<'_, std::sync::Arc<AppState>>) -> Result<String, String> {
    Ok(state.cfg.lock().unwrap().theme.clone())
}

#[tauri::command]
pub fn set_theme(
    state: State<'_, std::sync::Arc<AppState>>,
    theme: String,
) -> Result<(), String> {
    let theme = if theme == "light" { "light" } else { "dark" }.to_string();
    let cfg = {
        let mut c = state.cfg.lock().unwrap();
        c.theme = theme.clone();
        c.clone()
    };
    if let Some(handle) = state.handle.get() {
        if let Ok(dir) = handle.path().app_config_dir() {
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(json) = serde_json::to_string_pretty(&cfg) {
                let _ = std::fs::write(dir.join("config.json"), json);
            }
        }
    }
    state.log(format!("ui: 主题切换为 {theme}"));
    Ok(())
}

// ---- 密钥管理(给不想学 ssh-keygen 的小白)----

/// 本机默认私钥路径(~/.ssh/id_ed25519 的展开形式)。
#[tauri::command]
pub fn default_key_path() -> Result<String, String> {
    let home = if cfg!(windows) {
        std::env::var("USERPROFILE")
    } else {
        std::env::var("HOME")
    };
    match home {
        Ok(h) => Ok(std::path::Path::new(&h)
            .join(".ssh")
            .join("id_ed25519")
            .to_string_lossy()
            .to_string()),
        Err(_) => Ok(String::new()),
    }
}

/// 收紧私钥文件权限(Windows OpenSSH 对权限挑剔,不修会拒用)。
#[tauri::command]
pub fn fix_key_permissions(path: String) -> Result<(), String> {
    let p = path.trim();
    if p.is_empty() || !std::path::Path::new(p).exists() {
        return Err("私钥文件不存在,请检查路径".into());
    }
    #[cfg(windows)]
    {
        let r1 = std::process::Command::new("icacls")
            .args([p, "/inheritance:r"])
            .output();
        let r2 = std::process::Command::new("icacls")
            .args([p, "/grant:r", "%USERNAME%:R"])
            .output();
        let ok = matches!(&r1, Ok(o) if o.status.success())
            && matches!(&r2, Ok(o) if o.status.success());
        if !ok {
            let e = r2
                .or(r1)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "icacls 执行失败".into());
            return Err(format!("权限修复失败(可能需要以管理员身份运行): {e}"));
        }
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))
        {
            return Err(format!("chmod 失败: {e}"));
        }
    }
    Ok(())
}

/// 生成一对 ed25519 密钥(无密码短语),返回公钥内容。
/// 依赖系统 OpenSSH 客户端自带的 ssh-keygen。
#[tauri::command]
pub fn gen_keypair(state: State<'_, std::sync::Arc<AppState>>, path: String) -> Result<String, String> {
    let p = path.trim().to_string();
    if p.is_empty() {
        return Err("请先填写私钥保存路径".into());
    }
    let target = std::path::Path::new(&p);
    if target.exists() {
        return Err("该路径已有文件。换个路径,或先手动删除旧文件(避免误覆盖)。".into());
    }
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
        }
    }
    let out = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-f", &p, "-N", "", "-C", "dsh-connector"])
        .output()
        .map_err(|e| format!("找不到 ssh-keygen(请先安装 OpenSSH 客户端): {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ssh-keygen 失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // 生成了私钥,顺手把权限修好(Windows 必须)
    let _ = fix_key_permissions(p.clone());
    let pubkey = std::fs::read_to_string(format!("{p}.pub"))
        .map_err(|e| format!("公钥读取失败: {e}"))?;
    state.log(format!("key: 已生成密钥对 → {p}"));
    Ok(pubkey.trim().to_string())
}
