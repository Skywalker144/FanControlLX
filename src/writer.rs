use crate::{common::*, config, nvml::Nvml};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        io::{AsRawFd, FromRawFd},
        net::UnixStream,
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub trait Gpu {
    fn original(&mut self, key: &str) -> Result<Value>;
    fn write(&mut self, key: &str, duty: f64) -> Result<Value>;
    fn restore(&mut self, lease: &Value) -> Result<()>;
}
impl Gpu for Nvml {
    fn original(&mut self, k: &str) -> Result<Value> {
        Nvml::original(self, k)
    }
    fn write(&mut self, k: &str, d: f64) -> Result<Value> {
        Nvml::write(self, k, d)
    }
    fn restore(&mut self, l: &Value) -> Result<()> {
        Nvml::restore(self, l)
    }
}
pub struct Writer<G: Gpu> {
    pub gpu: G,
    pub leases: HashMap<String, Value>,
    pub(crate) deadlines: HashMap<String, Instant>,
    journal: PathBuf,
}
impl<G: Gpu> Writer<G> {
    pub fn new(gpu: G, journal: PathBuf) -> Result<Self> {
        let mut leases = HashMap::new();
        if journal.exists() {
            let v: Value = serde_json::from_slice(&fs::read(&journal)?)?;
            if v["version"] != 1 || !v["leases"].is_array() {
                return Err(conflict("NVIDIA 恢复记录格式无效"));
            }
            for l in array(&v["leases"]) {
                let key = text(&l["channel_id"]);
                crate::nvml::parse(key)?;
                if !matches!(l["policy"].as_u64(), Some(0 | 1)) || !l["token"].is_string() {
                    return Err(conflict("NVIDIA 恢复记录无效"));
                }
                if l["policy"] == 1 && l["target"].as_u64().is_none_or(|v| v > 100) {
                    return Err(conflict("NVIDIA 原目标输出无效"));
                }
                if leases.insert(key.to_owned(), l.clone()).is_some() {
                    return Err(conflict("NVIDIA 恢复通道重复"));
                }
            }
        }
        let deadlines = leases.keys().map(|k| (k.clone(), Instant::now())).collect();
        Ok(Self {
            gpu,
            leases,
            deadlines,
            journal,
        })
    }
    fn persist(&self) -> Result<()> {
        config::save(
            &self.journal,
            &json!({"version":1,"leases":self.leases.values().collect::<Vec<_>>()}),
        )
    }
    pub fn restore(&mut self, key: &str) -> Result<()> {
        if let Some(l) = self.leases.get(key).cloned() {
            self.gpu.restore(&l)?;
            self.leases.remove(key);
            if let Err(e) = self.persist() {
                self.leases.insert(key.to_owned(), l);
                return Err(e);
            }
            self.deadlines.remove(key);
        }
        Ok(())
    }
    pub fn expire(&mut self, all: bool) -> Vec<String> {
        let keys: Vec<_> = self
            .leases
            .keys()
            .filter(|k| all || self.deadlines[*k] <= Instant::now())
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|k| self.restore(&k).err().map(|e| e.1))
            .collect()
    }
    pub fn dispatch(&mut self, r: &Value) -> Result<Value> {
        let key = text(&r["channel_id"]);
        crate::nvml::parse(key)?;
        let action = text(&r["action"]);
        if action == "acquire" {
            if self.leases.contains_key(key) {
                return Err(conflict("NVIDIA 通道仍有未恢复租约"));
            }
            let duty = number(&r["duty"], 0., 100., "输出")?;
            let mut l = self.gpu.original(key)?;
            l["token"] = json!(token()?);
            self.leases.insert(key.to_owned(), l.clone());
            self.deadlines
                .insert(key.to_owned(), Instant::now() + Duration::from_secs(10));
            if let Err(e) = self.persist() {
                self.leases.remove(key);
                self.deadlines.remove(key);
                return Err(e);
            }
            let written = (|| {
                if duty != 0. {
                    self.gpu.write(key, 100.)?;
                }
                self.gpu.write(key, duty)
            })();
            if let Err(e) = written {
                self.deadlines.insert(key.to_owned(), Instant::now());
                self.expire(false);
                return Err(e);
            }
            return Ok(json!({"backend":"nvidia","channel_id":key,"token":l["token"]}));
        }
        let Some(l) = self.leases.get(key) else {
            if action == "restore" {
                return Ok(Value::Null);
            }
            return Err(conflict("NVIDIA 控制租约已失效"));
        };
        if l["token"] != r["token"] {
            return Err(conflict("NVIDIA 控制租约已失效"));
        }
        match action {
            "restore" => {
                self.deadlines.insert(key.to_owned(), Instant::now());
                self.restore(key)?;
                Ok(Value::Null)
            }
            "write" => {
                if self.deadlines[key] <= Instant::now() {
                    return Err(conflict("NVIDIA 控制租约已超时，等待恢复"));
                }
                let result = self.gpu.write(key, number(&r["duty"], 0., 100., "输出")?)?;
                self.deadlines
                    .insert(key.to_owned(), Instant::now() + Duration::from_secs(10));
                Ok(result)
            }
            _ => Err(invalid("未知 NVIDIA 控制请求")),
        }
    }
}
pub struct Client {
    reader: BufReader<UnixStream>,
    child: Child,
    pub broken: bool,
}
impl Client {
    pub fn start(data: &Path) -> Result<Self> {
        let (parent, child) = UnixStream::pair()?;
        parent.set_read_timeout(Some(Duration::from_secs(5)))?;
        parent.set_write_timeout(Some(Duration::from_secs(5)))?;
        let fd = child.as_raw_fd();
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.args(["--gpu-writer", "--fd", "3", "--data-dir"])
            .arg(data)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        unsafe {
            cmd.pre_exec(move || {
                if fd != 3 && libc::dup2(fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let process = cmd.spawn()?;
        drop(child);
        let mut client = Self {
            reader: BufReader::new(parent),
            child: process,
            broken: false,
        };
        let ready = client.receive()?;
        if ready["ready"] != true {
            return Err(conflict(text(&ready["error"])));
        }
        Ok(client)
    }
    fn receive(&mut self) -> Result<Value> {
        let mut line = String::new();
        let n = (&mut self.reader).take(65537).read_line(&mut line)?;
        if n == 0 || n > 65536 {
            return Err(conflict("NVIDIA 守护进程失联"));
        }
        Ok(serde_json::from_str(&line)?)
    }
    pub fn request(&mut self, r: &Value) -> Result<Value> {
        let mut bytes = serde_json::to_vec(r)?;
        bytes.push(b'\n');
        let response = self
            .reader
            .get_mut()
            .write_all(&bytes)
            .map_err(Error::from)
            .and_then(|_| self.receive());
        let v = match response {
            Ok(v) => v,
            Err(e) => {
                self.broken = true;
                return Err(e);
            }
        };
        if let Some(e) = v["error"].as_str() {
            return Err(conflict(e));
        }
        Ok(v["result"].clone())
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.reader.get_mut().shutdown(std::net::Shutdown::Both);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
pub fn run(data: &Path, fd: Option<i32>, stop: Arc<AtomicBool>) -> Result<()> {
    use std::io::Read;
    let journal = data.join("nvidia-leases.json");
    if fd.is_none() && !journal.exists() {
        return Ok(());
    }
    fs::create_dir_all(data)?;
    let _lock = crate::lock::take(&data.join("nvidia-writer.lock"))?;
    if fd.is_none() {
        let v: Value = serde_json::from_slice(&fs::read(&journal)?)?;
        if v["version"] == 1 && v["leases"].as_array().is_some_and(|a| a.is_empty()) {
            return Ok(());
        }
    }
    let stream = fd.map(|fd| unsafe { UnixStream::from_raw_fd(fd) });
    let result = (|| {
        let mut writer = Writer::new(Nvml::new()?, journal)?;
        let errors = writer.expire(true);
        if !errors.is_empty() {
            return Err(conflict(errors.join("；")));
        }
        let Some(mut sock) = stream.as_ref().map(|s| s.try_clone()).transpose()? else {
            return Ok(());
        };
        sock.set_read_timeout(Some(Duration::from_secs(1)))?;
        sock.set_write_timeout(Some(Duration::from_secs(2)))?;
        sock.write_all(b"{\"ready\":true}\n")?;
        let mut pending = Vec::new();
        let mut buf = [0; 4096];
        let serve = (|| -> Result<()> {
            while !stop.load(Ordering::Relaxed) {
                for e in writer.expire(false) {
                    eprintln!("NVIDIA 超时恢复: {e}");
                }
                match sock.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => pending.extend_from_slice(&buf[..n]),
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue
                    }
                    Err(e) => return Err(e.into()),
                }
                if pending.len() > 65536 {
                    return Err(invalid("GPU 请求过大"));
                }
                while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                    let line: Vec<_> = pending.drain(..=pos).collect();
                    let response = match serde_json::from_slice(&line)
                        .map_err(Error::from)
                        .and_then(|r| writer.dispatch(&r))
                    {
                        Ok(v) => json!({"result":v}),
                        Err(e) => json!({"error":e.1}),
                    };
                    let mut bytes = serde_json::to_vec(&response)?;
                    bytes.push(b'\n');
                    sock.write_all(&bytes)?;
                }
            }
            Ok(())
        })();
        for _ in 0..10 {
            let errors = writer.expire(true);
            if errors.is_empty() {
                return serve;
            }
            eprintln!("NVIDIA 恢复失败: {}", errors.join("；"));
            std::thread::sleep(Duration::from_secs(1));
        }
        Err(conflict("NVIDIA 恢复失败；已保留恢复记录"))
    })();
    if let (Err(e), Some(mut s)) = (&result, stream) {
        let _ = writeln!(s, "{}", json!({"error":e.1}));
    }
    result
}
use std::io::Read;
