use crate::state::AppState;
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Serialize)]
pub struct DownloadInfo {
    pub id: u64,
    pub url: String,
    pub file_name: String,
    pub dest: String,
    pub done: u64,
    pub total: Option<u64>,
    pub speed: f64,
    /// 下载中 / 已完成 / 失败 / 已取消 / 传输中(浏览器托管)
    pub status: String,
    pub error: Option<String>,
    #[serde(skip)]
    pub cancel: Arc<AtomicBool>,
}

/// 用户下载目录。
pub fn downloads_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Downloads"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads"))
    }
}

/// 目录下按文件名挑一个不冲突的路径(重名加 " (n)")。
pub fn unique_path(dir: &PathBuf, name: &str) -> PathBuf {
    let mut path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".into());
    let ext = path.extension().map(|s| s.to_string_lossy().to_string());
    let mut i = 1;
    loop {
        let candidate = match &ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        path = dir.join(candidate);
        if !path.exists() || i > 100 {
            return path;
        }
        i += 1;
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 从 Content-Disposition 里解析文件名。
fn filename_from_cd(headers: &[(String, String)]) -> Option<String> {
    for (k, v) in headers {
        if k != "content-disposition" {
            continue;
        }
        let lower = v.to_ascii_lowercase();
        if let Some(i) = lower.find("filename*=utf-8''") {
            let rest = &v[i + 17..];
            let end = rest
                .find(|c| c == ';' || c == ' ' || c == '"')
                .unwrap_or(rest.len());
            let name = percent_decode(&rest[..end]);
            if !name.is_empty() {
                return Some(name);
            }
        }
        if let Some(i) = lower.find("filename=") {
            let rest = &v[i + 9..];
            let name = if let Some(stripped) = rest.strip_prefix('"') {
                let end = stripped.find('"').unwrap_or(stripped.len());
                &stripped[..end]
            } else {
                let end = rest.find(';').unwrap_or(rest.len());
                &rest[..end]
            };
            let name = name.trim();
            if !name.is_empty() {
                return Some(percent_decode(name));
            }
        }
    }
    None
}

/// 从 URL query 里取一个参数并 % 解码(下载 URL 是 ?path=/home/...&download=1 形态)。
fn query_param(url: &tauri::Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.to_string())
}

/// 总大小探测:URL 的 path 参数指向服务器上的文件,借现成的 SSH 通道 stat 一下。
/// 拿不到就返回 None(进度条按"大小未知"展示,不影响下载本身)。
fn remote_file_size(state: &Arc<AppState>, url: &tauri::Url) -> Option<u64> {
    let path = query_param(url, "path")?;
    if path.is_empty() {
        return None;
    }
    let cfg = state.cfg.lock().unwrap().clone();
    if cfg.host.is_empty() || cfg.user.is_empty() || cfg.key_path.is_empty() {
        return None;
    }
    // 单引号安全转义,防路径里有特殊字符
    let escaped = path.replace('\'', "'\\''");
    let cmd = format!("stat -c%s '{escaped}' 2>/dev/null || wc -c < '{escaped}'");
    let out = crate::tunnel::ssh_exec_command(&cfg, &cmd, false)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse::<u64>().ok()
}

/// 打开一个 HTTP 连接并读完响应头,返回 (状态码, 响应头, 连接读取器)。
fn http_open(
    url: &tauri::Url,
    cookie: Option<&str>,
) -> Result<(u16, Vec<(String, String)>, BufReader<TcpStream>)> {
    let host = url.host_str().ok_or_else(|| anyhow!("URL 无主机"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("URL 无端口"))?;
    let addr: std::net::SocketAddr = format!("{}:{}", host, port)
        .parse()
        .map_err(|e| anyhow!("地址解析失败 {}:{}: {e}", host, port))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let path = match url.query() {
        Some(q) => format!("{}?{}", url.path(), q),
        None => url.path().to_string(),
    };
    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nUser-Agent: dsh-connector/1.0.0\r\n",
        path, host, port
    );
    if let Some(c) = cookie {
        req.push_str("Cookie: ");
        req.push_str(c);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("无法解析状态行: {:?}", status_line.trim()))?;
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(i) = line.find(':') {
            headers.push((
                line[..i].trim().to_ascii_lowercase(),
                line[i + 1..].trim().to_string(),
            ));
        }
    }
    Ok((status, headers, reader))
}

/// 读一段 body 并写入文件;返回读到的字节数(0 = 结束)。
fn read_more(
    reader: &mut BufReader<TcpStream>,
    buf: &mut [u8],
    chunked: bool,
    file: &mut std::fs::File,
    done: &mut u64,
    cancel: &AtomicBool,
) -> Result<u64> {
    if cancel.load(Ordering::SeqCst) {
        return Ok(0);
    }
    if chunked {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line)? == 0 {
            return Ok(0);
        }
        let size_hex = size_line.trim().split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| anyhow!("无效的 chunk 大小: {:?}", size_hex))?;
        if size == 0 {
            let mut tail = [0u8; 2];
            let _ = reader.read_exact(&mut tail);
            return Ok(0);
        }
        let mut remaining = size;
        let mut written = 0u64;
        while remaining > 0 {
            if cancel.load(Ordering::SeqCst) {
                return Ok(0);
            }
            let want = remaining.min(buf.len());
            let n = reader.read(&mut buf[..want])?;
            if n == 0 {
                return Err(anyhow!("chunk 传输中断"));
            }
            file.write_all(&buf[..n])?;
            remaining -= n;
            written += n as u64;
        }
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
        *done += written;
        Ok(written)
    } else {
        let n = reader.read(buf)?;
        if n > 0 {
            file.write_all(&buf[..n])?;
            *done += n as u64;
        }
        Ok(n as u64)
    }
}

/// 下载主流程(带重定向跟随)。
fn run(state: &Arc<AppState>, id: u64, url: &tauri::Url, cookie: Option<&str>) -> Result<()> {
    let mut current = url.clone();
    let mut attempts = 0;
    let (status, headers, mut reader) = loop {
        attempts += 1;
        let (st, hs, rd) = http_open(&current, cookie)?;
        if (301..=308).contains(&st) {
            if attempts > 5 {
                return Err(anyhow!("重定向次数过多"));
            }
            let loc = hs
                .iter()
                .find(|(k, _)| k == "location")
                .map(|(_, v)| v.clone())
                .ok_or_else(|| anyhow!("重定向响应没有 Location 头"))?;
            current = current
                .join(&loc)
                .map_err(|e| anyhow!("重定向地址无效({loc}): {e}"))?;
            continue;
        }
        break (st, hs, rd);
    };
    if status != 200 {
        return Err(anyhow!("服务器返回 HTTP {status}"));
    }
    // Content-Disposition 给出的真名(比 URL 末尾靠谱)
    if let Some(name) = filename_from_cd(&headers) {
        state.rename_download(id, &name);
    }
    let total = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.trim().parse::<u64>().ok());
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));

    // 没有 Content-Length(典型:better-sidebar 的下载端点是 chunked 流式),
    // 但 URL 里带着服务器上的文件路径 —— 用现成的 SSH 通道 stat 一下就知道总大小。
    let total = match total {
        Some(t) => Some(t),
        None => remote_file_size(state, url),
    };

    let (dest, cancel) = {
        let v = state.downloads.lock().unwrap();
        let info = v
            .iter()
            .find(|d| d.id == id)
            .ok_or_else(|| anyhow!("下载记录消失"))?;
        (info.dest.clone(), info.cancel.clone())
    };
    state.update_download(id, |d| d.total = total);
    let part = format!("{}.part", dest);
    let mut file =
        std::fs::File::create(&part).map_err(|e| anyhow!("无法写入 {part}: {e}"))?;
    let mut buf = [0u8; 16384];
    let mut done: u64 = 0;
    let mut last_t = Instant::now();
    let mut last_d: u64 = 0;
    loop {
        if cancel.load(Ordering::SeqCst) {
            drop(file);
            let _ = std::fs::remove_file(&part);
            state.update_download(id, |d| {
                d.status = "已取消".into();
                d.speed = 0.0;
            });
            state.log(format!("download: #{id} 已取消,已清理临时文件"));
            return Ok(());
        }
        let n = read_more(&mut reader, &mut buf, chunked, &mut file, &mut done, &cancel)?;
        if n == 0 {
            break;
        }
        let now = Instant::now();
        if now.duration_since(last_t) >= Duration::from_millis(250) {
            let secs = now.duration_since(last_t).as_secs_f64();
            let speed = if secs > 0.0 {
                (done - last_d) as f64 / secs
            } else {
                0.0
            };
            last_t = now;
            last_d = done;
            state.update_download(id, |d| {
                d.done = done;
                d.speed = speed;
            });
        }
    }
    file.flush()?;
    drop(file);
    std::fs::rename(&part, &dest).map_err(|e| anyhow!("保存失败: {e}"))?;
    state.update_download(id, |d| {
        if d.status == "下载中" {
            d.status = "已完成".into();
        }
        d.done = done;
        d.speed = 0.0;
    });
    state.log(format!("download: #{id} 完成 → {}", dest));
    Ok(())
}

/// 接管一个 WebView 下载请求。返回下载记录 id。
pub fn start_download(state: &Arc<AppState>, url_str: &str, cookie: Option<String>) -> Result<u64> {
    let url: tauri::Url = url_str
        .parse()
        .map_err(|e| anyhow!("URL 无效: {e}"))?;
    let id = state.next_download_id.fetch_add(1, Ordering::SeqCst);
    let fname = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .map(|s| percent_decode(s))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "download.bin".to_string());
    let dir = downloads_dir().unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&dir);
    let dest = unique_path(&dir, &fname);
    state.push_download(DownloadInfo {
        id,
        url: url_str.to_string(),
        file_name: fname,
        dest: dest.to_string_lossy().to_string(),
        done: 0,
        total: None,
        speed: 0.0,
        status: "下载中".into(),
        error: None,
        cancel: Arc::new(AtomicBool::new(false)),
    });
    state.log(format!("download: #{id} 开始 → {}", dest.display()));
    let state2 = state.clone();
    std::thread::spawn(move || match run(&state2, id, &url, cookie.as_deref()) {
        Ok(()) => {
            state2.update_download(id, |d| {
                if d.status == "下载中" {
                    d.status = "已完成".into();
                }
                d.speed = 0.0;
            });
        }
        Err(e) => {
            state2.update_download(id, |d| {
                d.status = "失败".into();
                d.error = Some(e.to_string());
                d.speed = 0.0;
            });
            state2.log(format!("download: #{id} 失败: {e}"));
        }
    });
    Ok(id)
}
