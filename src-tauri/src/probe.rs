use anyhow::{anyhow, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// 极简 HTTP/1.1 GET(不引第三方库),返回状态码、响应头和响应体(截断)。
pub fn http_get(
    host: &str,
    port: u16,
    path: &str,
    cookie: Option<&str>,
) -> Result<HttpResponse> {
    let addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .map_err(|e| anyhow!("地址解析失败 {}:{}: {e}", host, port))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nCache-Control: no-cache\r\n",
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
    let mut body = String::new();
    let _ = reader.read_to_string(&mut body);
    let body = truncate(&body, 300);
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// 探测本地 WebUI 首页状态。
pub fn probe_status(port: u16, cookie: Option<&str>) -> Result<u16> {
    Ok(http_get("127.0.0.1", port, "/", cookie)?.status)
}

/// 从 set-cookie 头里取出 name=value(去掉 Max-Age 等属性)。
pub fn cookie_from_headers(headers: &[(String, String)]) -> Option<String> {
    for (k, v) in headers {
        if k == "set-cookie" {
            if let Some(pair) = v.split(';').next() {
                let pair = pair.trim();
                if pair.contains('=') {
                    return Some(pair.to_string());
                }
            }
        }
    }
    None
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}
