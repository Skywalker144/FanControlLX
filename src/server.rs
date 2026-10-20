use crate::{common::*, engine::Engine};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufReader, Read, Write},
    net::{IpAddr, TcpListener, TcpStream},
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};
const INDEX: &[u8] = include_bytes!("../static/index.html");
const JS: &[u8] = include_bytes!("../static/app.js");
const CURVE_JS: &[u8] = include_bytes!("../static/curve.js");
const CSS: &[u8] = include_bytes!("../static/style.css");
const ICON: &[u8] = include_bytes!("../static/favicon.svg");
const VIEW: &[u8] = include_bytes!("../static/view.html");
const VIEW_JS: &[u8] = include_bytes!("../static/view.js");
const VIEW_CSS: &[u8] = include_bytes!("../static/view.css");
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
fn allowed_host(host: &str) -> bool {
    if host.bytes().any(|b| b.is_ascii_whitespace())
        || host.contains('@')
        || host.contains('/')
        || host.contains('\\')
    {
        return false;
    }
    let name = if let Some(rest) = host.strip_prefix('[') {
        let Some((name, end)) = rest.split_once(']') else {
            return false;
        };
        if !end.is_empty()
            && !end
                .strip_prefix(':')
                .is_some_and(|s| s.parse::<u16>().is_ok())
        {
            return false;
        }
        name
    } else {
        match host.split_once(':') {
            Some((name, port)) => {
                if port.parse::<u16>().is_err() {
                    return false;
                }
                name
            }
            None => host,
        }
    };
    name.eq_ignore_ascii_case("localhost")
        || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}
fn reply(s: &mut TcpStream, status: u16, body: &[u8], kind: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    write!(s,"HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'\r\n\r\n",body.len())?;
    s.write_all(body)
}
fn handle(
    s: &mut TcpStream,
    engine: &Arc<Mutex<Engine>>,
    token: &str,
    hostname: &str,
) -> Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(s.try_clone()?);
    let mut raw = vec![];
    // Limit bytes, not just line count; a slow/oversized client cannot allocate
    // arbitrary memory or hold the control lock while transmitting a request.
    loop {
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        raw.push(byte[0]);
        if raw.ends_with(b"\r\n\r\n") {
            break;
        }
        if raw.len() > 16384 {
            return Err(Error(413, "请求头过大".into()));
        }
    }
    let header = std::str::from_utf8(&raw).map_err(|_| invalid("请求头编码无效"))?;
    let mut lines = header.split("\r\n");
    let parts: Vec<_> = lines.next().unwrap_or("").split_whitespace().collect();
    if parts.len() != 3 || !["HTTP/1.0", "HTTP/1.1"].contains(&parts[2]) {
        return Err(invalid("无效 HTTP 请求"));
    }
    let mut headers = HashMap::new();
    for line in lines.filter(|s| !s.is_empty()) {
        let (k, v) = line.split_once(':').ok_or_else(|| invalid("无效请求头"))?;
        if headers
            .insert(k.to_ascii_lowercase(), v.trim().to_owned())
            .is_some()
        {
            return Err(invalid("请求头重复"));
        }
    }
    let host = headers.get("host").map(String::as_str).unwrap_or("");
    if !allowed_host(host) {
        return Err(Error(403, "仅允许通过 localhost 或回环地址访问".into()));
    }
    let (path, query) = parts[1].split_once('?').unwrap_or((parts[1], ""));
    if parts[0] == "GET" {
        let asset = match path {
            "/" | "/view" => Some((VIEW, "text/html; charset=utf-8")),
            "/edit" => Some((INDEX, "text/html; charset=utf-8")),
            "/view.js" => Some((VIEW_JS, "text/javascript; charset=utf-8")),
            "/view.css" => Some((VIEW_CSS, "text/css; charset=utf-8")),
            "/curve.js" => Some((CURVE_JS, "text/javascript; charset=utf-8")),
            "/app.js" => Some((JS, "text/javascript; charset=utf-8")),
            "/style.css" => Some((CSS, "text/css; charset=utf-8")),
            "/favicon.svg" => Some((ICON, "image/svg+xml")),
            _ => None,
        };
        if let Some((bytes, kind)) = asset {
            reply(s, 200, bytes, kind)?;
            return Ok(());
        }
        let engine = engine.lock().map_err(|_| conflict("控制器异常"))?;
        let value = match path {
            "/api/view" => {
                let mut value = engine.view_state();
                value["version"] = json!(VERSION);
                value["hostname"] = json!(hostname);
                value
            }
            "/api/health" => {
                json!({"ok":engine.error.is_none(),"version":VERSION,"background_service":engine.managed,"runtime":"rust"})
            }
            "/api/state" => {
                let params: HashMap<_, _> =
                    query.split('&').filter_map(|p| p.split_once('=')).collect();
                let after = params
                    .get("after")
                    .and_then(|s| s.parse::<f64>().ok())
                    .filter(|v| v.is_finite() && *v >= 0. && *v <= wall());
                let mut state = engine.state(params.get("since").copied(), after);
                state["csrf_token"] = json!(token);
                state["version"] = json!(VERSION);
                state["hostname"] = json!(hostname);
                state
            }
            _ => return Err(Error(404, "未找到资源".into())),
        };
        let bytes = serde_json::to_vec(&value)?;
        drop(engine);
        reply(s, 200, &bytes, "application/json; charset=utf-8")?;
        return Ok(());
    }
    if parts[0] != "POST" {
        return Err(Error(400, "仅支持 GET/POST".into()));
    }
    if headers
        .get("origin")
        .is_some_and(|o| o != &format!("http://{host}"))
    {
        return Err(Error(403, "不允许跨站请求".into()));
    }
    let provided = headers
        .get("x-fancontrol-token")
        .map(String::as_str)
        .unwrap_or("");
    let mismatch = provided.len() != token.len()
        || provided
            .bytes()
            .zip(token.bytes())
            .fold(0, |acc, (a, b)| acc | (a ^ b))
            != 0;
    if mismatch {
        return Err(Error(403, "页面会话已过期，请刷新后重试".into()));
    }
    if headers.contains_key("transfer-encoding")
        || headers
            .get("content-type")
            .is_none_or(|s| s.split(';').next().unwrap_or("").trim() != "application/json")
    {
        return Err(invalid("需要 application/json"));
    }
    let len = headers
        .get("content-length")
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0 && *n <= 256 * 1024)
        .ok_or_else(|| Error(413, "请求体大小无效".into()))?;
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    let value: Value = serde_json::from_slice(&body)?;
    let result = engine
        .lock()
        .map_err(|_| conflict("控制器异常"))?
        .request(path, &value)?;
    reply(
        s,
        200,
        &serde_json::to_vec(&result)?,
        "application/json; charset=utf-8",
    )?;
    Ok(())
}
pub fn start(listener: TcpListener, engine: Arc<Mutex<Engine>>) -> Result<()> {
    let csrf = Arc::new(token()?);
    let hostname = Arc::new(
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .unwrap_or_else(|_| "localhost".into())
            .trim()
            .to_owned(),
    );
    let (tx, rx) = mpsc::sync_channel::<TcpStream>(8);
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..2 {
        let (rx, e, t, h) = (rx.clone(), engine.clone(), csrf.clone(), hostname.clone());
        std::thread::spawn(move || loop {
            let socket = rx.lock().unwrap().recv();
            let Ok(mut socket) = socket else {
                break;
            };
            if let Err(e) = handle(&mut socket, &e, &t, &h) {
                let _ = reply(
                    &mut socket,
                    e.0,
                    json!({"error":e.1}).to_string().as_bytes(),
                    "application/json; charset=utf-8",
                );
                // Send the complete rejection before closing an unread POST.
                // Closing with queued request bytes would send RST and can
                // truncate the response seen by the browser.
                let _ = socket.shutdown(std::net::Shutdown::Write);
                let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
                let deadline = std::time::Instant::now() + Duration::from_millis(200);
                let mut remaining = 256 * 1024;
                let mut buf = [0; 4096];
                while remaining > 0 && std::time::Instant::now() < deadline {
                    match socket.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => remaining -= n.min(remaining),
                    }
                }
            }
        });
    }
    std::thread::spawn(move || {
        for socket in listener.incoming() {
            match socket {
                Ok(socket) => {
                    let _ = tx.try_send(socket);
                }
                Err(e) => {
                    eprintln!("HTTP accept: {e}");
                    break;
                }
            }
        }
    });
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_validation() {
        for h in ["localhost:8090", "127.0.0.1:8090", "[::1]:8090"] {
            assert!(allowed_host(h));
        }
        for h in [
            "evil.test",
            "127.0.0.1.evil",
            "localhost@evil",
            "[::1]evil",
            "[::]:8090",
            "127.0.0.1:80:90",
            "localhost/",
        ] {
            assert!(!allowed_host(h));
        }
    }
}
