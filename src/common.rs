use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct Error(pub u16, pub String);
pub type Result<T> = std::result::Result<T, Error>;
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self(409, e.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self(400, e.to_string())
    }
}
pub fn invalid(s: impl Into<String>) -> Error {
    Error(400, s.into())
}
pub fn conflict(s: impl Into<String>) -> Error {
    Error(409, s.into())
}
pub fn text(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
pub fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.)
}
pub fn yes(v: &Value) -> bool {
    v.as_bool().unwrap_or(false)
}
pub fn array(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
pub fn hash(s: &[u8]) -> String {
    format!("{:x}", Sha256::digest(s))
}
pub fn wall() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
pub fn rounded(v: f64) -> Value {
    json!((v * 10.).round() / 10.)
}
pub fn token() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hash(&bytes))
}
pub fn number(v: &Value, low: f64, high: f64, name: &str) -> Result<f64> {
    let n = v
        .as_f64()
        .filter(|n| n.is_finite())
        .ok_or_else(|| invalid(format!("{name} 必须是有限数值")))?;
    if !(low..=high).contains(&n) {
        return Err(invalid(format!("{name} 必须在 {low}–{high} 之间")));
    }
    Ok(n)
}
// JSON clients normalize 50.0 to 50. Compare numbers semantically so a browser
// round-trip cannot produce a false optimistic-concurrency conflict.
pub fn equivalent(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equivalent(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, a)| b.get(k).is_some_and(|b| equivalent(a, b)))
        }
        _ => a == b,
    }
}
