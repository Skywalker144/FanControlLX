mod nvidia;
use crate::{
    common::*,
    nvml::{Device, Nvml},
    writer::Client,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

#[derive(Clone, Default)]
pub struct Snapshot {
    pub sensors: Vec<Value>,
    pub fans: Vec<Value>,
    pub channels: Vec<Value>,
    pub chips: Vec<Value>,
    // Globally unique discovery generation; a snapshot from another source
    // cannot accidentally reuse this source's topology.
    pub(crate) hwmon_generation: u64,
    pub(crate) gpu_generation: u64,
}
pub trait Hardware: Send {
    fn sample(&mut self, force: bool) -> Result<Snapshot>;
    fn sample_into(&mut self, force: bool, snapshot: &mut Snapshot) -> Result<()> {
        *snapshot = self.sample(force)?;
        Ok(())
    }
    fn acquire(&mut self, key: &str, duty: f64) -> Result<Value>;
    fn write(&mut self, key: &str, duty: f64) -> Result<Value>;
    fn write_batch(&mut self, writes: &[(&str, f64)]) -> Vec<Result<Value>> {
        writes
            .iter()
            .map(|(key, duty)| self.write(key, *duty))
            .collect()
    }
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
    // Read sysfs numeric attributes into a bounded stack buffer. Reopen each
    // sample so removed or replaced devices cannot leave a stale descriptor.
    let mut file = fs::File::open(path).ok()?;
    let mut bytes = [0; 64];
    let mut len = 0;
    loop {
        match file.read(&mut bytes[len..]) {
            Ok(0) => break,
            Ok(n) => {
                len += n;
                if len == bytes.len() {
                    return None; // Never accept a truncated numeric prefix.
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    std::str::from_utf8(&bytes[..len])
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
fn update_field(row: &mut Value, field: &str, value: Value) {
    if let Some(current) = row.get_mut(field) {
        *current = value;
    } else {
        row[field] = value;
    }
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
    sensor: Value,
    chip: Value,
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
    generation: u64,
    nvml: Option<Nvml>,
    gpu_entries: Vec<GpuEntry>,
    gpu_generation: u64,
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
            generation: 0,
            nvml: None,
            gpu_entries: vec![],
            gpu_generation: 0,
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
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        self.generation = GENERATION.fetch_add(1, Ordering::Relaxed);
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
        let mut snapshot = Snapshot::default();
        self.sample_into(force, &mut snapshot)?;
        Ok(snapshot)
    }
    fn sample_into(&mut self, force: bool, s: &mut Snapshot) -> Result<()> {
        if force
            || self.at.is_none()
            || (self.invalidated
                && self
                    .at
                    .is_some_and(|at| at.elapsed() >= Duration::from_secs(30)))
        {
            self.discover();
        }
        if s.hwmon_generation != self.generation {
            s.sensors = self.sensors.iter().map(|r| r.meta.clone()).collect();
            s.fans = self.fans.iter().map(|r| r.meta.clone()).collect();
            s.channels = self
                .channel_order
                .iter()
                .map(|key| self.channels[key].meta.clone())
                .collect();
            s.chips.clone_from(&self.chips);
            s.hwmon_generation = self.generation;
            s.gpu_generation = 0;
        }
        let mut stale = false;
        for (r, m) in self.sensors.iter().zip(&mut s.sensors) {
            let v = number_at(&r.path)
                .map(|v| v / 1000.)
                .filter(|v| (-40.0..=150.).contains(v));
            stale |= v.is_none();
            update_field(m, "value", json!(v));
        }
        for (r, m) in self.fans.iter().zip(&mut s.fans) {
            let v = number_at(&r.path);
            stale |= v.is_none();
            update_field(m, "rpm", json!(v));
        }
        for (key, m) in self.channel_order.iter().zip(&mut s.channels) {
            let c = &self.channels[key];
            let duty = number_at(&c.pwm);
            let mode = number_at(&c.enable);
            if duty.is_none() || mode.is_none() {
                update_field(m, "writable", json!(false));
                if m["reason"] != "通道读数不可用" {
                    update_field(m, "reason", json!("通道读数不可用"));
                }
                stale = true;
            } else {
                // Recover from transient read errors without losing the
                // original permission/driver restrictions in the metadata.
                update_field(m, "writable", c.meta["writable"].clone());
                if m["reason"] != c.meta["reason"] {
                    update_field(m, "reason", c.meta["reason"].clone());
                }
            }
            let duty = duty
                .map(|v| rounded(v / 255. * 100.))
                .unwrap_or(Value::Null);
            update_field(m, "duty", duty);
            update_field(m, "mode", json!(mode));
        }
        if stale {
            self.invalidated = true;
        }
        self.gpu(s, force);
        Ok(())
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
    fn write_batch(&mut self, writes: &[(&str, f64)]) -> Vec<Result<Value>> {
        // Board writes keep their independent watchdog cadence. GPU writes
        // share one IPC exchange, but every channel still gets its own result.
        let mut results: Vec<_> = writes
            .iter()
            .map(|(key, duty)| {
                if key.starts_with("nvidia/") {
                    None
                } else {
                    Some(self.write(key, *duty))
                }
            })
            .collect();
        let mut gpu = Vec::new();
        let mut positions = Vec::new();
        for (index, (key, duty)) in writes.iter().enumerate() {
            if !key.starts_with("nvidia/") {
                continue;
            }
            if let Some(lease) = self.gpu_leases.get(*key) {
                gpu.push(json!({"channel_id":key,"token":lease["token"],"duty":duty}));
                positions.push(index);
            } else {
                results[index] = Some(Err(conflict("NVIDIA 通道尚未接管")));
            }
        }
        if !gpu.is_empty() {
            match self.request(json!({"action":"write-batch","writes":gpu})) {
                Ok(mut response) => {
                    for (offset, index) in positions.into_iter().enumerate() {
                        results[index] = Some(
                            match response
                                .as_array_mut()
                                .and_then(|rows| rows.get_mut(offset))
                            {
                                Some(row) if row["error"].is_string() => {
                                    Err(conflict(text(&row["error"])))
                                }
                                Some(row) if row.get("result").is_some() => {
                                    Ok(row["result"].take())
                                }
                                _ => Err(conflict("NVIDIA 批量响应不完整")),
                            },
                        );
                    }
                }
                Err(e) => {
                    for index in positions {
                        results[index] = Some(Err(conflict(&e.1)));
                    }
                }
            }
        }
        results.into_iter().map(|r| r.unwrap()).collect()
    }
    fn restore(&mut self, l: &Value) -> Result<()> {
        let key = text(&l["channel_id"]);
        if l["backend"] == "nvidia" {
            self.request(json!({"action":"restore","channel_id":key,"token":l["token"]}))?;
            self.gpu_leases.remove(key);
            if self.gpu_leases.is_empty() {
                // Close the writer after the last lease is restored. The next
                // acquisition starts a new writer; failed restores keep it alive.
                self.client = None;
            }
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
    fn reused_snapshot_recovers_readings_and_rebuilds_changed_topology() {
        let t = Temp::new();
        let chip = t.0.join("hwmon0");
        fs::create_dir(&chip).unwrap();
        for (name, value) in [
            ("name", "nct6687"),
            ("temp1_input", "50000"),
            ("fan1_input", "1200"),
            ("pwm1", "100"),
            ("pwm1_enable", "2"),
        ] {
            fs::write(chip.join(name), value).unwrap();
        }
        let mut h = Real::with_root(t.0.clone(), None);
        h.gpu_retry = Instant::now() + Duration::from_secs(3600);
        h.fallback_at = Some(Instant::now() + Duration::from_secs(3600));
        let mut s = h.sample(true).unwrap();
        let writable = s.channels[0]["writable"].clone();
        let reason = s.channels[0]["reason"].clone();
        let original_generation = s.hwmon_generation;
        fs::remove_file(chip.join("temp1_input")).unwrap();
        fs::remove_file(chip.join("pwm1_enable")).unwrap();
        fs::write(chip.join("fan1_input"), "0").unwrap();
        h.sample_into(false, &mut s).unwrap();
        assert!(s.sensors[0]["value"].is_null());
        assert_eq!(s.fans[0]["rpm"], 0.);
        assert_eq!(s.channels[0]["writable"], false);
        assert!(s.channels[0]["mode"].is_null());
        fs::write(chip.join("temp1_input"), "61000").unwrap();
        fs::write(chip.join("pwm1_enable"), "2").unwrap();
        h.sample_into(false, &mut s).unwrap();
        assert_eq!(s.sensors[0]["value"], 61.);
        assert_eq!(s.channels[0]["writable"], writable);
        assert_eq!(s.channels[0]["reason"], reason);
        assert_eq!(s.hwmon_generation, original_generation);
        // Reordering/new sensors and metadata changes replace the old layout.
        fs::write(chip.join("temp1_label"), "renamed").unwrap();
        fs::write(chip.join("temp2_input"), "65000").unwrap();
        fs::remove_file(chip.join("fan1_input")).unwrap();
        h.sample_into(true, &mut s).unwrap();
        assert_ne!(s.hwmon_generation, original_generation);
        assert_eq!(s.sensors.len(), 2);
        assert_eq!(s.sensors[0]["name"], "renamed");
        assert_eq!(s.sensors[1]["value"], 65.);
        assert!(s.fans.is_empty());
        // No stale entries survive complete device removal.
        fs::remove_dir_all(chip).unwrap();
        h.sample_into(true, &mut s).unwrap();
        assert!(
            s.sensors.is_empty()
                && s.fans.is_empty()
                && s.channels.is_empty()
                && s.chips.is_empty()
        );
    }
    #[test]
    fn numeric_reads_are_bounded_fresh_and_reject_invalid_input() {
        let t = Temp::new();
        let path = t.0.join("temp1_input");
        for (value, expected) in [
            ("  -12345\n", Some(-12345.)),
            ("0\n", Some(0.)),
            ("123.5", Some(123.5)),
            ("", None),
            ("NaN", None),
            ("inf", None),
            ("123\njunk", None),
        ] {
            fs::write(&path, value).unwrap();
            assert_eq!(number_at(&path), expected);
        }
        fs::write(&path, "1".repeat(64)).unwrap();
        assert_eq!(number_at(&path), None);
        fs::write(&path, [0xff]).unwrap();
        assert_eq!(number_at(&path), None);
        fs::remove_file(&path).unwrap();
        assert_eq!(number_at(&path), None);
        fs::write(&path, "61000").unwrap();
        assert_eq!(number_at(&path), Some(61000.));
    }
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

    #[test]
    #[ignore = "requires tests/mock_nvml.c via LD_LIBRARY_PATH and FANCONTROLLX_MOCK_DIR"]
    fn benchmark_mock_nvml_sampling() {
        if std::env::var_os("FANCONTROLLX_MOCK_DIR").is_none() {
            eprintln!("Skipped: provide the isolated mock NVML library and directory");
            return;
        }
        let t = Temp::new();
        let mut h = Real::with_root(t.0.clone(), None);
        let mut s = h.sample(true).unwrap();
        assert!(s.sensors.iter().any(|row| row["name"] == "Mock GPU"));
        crate::perf_tests::measure("mock_nvml_reused_snapshot", 10000, || {
            h.sample_into(false, &mut s).unwrap();
            std::hint::black_box(&s);
        });
    }

    #[test]
    #[ignore = "opt-in synthetic performance measurement"]
    fn benchmark_hwmon_sampling() {
        let t = Temp::new();
        let chip = t.0.join("hwmon0");
        fs::create_dir(&chip).unwrap();
        fs::write(chip.join("name"), "nct6687").unwrap();
        for i in 1..=32 {
            fs::write(chip.join(format!("temp{i}_input")), "50000").unwrap();
        }
        for i in 1..=8 {
            for (name, value) in [
                (format!("fan{i}_input"), "1200"),
                (format!("pwm{i}"), "100"),
                (format!("pwm{i}_enable"), "2"),
            ] {
                fs::write(chip.join(name), value).unwrap();
            }
        }
        let mut h = Real::with_root(t.0.clone(), None);
        h.gpu_retry = Instant::now() + Duration::from_secs(3600);
        h.fallback_at = Some(Instant::now() + Duration::from_secs(3600));
        let mut snapshot = h.sample(true).unwrap();
        crate::perf_tests::measure("hwmon_32_temperatures_8_fans_8_channels", 2000, || {
            h.sample_into(false, &mut snapshot).unwrap();
            std::hint::black_box(&snapshot);
        });
    }
}
