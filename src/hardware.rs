use crate::{
    common::*,
    nvml::{Device, Nvml},
    writer::Client,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Default)]
pub struct Snapshot {
    pub sensors: Vec<Value>,
    pub fans: Vec<Value>,
    pub channels: Vec<Value>,
    pub chips: Vec<Value>,
}
pub trait Hardware: Send {
    fn sample(&mut self, force: bool) -> Result<Snapshot>;
    fn acquire(&mut self, key: &str, duty: f64) -> Result<Value>;
    fn write(&mut self, key: &str, duty: f64) -> Result<Value>;
    fn restore(&mut self, lease: &Value) -> Result<()>;
    fn disarm(&mut self, leases: &[Value]) -> Result<()>;
    fn errors(&self) -> (Option<String>, Option<String>) {
        (None, None)
    }
}
fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_owned()
}
fn number_at(path: &Path) -> Option<f64> {
    // Parse the read buffer directly; numeric sampling needs no trimmed copy.
    fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
}
fn named(path: &Path, default: &str) -> String {
    let n = read(path);
    if n.is_empty() {
        default.to_owned()
    } else {
        n
    }
}
fn entries(path: &Path) -> Vec<PathBuf> {
    let mut result: Vec<_> = fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    result.sort();
    result
}
fn can_write(path: &Path) -> bool {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    CString::new(path.as_os_str().as_bytes())
        .is_ok_and(|p| unsafe { libc::access(p.as_ptr(), libc::W_OK) == 0 })
}
fn pwm_name(s: &str) -> bool {
    s.strip_prefix("pwm")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}
struct Reading {
    meta: Value,
    path: PathBuf,
}
#[derive(Clone)]
struct Channel {
    meta: Value,
    pwm: PathBuf,
    enable: PathBuf,
    watchdog: PathBuf,
}
struct GpuEntry {
    device: Device,
    channels: Vec<Value>,
}
pub struct Real {
    root: PathBuf,
    data: Option<PathBuf>,
    at: Option<Instant>,
    invalidated: bool,
    sensors: Vec<Reading>,
    fans: Vec<Reading>,
    channels: HashMap<String, Channel>,
    channel_order: Vec<String>,
    chips: Vec<Value>,
    nvml: Option<Nvml>,
    gpu_entries: Vec<GpuEntry>,
    gpu_at: Option<Instant>,
    gpu_invalidated: bool,
    gpu_retry: Instant,
    gpu_error: Option<String>,
    control_error: Option<String>,
    client: Option<Client>,
    gpu_leases: HashMap<String, Value>,
    last_pwm: HashMap<String, (i32, Instant)>,
    watchdogs: HashMap<PathBuf, Instant>,
    fallback_at: Option<Instant>,
    fallback: Vec<Value>,
    fallback_failed: bool,
}
impl Real {
    pub fn new(data: Option<PathBuf>) -> Self {
        Self::with_root(PathBuf::from("/sys/class/hwmon"), data)
    }
    pub fn with_root(root: PathBuf, data: Option<PathBuf>) -> Self {
        Self {
            root,
            data,
            at: None,
            invalidated: false,
            sensors: vec![],
            fans: vec![],
            channels: HashMap::new(),
            channel_order: vec![],
            chips: vec![],
            nvml: None,
            gpu_entries: vec![],
            gpu_at: None,
            gpu_invalidated: false,
            gpu_retry: Instant::now(),
            gpu_error: None,
            control_error: None,
            client: None,
            gpu_leases: HashMap::new(),
            last_pwm: HashMap::new(),
            watchdogs: HashMap::new(),
            fallback_at: None,
            fallback: vec![],
            fallback_failed: false,
        }
    }
    fn discover(&mut self) {
        self.sensors.clear();
        self.fans.clear();
        self.channels.clear();
        self.chips.clear();
        for hw in entries(&self.root) {
            if !hw
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with("hwmon")
            {
                continue;
            }
            let name = named(&hw.join("name"), "unknown");
            let Ok(real) = hw.canonicalize() else {
                continue;
            };
            let physical = if real
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == "hwmon")
            {
                real.parent().unwrap().parent().unwrap_or(&real)
            } else {
                &real
            };
            let id = hash(format!("{name}:{}", physical.display()).as_bytes())[..16].to_owned();
            self.chips.push(json!({"name":name,"path":hw,"id":id}));
            for path in entries(&real) {
                let Some(file) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if let Some(key) = file.strip_suffix("_input") {
                    let meta = json!({"id":format!("{id}/{key}"),"name":named(&real.join(format!("{key}_label")),key),"chip":name});
                    if key.starts_with("temp") {
                        let mut m = meta;
                        m["kind"] = json!(if ["coretemp", "k10temp", "zenpower"]
                            .contains(&name.as_str())
                        {
                            "cpu"
                        } else {
                            "other"
                        });
                        self.sensors.push(Reading { meta: m, path });
                    } else if key.starts_with("fan") {
                        self.fans.push(Reading { meta, path });
                    }
                } else if pwm_name(file) {
                    let enable = real.join(format!("{file}_enable"));
                    let watchdog = real.join("fan_control_watchdog");
                    let supported = [
                        "nct6687", "nct6775", "nct6776", "nct6779", "nct6791", "nct6792",
                        "nct6793", "nct6795", "nct6796", "nct6797", "nct6798", "nct6799",
                    ]
                    .contains(&name.as_str());
                    let reason = if !supported {
                        Some(format!("{name} 的控制模式尚未验证，仅监控"))
                    } else if !enable.exists() {
                        Some("驱动未提供自动模式恢复接口".into())
                    } else if !can_write(&path) || !can_write(&enable) {
                        Some("需要写入权限：使用 sudo 控制会话或后台服务".into())
                    } else {
                        None
                    };
                    let key = format!("{id}/{file}");
                    let tach = file.replacen("pwm", "fan", 1);
                    let rpm = if name == "nct6687" && real.join(format!("{tach}_input")).exists() {
                        Some(format!("{id}/{tach}"))
                    } else {
                        None
                    };
                    let meta = json!({"id":key,"name":named(&real.join(format!("{tach}_label")),file),"chip":name,"writable":reason.is_none(),"reason":reason,"path":hw.join(file),"watchdog":watchdog.exists(),"rpm_sensor_id":rpm});
                    self.channels.insert(
                        key,
                        Channel {
                            meta,
                            pwm: path,
                            enable,
                            watchdog,
                        },
                    );
                }
            }
        }
        self.channel_order = self.channels.keys().cloned().collect();
        self.channel_order.sort();
        self.at = Some(Instant::now());
        self.invalidated = false;
    }
    fn gpu(&mut self, s: &mut Snapshot, force: bool) {
        if self.nvml.is_none() && Instant::now() >= self.gpu_retry {
            match Nvml::new() {
                Ok(n) => {
                    self.nvml = Some(n);
                    self.gpu_at = None;
                }
                Err(e) => {
                    self.gpu_error = Some(e.1);
                    self.gpu_retry = Instant::now() + Duration::from_secs(30);
                }
            }
        }
        let Some(n) = self.nvml.as_mut() else {
            self.fallback_gpu(s);
            return;
        };
        let sample = (|| -> Result<()> {
            if force
                || self.gpu_at.is_none()
                || (self.gpu_invalidated
                    && self
                        .gpu_at
                        .is_some_and(|at| at.elapsed() >= Duration::from_secs(30)))
            {
                let devices = n.devices()?;
                let mut metadata = vec![];
                for device in devices {
                    let mut channels = vec![];
                    match n.uint(b"nvmlDeviceGetNumFans\0", device.handle) {
                        Ok(count) => {
                            for index in 0..count {
                                let key = format!("nvidia/{}/fan{index}", device.uuid);
                                let mut reason = None;
                                let mut limits = (30, 100);
                                match n.limits(device.handle).and_then(|l| {
                                    n.original(&key)?;
                                    Ok(l)
                                }) {
                                    Ok(l) => limits = l,
                                    Err(e) => reason = Some(e.1),
                                }
                                if reason.is_none()
                                    && (self.data.is_none() || unsafe { libc::geteuid() } != 0)
                                {
                                    reason = Some("NVIDIA 调速需要 sudo 控制会话或后台服务".into());
                                }
                                channels.push(json!({"id":key,"name":format!("{} · 风扇 {}",device.name,index+1),"chip":"nvidia","backend":"nvidia",
                                "writable":reason.is_none(),"reason":reason,"path":"NVML","watchdog":false,"software_watchdog":true,
                                "min_duty":limits.0,"max_duty":limits.1,"present":true,"curve_zero_auto":true,
                                "temperature_sensor_id":format!("nvidia/{}",device.uuid),"rpm_sensor_id":format!("{key}/rpm"),"rpm_is_feedback":false,"index":index}));
                            }
                        }
                        Err(e) => self.control_error = Some(e.1),
                    }
                    metadata.push(GpuEntry { device, channels });
                }
                self.gpu_entries = metadata;
                self.gpu_at = Some(Instant::now());
                self.gpu_invalidated = false;
            }
            let mut temp_error = false;
            let mut errors = vec![];
            for entry in &self.gpu_entries {
                let d = &entry.device;
                let temp = n.temperature(d.handle).ok();
                temp_error |= temp.is_none();
                let sid = format!("nvidia/{}", d.uuid);
                s.sensors.push(
                    json!({"id":sid,"name":d.name,"chip":"nvidia","kind":"gpu","value":temp}),
                );
                s.chips.push(json!({"id":sid,"name":d.name,"path":"NVML"}));
                for meta in &entry.channels {
                    let index = meta["index"].as_u64().unwrap() as u32;
                    let mut c = meta.clone();
                    c.as_object_mut().unwrap().remove("index");
                    let policy = n.fan_uint(b"nvmlDeviceGetFanControlPolicy_v2\0", d.handle, index);
                    let duty = n.fan_uint(b"nvmlDeviceGetFanSpeed_v2\0", d.handle, index);
                    if policy.is_err() || duty.is_err() {
                        c["writable"] = json!(false);
                        c["reason"] = json!("NVIDIA 读数不可用");
                        // Keep the discovery timestamp so a read failure cannot
                        // turn the 30-second retry into enumeration every tick.
                        self.gpu_invalidated = true;
                    }
                    c["mode"] = json!(policy.ok());
                    c["duty"] = json!(duty.ok());
                    if let Some(e) = c["reason"].as_str() {
                        errors.push(e.to_owned());
                    }
                    s.fans.push(json!({"id":c["rpm_sensor_id"],"name":c["name"],"chip":"nvidia","rpm":n.rpm(d.handle,index).ok(),"rpm_is_feedback":false}));
                    s.channels.push(c);
                }
            }
            self.gpu_error = if temp_error {
                Some("GPU 温度不可用".into())
            } else {
                None
            };
            if !errors.is_empty() {
                self.control_error = Some(errors.join("；"));
            } else if !self.gpu_entries.is_empty() {
                self.control_error = None;
            }
            self.gpu_invalidated |= temp_error || !errors.is_empty();
            Ok(())
        })();
        if let Err(e) = sample {
            self.gpu_error = Some(e.1.clone());
            self.control_error = Some(e.1);
            self.nvml = None;
            self.gpu_entries.clear();
            self.gpu_retry = Instant::now() + Duration::from_secs(10);
        }
        if !s.sensors.iter().any(|v| v["kind"] == "gpu") {
            self.fallback_gpu(s);
        }
    }
    fn fallback_gpu(&mut self, s: &mut Snapshot) {
        // Compatibility fallback only. Never fork nvidia-smi on the normal NVML
        // path. Back off failures and bound a hung subprocess to two seconds.
        if self.fallback_at.is_none_or(|a| {
            a.elapsed() >= Duration::from_secs(if self.fallback_failed { 30 } else { 3 })
        }) {
            self.fallback_at = Some(Instant::now());
            self.fallback.clear();
            use std::process::{Command, Stdio};
            let query = (|| -> Result<String> {
                let mut child = Command::new("nvidia-smi")
                    .args([
                        "--query-gpu=uuid,name,temperature.gpu",
                        "--format=csv,noheader,nounits",
                    ])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()?;
                let limit = Instant::now() + Duration::from_secs(2);
                while child.try_wait()?.is_none() {
                    if Instant::now() >= limit {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(conflict("nvidia-smi 超时"));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let out = child.wait_with_output()?;
                if !out.status.success() {
                    return Err(conflict("nvidia-smi 查询失败"));
                }
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            })();
            match query {
                Ok(out) => {
                    for line in out.lines() {
                        let parts: Vec<_> = line.split(',').map(str::trim).collect();
                        if parts.len() == 3 {
                            if let Ok(t) = parts[2].parse::<f64>() {
                                if (-40.0..=150.).contains(&t) {
                                    self.fallback.push(json!({"id":format!("nvidia/{}",parts[0]),"name":parts[1],"chip":"nvidia","kind":"gpu","value":t}));
                                }
                            }
                        }
                    }
                    self.fallback_failed = self.fallback.is_empty();
                    if !self.fallback_failed {
                        self.gpu_error = None;
                    }
                }
                Err(e) => {
                    self.gpu_error = Some(e.1);
                    self.fallback_failed = true;
                }
            }
        }
        s.sensors.extend(self.fallback.clone());
    }
    fn request(&mut self, r: Value) -> Result<Value> {
        if self.client.is_none() {
            let data = self
                .data
                .as_ref()
                .ok_or_else(|| conflict("当前为监控模式"))?;
            if unsafe { libc::geteuid() } != 0 {
                return Err(conflict("NVIDIA 调速需要 root"));
            }
            self.client = Some(Client::start(data)?);
        }
        let result = self.client.as_mut().unwrap().request(&r);
        if self.client.as_ref().is_some_and(|c| c.broken) {
            self.client = None;
        }
        result
    }
    fn restore_paths(l: &Value) -> Result<()> {
        let pwm = Path::new(text(&l["paths"][0]));
        let enable = Path::new(text(&l["paths"][1]));
        fs::write(pwm, format!("{}\n", num(&l["original"][0]) as i32))?;
        fs::write(enable, format!("{}\n", num(&l["original"][1]) as i32))?;
        Ok(())
    }
}
impl Hardware for Real {
    fn sample(&mut self, force: bool) -> Result<Snapshot> {
        if force
            || self.at.is_none()
            || (self.invalidated
                && self
                    .at
                    .is_some_and(|at| at.elapsed() >= Duration::from_secs(30)))
        {
            self.discover();
        }
        let mut s = Snapshot {
            chips: self.chips.clone(),
            ..Snapshot::default()
        };
        let mut stale = false;
        for r in &self.sensors {
            let mut m = r.meta.clone();
            let v = number_at(&r.path)
                .map(|v| v / 1000.)
                .filter(|v| (-40.0..=150.).contains(v));
            stale |= v.is_none();
            m["value"] = json!(v);
            s.sensors.push(m);
        }
        for r in &self.fans {
            let mut m = r.meta.clone();
            let v = number_at(&r.path);
            stale |= v.is_none();
            m["rpm"] = json!(v);
            s.fans.push(m);
        }
        for key in &self.channel_order {
            let c = &self.channels[key];
            let mut m = c.meta.clone();
            let duty = number_at(&c.pwm);
            let mode = number_at(&c.enable);
            if duty.is_none() || mode.is_none() {
                m["writable"] = json!(false);
                m["reason"] = json!("通道读数不可用");
                stale = true;
            }
            m["duty"] = duty
                .map(|v| rounded(v / 255. * 100.))
                .unwrap_or(Value::Null);
            m["mode"] = json!(mode);
            s.channels.push(m);
        }
        if stale {
            self.invalidated = true;
        }
        self.gpu(&mut s, force);
        Ok(s)
    }
    fn acquire(&mut self, key: &str, duty: f64) -> Result<Value> {
        if key.starts_with("nvidia/") {
            let l = self.request(json!({"action":"acquire","channel_id":key,"duty":duty}))?;
            self.gpu_leases.insert(key.into(), l.clone());
            return Ok(l);
        }
        let c = self
            .channels
            .get(key)
            .ok_or_else(|| conflict("通道离线"))?
            .clone();
        if !yes(&c.meta["writable"]) {
            return Err(conflict(text(&c.meta["reason"])));
        }
        let value = number_at(&c.pwm).ok_or_else(|| conflict("无法读取原始 PWM"))?;
        let mode = number_at(&c.enable)
            .filter(|v| [0., 1., 2., 3., 4., 5.].contains(v))
            .ok_or_else(|| conflict("无法读取原始控制模式"))?;
        let l =
            json!({"channel_id":key,"paths":[c.pwm,c.enable,c.watchdog],"original":[value,mode]});
        if yes(&c.meta["watchdog"]) {
            fs::write(&c.watchdog, "15\n")?;
        }
        let result = (|| {
            fs::write(&c.pwm, "255\n")?;
            fs::write(&c.enable, "1\n")?;
            self.last_pwm.remove(key);
            self.write(key, duty)
        })();
        if let Err(e) = result {
            let _ = Self::restore_paths(&l);
            return Err(e);
        }
        Ok(l)
    }
    fn write(&mut self, key: &str, duty: f64) -> Result<Value> {
        if key.starts_with("nvidia/") {
            let l = self
                .gpu_leases
                .get(key)
                .ok_or_else(|| conflict("NVIDIA 通道尚未接管"))?;
            return self.request(
                json!({"action":"write","channel_id":key,"token":l["token"],"duty":duty}),
            );
        }
        let c = self.channels.get(key).ok_or_else(|| conflict("通道离线"))?;
        let pwm = (duty.clamp(0., 100.) / 100. * 255.).round_ties_even() as i32;
        if self
            .last_pwm
            .get(key)
            .is_none_or(|(last, at)| *last != pwm || at.elapsed() >= Duration::from_secs(3))
        {
            fs::write(&c.pwm, format!("{pwm}\n"))?;
            self.last_pwm.insert(key.into(), (pwm, Instant::now()));
        }
        // Heartbeat is independent of PWM changes, shared once per chip.
        if yes(&c.meta["watchdog"])
            && self
                .watchdogs
                .get(&c.watchdog)
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(3))
        {
            fs::write(&c.watchdog, "15\n")?;
            self.watchdogs.insert(c.watchdog.clone(), Instant::now());
        }
        Ok(Value::Null)
    }
    fn restore(&mut self, l: &Value) -> Result<()> {
        let key = text(&l["channel_id"]);
        if l["backend"] == "nvidia" {
            self.request(json!({"action":"restore","channel_id":key,"token":l["token"]}))?;
            self.gpu_leases.remove(key);
        } else {
            // Reconnect by stable chip/device identity, never by a recycled
            // class/hwmonN symlink. The hwmon index may change after a reload.
            let mut restored = l.clone();
            if let Some(c) = self.channels.get(key) {
                restored["paths"] = json!([c.pwm, c.enable, c.watchdog]);
            }
            Self::restore_paths(&restored)?;
            self.last_pwm.remove(key);
        }
        Ok(())
    }
    fn disarm(&mut self, leases: &[Value]) -> Result<()> {
        for l in leases {
            let path = self
                .channels
                .get(text(&l["channel_id"]))
                .map(|c| c.watchdog.as_path())
                .or_else(|| l["paths"][2].as_str().map(Path::new));
            if let Some(p) = path {
                if p.exists() {
                    fs::write(p, "0\n")?;
                    self.watchdogs.remove(p);
                }
            }
        }
        Ok(())
    }
    fn errors(&self) -> (Option<String>, Option<String>) {
        (self.gpu_error.clone(), self.control_error.clone())
    }
}

pub struct Demo {
    pub duties: HashMap<String, f64>,
    pub fixed: Option<Snapshot>,
    start: Instant,
}
impl Demo {
    pub fn new() -> Self {
        Self {
            duties: [
                ("demo/pwm1".into(), 38.),
                ("demo/pwm2".into(), 42.),
                ("demo/pwm3".into(), 42.),
            ]
            .into(),
            fixed: None,
            start: Instant::now(),
        }
    }
}
impl Hardware for Demo {
    fn sample(&mut self, _: bool) -> Result<Snapshot> {
        if let Some(s) = &self.fixed {
            return Ok(s.clone());
        }
        let t = self.start.elapsed().as_secs_f64();
        let mut s = Snapshot {
            sensors: vec![
                json!({"id":"demo/cpu","name":"CPU Package","chip":"demo","kind":"cpu","value":rounded(48.+8.*(t/20.).sin())}),
                json!({"id":"demo/gpu","name":"RTX 5080 · 演示","chip":"demo","kind":"gpu","value":rounded(44.+9.*(t/30.).sin())}),
                json!({"id":"demo/board","name":"主板","chip":"demo","kind":"other","value":35}),
            ],
            ..Snapshot::default()
        };
        for (i, name) in ["CPU_FAN", "SYS_FAN1", "SYS_FAN2"].iter().enumerate() {
            let key = format!("demo/pwm{}", i + 1);
            let rpm = format!("demo/fan{}", i + 1);
            let duty = self.duties[&key];
            s.channels.push(json!({"id":key,"name":name,"chip":"demo","duty":duty,"mode":1,"writable":true,"reason":null,"path":"演示设备","watchdog":false,"rpm_sensor_id":rpm}));
            s.fans
                .push(json!({"id":rpm,"name":name,"chip":"demo","rpm":(duty*21.).round()}));
        }
        s.chips = vec![json!({"name":"demo","path":"演示设备","id":"demo"})];
        Ok(s)
    }
    fn acquire(&mut self, key: &str, duty: f64) -> Result<Value> {
        let l = json!({"key":key,"duty":self.duties.get(key).ok_or_else(||conflict("通道离线"))?});
        self.write(key, duty)?;
        Ok(l)
    }
    fn write(&mut self, key: &str, duty: f64) -> Result<Value> {
        self.duties.insert(key.into(), duty);
        Ok(Value::Null)
    }
    fn restore(&mut self, l: &Value) -> Result<()> {
        self.duties.insert(text(&l["key"]).into(), num(&l["duty"]));
        Ok(())
    }
    fn disarm(&mut self, _: &[Value]) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::tests::Temp;
    #[test]
    fn healthy_background_keeps_metadata_until_manual_scan() {
        let t = Temp::new();
        let chip = t.0.join("hwmon0");
        fs::create_dir(&chip).unwrap();
        fs::write(chip.join("name"), "nct6687").unwrap();
        fs::write(chip.join("temp1_input"), "50000").unwrap();
        fs::write(chip.join("temp1_label"), "old name").unwrap();
        let mut h = Real::with_root(t.0.clone(), None);
        h.sample(true).unwrap();
        h.at = Some(Instant::now() - Duration::from_secs(600));
        fs::write(chip.join("temp1_label"), "new name").unwrap();
        fs::write(chip.join("temp1_input"), "60000").unwrap();
        let s = h.sample(false).unwrap();
        assert_eq!(s.sensors[0]["name"], "old name");
        assert_eq!(num(&s.sensors[0]["value"]), 60.);
        let s = h.sample(true).unwrap();
        assert_eq!(s.sensors[0]["name"], "new name");
    }
    #[test]
    fn invalid_readings_are_not_cached_and_discovery_retries_are_bounded() {
        let t = Temp::new();
        let chip = t.0.join("hwmon0");
        fs::create_dir(&chip).unwrap();
        fs::write(chip.join("name"), "nct6687").unwrap();
        fs::write(chip.join("temp1_input"), "50000").unwrap();
        let mut h = Real::with_root(t.0.clone(), None);
        h.sample(true).unwrap();
        fs::remove_file(chip.join("temp1_input")).unwrap();
        assert!(h.sample(false).unwrap().sensors[0]["value"].is_null());
        assert!(h.invalidated);
        fs::write(chip.join("temp1_label"), "reloaded").unwrap();
        fs::write(chip.join("temp1_input"), "61000").unwrap();
        let s = h.sample(false).unwrap();
        assert_ne!(s.sensors[0]["name"], "reloaded");
        h.at = Some(Instant::now() - Duration::from_secs(31));
        let s = h.sample(false).unwrap();
        assert_eq!(s.sensors[0]["name"], "reloaded");
        assert!(!h.invalidated);
    }
    #[test]
    fn unknown_hwmon_driver_remains_read_only() {
        let t = Temp::new();
        let chip = t.0.join("hwmon0");
        fs::create_dir(&chip).unwrap();
        for (name, value) in [("name", "unknown"), ("pwm1", "100"), ("pwm1_enable", "2")] {
            fs::write(chip.join(name), value).unwrap();
        }
        let mut h = Real::with_root(t.0.clone(), None);
        let s = h.sample(true).unwrap();
        let c = s.channels.iter().find(|c| c["chip"] == "unknown").unwrap();
        assert!(!yes(&c["writable"]));
        assert!(h.acquire(text(&c["id"]), 50.).is_err());
        assert_eq!(fs::read_to_string(chip.join("pwm1")).unwrap(), "100");
    }
}
