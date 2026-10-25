use crate::{
    common::*,
    runtime::{Client, Operation},
    state::Metadata,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
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
// Control cycles advance the atomic response generation.
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

// Read bounded chunks, preserving buffered body bytes. A worker reuses its
// header storage; header names and values borrow it for this request only.
fn read_headers(reader: &mut impl BufRead, raw: &mut Vec<u8>) -> Result<()> {
    const LIMIT: usize = 16384;
    raw.clear();
    while !raw.ends_with(b"\r\n\r\n") {
        let count = reader
            .take((LIMIT + 1 - raw.len()) as u64)
            .read_until(b'\n', raw)?;
        if raw.len() > LIMIT {
            return Err(Error(413, "请求头过大".into()));
        }
        if count == 0 {
            return Err(invalid("请求头不完整"));
        }
    }
    Ok(())
}
struct Headers<'a>(Vec<(&'a str, &'a str)>);
impl<'a> Headers<'a> {
    fn parse(lines: impl Iterator<Item = &'a str>) -> Result<Self> {
        let mut headers = Self(Vec::with_capacity(16));
        for line in lines.filter(|s| !s.is_empty()) {
            let (key, value) = line.split_once(':').ok_or_else(|| invalid("无效请求头"))?;
            if headers.get(key).is_some() {
                return Err(invalid("请求头重复"));
            }
            headers.0.push((key, value.trim()));
        }
        Ok(headers)
    }
    fn get(&self, key: &str) -> Option<&'a str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| *v)
    }
}
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .filter(|(k, _)| *k == key)
        .map(|(_, v)| v)
        .next_back()
}
fn handle(
    s: &mut TcpStream,
    engine: &Client,
    cache: &Mutex<ResponseCache>,
    token: &str,
    hostname: &str,
    raw: &mut Vec<u8>,
) -> Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::with_capacity(1024, &mut *s);
    read_headers(&mut reader, raw)?;
    let header = std::str::from_utf8(raw).map_err(|_| invalid("请求头编码无效"))?;
    let mut lines = header.split("\r\n");
    let parts: Vec<_> = lines.next().unwrap_or("").split_whitespace().collect();
    if parts.len() != 3 || !["HTTP/1.0", "HTTP/1.1"].contains(&parts[2]) {
        return Err(invalid("无效 HTTP 请求"));
    }
    let headers = Headers::parse(lines)?;
    let host = headers.get("host").unwrap_or("");
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
            "/api/view" => Operation::View {
                since: query_param(query, "since").map(str::to_owned),
                after: query_param(query, "after").and_then(|s| s.parse().ok()),
            },
            "/api/state" => {
                let after = query_param(query, "after")
                    .and_then(|s| s.parse::<f64>().ok())
                    .filter(|v| v.is_finite() && *v >= 0. && *v <= wall());
                Operation::State {
                    since: query_param(query, "since").map(str::to_owned),
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
            let response = engine.call(operation)?;
            // Serialize fields borrowed from the immutable snapshot.
            let bytes: Arc<[u8]> = response.encode(Metadata { token, hostname })?.into();
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
        .is_some_and(|o| o != format!("http://{host}"))
    {
        return Err(Error(403, "不允许跨站请求".into()));
    }
    let provided = headers.get("x-fancontrol-token").unwrap_or("");
    let mismatch = provided.len() != token.len()
        || provided
            .bytes()
            .zip(token.bytes())
            .fold(0, |acc, (a, b)| acc | (a ^ b))
            != 0;
    if mismatch {
        return Err(Error(403, "页面会话已过期，请刷新后重试".into()));
    }
    if headers.get("transfer-encoding").is_some()
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
        &result.encode(Metadata { token, hostname })?,
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
        std::thread::spawn(move || {
            let mut raw = Vec::with_capacity(1024);
            loop {
                let socket = rx.lock().unwrap().recv();
                let Ok(mut socket) = socket else {
                    break;
                };
                if let Err(e) = handle(&mut socket, &e, &cache, &t, &h, &mut raw) {
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
    fn header_chunks_preserve_body_and_reject_oversize_or_partial_requests() {
        let request =
            b"POST /api/config HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}";
        let mut raw = Vec::new();
        for capacity in [1, 2, 7, 1024] {
            let mut reader = BufReader::with_capacity(capacity, request.as_slice());
            read_headers(&mut reader, &mut raw).unwrap();
            assert_eq!(raw, &request[..request.len() - 2]);
            let mut body = String::new();
            reader.read_to_string(&mut body).unwrap();
            assert_eq!(body, "{}");
        }
        let mut oversized = std::io::Cursor::new(vec![b'x'; 16385]);
        assert_eq!(read_headers(&mut oversized, &mut raw).unwrap_err().0, 413);
        let mut partial = std::io::Cursor::new(b"GET / HTTP/1.1\r\nHost: localhost\r\n");
        assert_eq!(read_headers(&mut partial, &mut raw).unwrap_err().0, 400);
        assert!(Headers::parse(["Host: localhost", "hOsT: evil"].into_iter()).is_err());
        let headers =
            Headers::parse(["HOST: localhost", "Content-Type: application/json"].into_iter())
                .unwrap();
        assert_eq!(headers.get("host"), Some("localhost"));
        assert_eq!(headers.get("content-type"), Some("application/json"));
        assert_eq!(
            query_param("after=1&since=revision&after=2", "after"),
            Some("2")
        );
    }

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
