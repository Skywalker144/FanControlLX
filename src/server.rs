use crate::{
    common::*,
    runtime::{Client, Operation},
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    io::{BufReader, Read, Write},
    net::{IpAddr, TcpListener, TcpStream},
    sync::{mpsc, Arc, Mutex, OnceLock},
    time::Duration,
};
const INDEX: &[u8] = include_bytes!("../static/index.html");
const JS: &[u8] = include_bytes!("../static/app.js");
const CURVE_JS: &[u8] = include_bytes!("../static/curve.js");
const CURVE_EDITOR_JS: &[u8] = include_bytes!("../static/curve-editor.js");
const UI_JS: &[u8] = include_bytes!("../static/ui.js");
const THEME_JS: &[u8] = include_bytes!("../static/theme.js");
const CONFIRM_JS: &[u8] = include_bytes!("../static/confirm.js");
const ICONS: &[u8] = include_bytes!("../static/icons.svg");
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
struct Asset {
    paths: &'static [&'static str],
    bytes: &'static [u8],
    kind: &'static str,
    etag: String,
}
fn assets() -> &'static [Asset] {
    static ASSETS: OnceLock<Vec<Asset>> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let js = "text/javascript; charset=utf-8";
        let css = "text/css; charset=utf-8";
        let definitions: &[(&[&str], &[u8], &str)] = &[
            (&["/", "/view"], VIEW, "text/html; charset=utf-8"),
            (&["/edit"], INDEX, "text/html; charset=utf-8"),
            (&["/view.js"], VIEW_JS, js),
            (&["/view.css"], VIEW_CSS, css),
            (&["/curve.js"], CURVE_JS, js),
            (&["/curve-editor.js"], CURVE_EDITOR_JS, js),
            (&["/ui.js"], UI_JS, js),
            (&["/theme.js"], THEME_JS, js),
            (&["/confirm.js"], CONFIRM_JS, js),
            (&["/app.js"], JS, js),
            (&["/style.css"], CSS, css),
            (&["/icons.svg"], ICONS, "image/svg+xml"),
            (&["/favicon.svg"], ICON, "image/svg+xml"),
        ];
        definitions
            .iter()
            .map(|&(paths, bytes, kind)| Asset {
                paths,
                bytes,
                kind,
                etag: format!("\"{}\"", hash(bytes)),
            })
            .collect()
    })
}

// At most two current-generation responses and 256 KiB of retained payloads.
// Idle control cycles only advance an atomic generation; they build no JSON.
#[derive(Default)]
struct ResponseCache {
    generation: u64,
    entries: VecDeque<(String, Arc<[u8]>)>,
    bytes: usize,
}
impl ResponseCache {
    fn get(&mut self, generation: u64, key: &str) -> Option<Arc<[u8]>> {
        if generation < self.generation {
            return None;
        }
        if self.generation != generation {
            self.entries.clear();
            self.bytes = 0;
            self.generation = generation;
        }
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
    fn insert(&mut self, generation: u64, key: String, value: Arc<[u8]>) {
        // A slower HTTP worker must never roll a newer cache generation back.
        if generation < self.generation || value.len() > 256 * 1024 {
            return;
        }
        self.get(generation, &key);
        if let Some(index) = self.entries.iter().position(|(k, _)| *k == key) {
            self.bytes -= self.entries.remove(index).unwrap().1.len();
        }
        while self.entries.len() >= 2 || self.bytes + value.len() > 256 * 1024 {
            self.bytes -= self.entries.pop_front().unwrap().1.len();
        }
        self.bytes += value.len();
        self.entries.push_back((key, value));
    }
}

fn reply(s: &mut TcpStream, status: u16, body: &[u8], kind: &str) -> std::io::Result<()> {
    reply_headers(s, status, body, kind, "no-store", None)
}
fn reply_headers(
    s: &mut TcpStream,
    status: u16,
    body: &[u8],
    kind: &str,
    cache: &str,
    etag: Option<&str>,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let mut header = Vec::with_capacity(600);
    write!(header,"HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nConnection: close\r\nCache-Control: {cache}\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'\r\n")?;
    if status != 304 {
        write!(header, "Content-Length: {}\r\n", body.len())?;
    }
    if let Some(etag) = etag {
        write!(header, "ETag: {etag}\r\n")?;
    }
    header.extend_from_slice(b"\r\n");
    s.write_all(&header)?;
    s.write_all(body)
}
fn handle(
    s: &mut TcpStream,
    engine: &Client,
    cache: &Mutex<ResponseCache>,
    token: &str,
    hostname: &str,
) -> Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(&mut *s);
    let mut raw = vec![];
    // Limit bytes, not just line count; a slow/oversized client cannot allocate
    // arbitrary memory or enqueue a control command before its body is ready.
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
        drop(reader);
        if let Some(asset) = assets().iter().find(|a| a.paths.contains(&path)) {
            let unchanged = headers.get("if-none-match").is_some_and(|value| {
                value.split(',').any(|tag| {
                    let tag = tag.trim();
                    tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == asset.etag
                })
            });
            reply_headers(
                s,
                if unchanged { 304 } else { 200 },
                if unchanged { &[] } else { asset.bytes },
                asset.kind,
                "no-cache",
                Some(&asset.etag),
            )?;
            return Ok(());
        }
        if path == "/api/health" {
            let bytes = serde_json::to_vec(&engine.health())?;
            reply(s, 200, &bytes, "application/json; charset=utf-8")?;
            return Ok(());
        }
        let operation = match path {
            "/api/view" => {
                let params: HashMap<_, _> =
                    query.split('&').filter_map(|p| p.split_once('=')).collect();
                Operation::View {
                    since: params.get("since").map(|s| (*s).to_owned()),
                    after: params.get("after").and_then(|s| s.parse().ok()),
                }
            }
            "/api/state" => {
                let params: HashMap<_, _> =
                    query.split('&').filter_map(|p| p.split_once('=')).collect();
                let after = params
                    .get("after")
                    .and_then(|s| s.parse::<f64>().ok())
                    .filter(|v| v.is_finite() && *v >= 0. && *v <= wall());
                Operation::State {
                    since: params.get("since").map(|s| (*s).to_owned()),
                    after,
                }
            }
            _ => return Err(Error(404, "未找到资源".into())),
        };
        let generation = engine.generation();
        let key = format!("{path}?{query}");
        let cached = cache
            .lock()
            .map_err(|_| conflict("状态缓存异常"))?
            .get(generation, &key);
        let bytes = if let Some(bytes) = cached {
            bytes
        } else {
            let mut value = engine.call(operation)?;
            if path == "/api/state" {
                value["csrf_token"] = json!(token);
            }
            value["version"] = json!(VERSION);
            value["hostname"] = json!(hostname);
            // Serialization and socket writes stay entirely on HTTP workers.
            let bytes: Arc<[u8]> = serde_json::to_vec(&value)?.into();
            cache.lock().map_err(|_| conflict("状态缓存异常"))?.insert(
                generation,
                key,
                bytes.clone(),
            );
            bytes
        };
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
    drop(reader);
    let value: Value = serde_json::from_slice(&body)?;
    let result = engine.call(Operation::Write {
        path: path.into(),
        body: value,
    })?;
    reply(
        s,
        200,
        &serde_json::to_vec(&result)?,
        "application/json; charset=utf-8",
    )?;
    Ok(())
}
pub fn start(listener: TcpListener, engine: Client) -> Result<()> {
    let csrf = Arc::new(token()?);
    let hostname = Arc::new(
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .unwrap_or_else(|_| "localhost".into())
            .trim()
            .to_owned(),
    );
    let cache = Arc::new(Mutex::new(ResponseCache::default()));
    let (tx, rx) = mpsc::sync_channel::<TcpStream>(8);
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..2 {
        let (rx, e, t, h, cache) = (
            rx.clone(),
            engine.clone(),
            csrf.clone(),
            hostname.clone(),
            cache.clone(),
        );
        std::thread::spawn(move || loop {
            let socket = rx.lock().unwrap().recv();
            let Ok(mut socket) = socket else {
                break;
            };
            if let Err(e) = handle(&mut socket, &e, &cache, &t, &h) {
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

    #[test]
    fn response_cache_is_bounded_and_never_rolls_back() {
        let mut cache = ResponseCache::default();
        let body: Arc<[u8]> = vec![0; 128 * 1024].into();
        cache.insert(1, "a".into(), body.clone());
        cache.insert(1, "b".into(), body.clone());
        assert!(Arc::ptr_eq(&cache.get(1, "a").unwrap(), &body));
        cache.insert(1, "c".into(), body.clone());
        assert!(cache.get(1, "a").is_none());
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.bytes, 256 * 1024);
        assert!(cache.get(2, "b").is_none());
        assert!(cache.get(1, "b").is_none());
        assert_eq!(cache.generation, 2);
        cache.insert(1, "late worker".into(), body);
        assert_eq!(cache.generation, 2);
        assert!(cache.entries.is_empty());
        cache.insert(2, "oversized".into(), vec![0; 256 * 1024 + 1].into());
        assert!(cache.entries.is_empty());
    }
}
