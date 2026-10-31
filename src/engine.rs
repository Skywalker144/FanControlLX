mod api;
mod configuration;
mod faults;
mod policy;
mod recovery;
use crate::{
    common::*,
    config,
    hardware::{Hardware, Snapshot},
    telemetry::Inputs,
};
use faults::{Fault, Faults};
use policy::{calculate, source_value, Policy};
#[cfg(test)]
pub use policy::{compute, interpolate};
pub use policy::{source_values, Memory, Output};
use serde_core::{ser::SerializeMap, Serialize, Serializer};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

struct Identification {
    channel: String,
    lease: Value,
    resume: Option<String>,
    stopping: bool,
    resume_requested: bool,
    duty: f64,
    effective: f64,
    fault: Option<String>,
    expires: f64,
}
// Compact numeric history is copied only when a reader requests a new sample.
#[derive(Clone)]
pub(crate) struct HistorySample {
    pub time: f64,
    cpu: Option<f64>,
    gpu: Option<f64>,
}
impl Serialize for HistorySample {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut m = serializer.serialize_map(Some(3))?;
        m.serialize_entry("time", &self.time)?;
        m.serialize_entry("cpu", &self.cpu)?;
        m.serialize_entry("gpu", &self.gpu)?;
        m.end()
    }
}
pub struct Engine {
    pub hardware: Box<dyn Hardware>,
    pub config: Arc<Value>,
    policies: Vec<Policy>,
    inputs: Inputs,
    pub snapshot: Snapshot,
    pub outputs: HashMap<String, Output>,
    pub enabled: HashSet<String>,
    pub leases: HashMap<String, Value>,
    pub memory: HashMap<String, Memory>,
    pub pending: HashMap<String, (String, f64)>,
    faults: Faults,
    pub last_sample: Instant,
    pub cycle_ms: u64,
    pub demo: bool,
    pub managed: bool,
    pub allow: bool,
    pub stopping: bool,
    path: PathBuf,
    start: Instant,
    identification: Option<Identification>,
    history: VecDeque<HistorySample>,
    events: VecDeque<Value>,
    revision: String,
    view_session: String,
    view_revision: String,
    history_cursor: u64,
    disarmed: Vec<Value>,
    discovery: Discovery,
}
/// A short editor lease expires even if the tab crashes or disappears.
#[derive(Default)]
pub(crate) struct Discovery {
    pub(crate) editor_until: f64,
    pub(crate) last: f64,
}
impl Discovery {
    pub(crate) fn due(&mut self, now: f64, recovery: bool) -> bool {
        if now - self.last >= 30. && (now < self.editor_until || recovery) {
            self.last = now;
            true
        } else {
            false
        }
    }
}
impl Engine {
    pub fn new(
        hardware: Box<dyn Hardware>,
        path: PathBuf,
        allow: bool,
        demo: bool,
        managed: bool,
    ) -> Result<Self> {
        let config = if path.exists() {
            config::validate(&serde_json::from_slice(&fs::read(&path)?)?)?
        } else {
            config::default_config()
        };
        let mut e = Self {
            hardware,
            policies: Policy::all(&config),
            inputs: Inputs::default(),
            config: Arc::new(config),
            snapshot: Snapshot::default(),
            outputs: HashMap::new(),
            enabled: HashSet::new(),
            leases: HashMap::new(),
            memory: HashMap::new(),
            pending: HashMap::new(),
            faults: Faults::default(),
            last_sample: Instant::now(),
            cycle_ms: 0,
            demo,
            managed,
            allow: allow || demo,
            stopping: false,
            path,
            start: Instant::now(),
            identification: None,
            history: VecDeque::with_capacity(180),
            events: VecDeque::new(),
            revision: String::new(),
            view_session: token()?,
            view_revision: String::new(),
            history_cursor: 0,
            disarmed: vec![],
            discovery: Discovery::default(),
        };
        e.scan(false)?;
        e.queue();
        Ok(e)
    }
    pub fn healthy(&self) -> bool {
        self.faults.is_empty()
    }
    pub fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
    fn event(&mut self, message: impl Into<String>) {
        let m = message.into();
        eprintln!("{m}");
        self.events.push_front(json!({"time":wall(),"message":m}));
        self.events.truncate(60);
    }
    fn commit(&mut self, v: Value, persist: bool) -> Result<()> {
        if persist {
            config::save(&self.path, &v)?;
        }
        self.revision = hash(&serde_json::to_vec(&v)?);
        self.view_revision = format!("{}-{}", self.view_session, self.revision);
        self.outputs
            .retain(|id, _| array(&v["fans"]).iter().any(|f| text(&f["id"]) == id));
        self.policies = Policy::all(&v);
        self.config = Arc::new(v);
        Ok(())
    }
    fn queue(&mut self) {
        if self.managed {
            self.pending = array(&self.config["fans"])
                .iter()
                .filter(|f| {
                    yes(&f["autostart"])
                        && yes(&f["identified"])
                        && !text(&f["channel_id"]).is_empty()
                        && !self.enabled.contains(text(&f["id"]))
                })
                .map(|f| (text(&f["id"]).into(), ("等待后台接管".into(), 0.)))
                .collect();
        }
    }
    pub fn resume(&mut self) {
        if !self.managed || !self.allow || self.stopping || self.identification.is_some() {
            return;
        }
        let ids: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| self.now() >= p.1)
            .map(|(k, _)| k.clone())
            .collect();
        for id in ids {
            match self.activate(&id) {
                Ok(()) => {
                    self.pending.remove(&id);
                }
                Err(e) => {
                    if self.pending.get(&id).is_none_or(|p| p.0 != e.1) {
                        self.event(format!("后台接管等待：{e}"));
                    }
                    self.pending.insert(id, (e.1, self.now() + 10.));
                }
            }
        }
    }
    pub fn tick(&mut self) -> Result<()> {
        // Reconnect configured devices even with every browser closed. Healthy
        // background operation never enumerates topology on a periodic timer.
        let recovery = array(&self.config["fans"])
            .iter()
            .zip(&self.policies)
            .any(|(f, p)| {
                !text(&f["channel_id"]).is_empty()
                    && (self
                        .inputs
                        .channels
                        .get(&self.snapshot.channels, text(&f["channel_id"]))
                        .is_none()
                        || ((yes(&f["autostart"]) || self.enabled.contains(text(&f["id"])))
                            && p.values(&self.snapshot, &self.inputs).is_none()))
            });
        let now = self.now();
        let force = self.discovery.due(now, recovery);
        let mut sample = std::mem::take(&mut self.snapshot);
        if let Err(error) = self.hardware.sample_into(force, &mut sample) {
            self.inputs.refresh(&sample);
            self.snapshot = sample;
            self.faults.set(Fault::Sampling, error.1.clone());
            return Err(error);
        }
        self.faults.clear(&Fault::Sampling);
        self.tick_with(sample, self.now())
    }
    pub fn tick_with(&mut self, s: Snapshot, now: f64) -> Result<()> {
        let sampled_at = Instant::now();
        let result = self.apply_sample(&s, now);
        // Even a failed control write must retain the fresh readings and the
        // reusable sampling storage for recovery and subsequent API requests.
        self.snapshot = s;
        if result.is_ok() {
            self.last_sample = sampled_at;
        }
        result
    }
    fn apply_sample(&mut self, s: &Snapshot, now: f64) -> Result<()> {
        self.inputs.refresh(s);
        if self
            .identification
            .as_ref()
            .is_some_and(|t| t.stopping || now >= t.expires)
        {
            self.stop_identification(None, true)?;
        }
        let inactive: Vec<_> = self
            .leases
            .keys()
            .filter(|id| {
                !self.enabled.contains(*id)
                    && !self
                        .identification
                        .as_ref()
                        .is_some_and(|t| t.resume.as_ref() == Some(*id))
            })
            .cloned()
            .collect();
        for id in inactive {
            let _ = self.release(&id, false);
        }
        if let Some(t) = self.identification.as_mut() {
            let channel = s.channels.iter().find(|c| text(&c["id"]) == t.channel);
            let outcome = (|| {
                let c = channel
                    .filter(|c| yes(&c["writable"]))
                    .ok_or_else(|| conflict("识别通道离线或不可写"))?;
                let (duty, fault) = Self::identification_duty(t.duty, &s.sensors, Some(c));
                self.hardware.write(&t.channel, duty)?;
                t.effective = duty;
                t.fault = fault;
                Ok(())
            })();
            if let Err(e) = outcome {
                self.stop_identification(None, true)?;
                return Err(e);
            }
        }
        if !self.disarmed.is_empty() {
            self.maybe_disarm()?;
        }
        // End each configuration borrow before running hardware recovery.
        for index in 0..array(&self.config["fans"]).len() {
            let f = &self.config["fans"][index];
            let id = text(&f["id"]);
            let active = self.enabled.contains(id);
            if !self.memory.contains_key(id) {
                self.memory.insert(
                    id.to_owned(),
                    Memory {
                        last: now,
                        changed: now,
                        ..Memory::default()
                    },
                );
            }
            let m = self.memory.get_mut(id).unwrap();
            let o = calculate(f, &self.policies[index], s, &self.inputs, m, active, now);
            let id = text(&self.config["fans"][index]["id"]);
            if let Some(output) = self.outputs.get_mut(id) {
                *output = o;
            } else {
                self.outputs.insert(id.to_owned(), o);
            }
        }
        let config = self.config.clone();
        let active: Vec<_> = array(&config["fans"])
            .iter()
            .filter(|f| self.enabled.contains(text(&f["id"])))
            .collect();
        let mut writes = Vec::with_capacity(active.len());
        for f in &active {
            let key = text(&f["channel_id"]);
            if self
                .inputs
                .channels
                .get(&s.channels, key)
                .is_some_and(|c| yes(&c["writable"]))
            {
                writes.push((key, self.memory[text(&f["id"])].duty.unwrap()));
            }
        }
        let mut results = self.hardware.write_batch(&writes).into_iter();
        for f in active {
            let key = text(&f["channel_id"]);
            let result = if self
                .inputs
                .channels
                .get(&s.channels, key)
                .is_some_and(|c| yes(&c["writable"]))
            {
                results
                    .next()
                    .unwrap_or_else(|| Err(conflict("硬件批量响应不完整")))
            } else {
                Err(conflict("控制通道离线或不可写"))
            };
            let id = text(&f["id"]);
            match result {
                Ok(r) => {
                    let o = self.outputs.get_mut(id).unwrap();
                    if r["fault"].is_string() {
                        o.automatic = false;
                        o.target = num(&r["duty"]);
                        o.requested = num(&r["duty"]);
                        self.memory.get_mut(id).unwrap().duty = r["duty"].as_f64();
                        if o.fault.is_none() {
                            o.fault = Some(text(&r["fault"]).to_owned().into());
                        }
                    }
                    o.status = if o.fault.is_some() {
                        "保护运行"
                    } else if o.automatic {
                        "显卡自动调速"
                    } else {
                        "正在控制"
                    };
                }
                Err(e) => {
                    let _ = self.release(id, true);
                    if self.managed && yes(&f["autostart"]) {
                        self.pending.insert(id.into(), (e.1.clone(), now + 10.));
                    }
                    let o = self.outputs.get_mut(id).unwrap();
                    o.status = "控制失败";
                    o.fault = Some(e.1.clone().into());
                    o.active = false;
                    self.event(format!("{}：{e}", text(&f["name"])));
                }
            }
        }
        self.history_cursor += 1;
        if self.history.len() == 180 {
            self.history.pop_front();
        }
        self.history.push_back(HistorySample {
            time: wall(),
            cpu: Self::group(&s.sensors, "cpu"),
            gpu: Self::group(&s.sensors, "gpu"),
        });
        Ok(())
    }
    pub fn cycle(&mut self) {
        let started = Instant::now();
        if let Err(e) = self.tick() {
            self.event(format!("控制周期失败：{e}"));
            let _ = self.release_all();
            self.queue();
        }
        self.resume();
        self.cycle_ms = started.elapsed().as_millis() as u64;
    }
    fn group(sensors: &[Value], kind: &str) -> Option<f64> {
        sensors
            .iter()
            .filter(|s| text(&s["kind"]) == kind)
            .filter_map(|s| s["value"].as_f64())
            .reduce(f64::max)
    }
    pub fn read_snapshot(&self, editor: bool) -> crate::state::ReadSnapshot {
        crate::state::ReadSnapshot {
            config: self.config.clone(),
            outputs: self.outputs.clone(),
            pending: self.pending.iter()
                .map(|(id, p)| (id.clone(), p.0.clone())).collect(),
            history: self.history.iter()
                .skip(if editor { 0 } else { self.history.len().saturating_sub(80) })
                .cloned().collect(),
            editor: editor.then(|| {
                let (gpu_error, gpu_control_error) = self.hardware.errors();
                crate::state::EditorSnapshot {
                    hardware: self.snapshot.clone(),
                    events: self.events.clone(),
                    allow: self.allow,
                    gpu_error,
                    gpu_control_error,
                    identification: self.identification.as_ref().map(|t| json!({
                        "channel_id":t.channel,"duty":t.duty,"effective_duty":t.effective,"fault":t.fault,
                        "remaining_s":(t.expires-self.now()).max(0.).ceil(),"stopping":t.stopping
                    })),
                }
            }),
            demo: self.demo,
            managed: self.managed,
            error: self.faults.message(),
            revision: self.revision.clone(),
            view_revision: self.view_revision.clone(),
            history_cursor: self.history_cursor,
            cpu: Self::group(&self.snapshot.sensors, "cpu"),
            gpu: Self::group(&self.snapshot.sensors, "gpu"),
        }
    }
    #[cfg(test)]
    pub fn state(&self, since: Option<&str>, after: Option<f64>) -> Value {
        serde_json::to_value(
            self.read_snapshot(true)
                .response(crate::state::Query::Editor { since, after }, None),
        )
        .unwrap()
    }
    #[cfg(test)]
    pub fn view_state(&self, since: Option<&str>, after: Option<u64>) -> Value {
        serde_json::to_value(
            self.read_snapshot(false)
                .response(crate::state::Query::Viewer { since, after }, None),
        )
        .unwrap()
    }
    pub fn discovery(&self) -> Vec<Value> {
        self.snapshot.channels.iter().map(|c|{
            let f=array(&self.config["fans"]).iter().find(|f|f["channel_id"]==c["id"]);
            let rpm_id=f.filter(|f|!text(&f["rpm_sensor_id"]).is_empty()).map(|f|f["rpm_sensor_id"].clone()).unwrap_or_else(||c["rpm_sensor_id"].clone());
            let rpm=self.snapshot.fans.iter().find(|s|s["id"]==rpm_id).map(|s|s["rpm"].clone()).unwrap_or(Value::Null);
            let mut result=c.clone();let obj=result.as_object_mut().unwrap();
            obj.extend(json!({"rpm_sensor_id":rpm_id,"rpm":rpm,"connection":if num(&rpm)>0.{"rotating"}else if yes(&c["present"]){"present"}else{"unconfirmed"},
                "fan_id":f.map(|f|&f["id"]),"display_name":f.map(|f|&f["name"]).unwrap_or(&c["name"]),"identified":f.is_some_and(|f|yes(&f["identified"])),"device_kind":f.map(|f|&f["device_kind"]).unwrap_or(&json!("unknown"))}).as_object_mut().map(std::mem::take).unwrap());
            result
        }).collect()
    }
    fn discovered(c: &Value) -> Value {
        let id = format!("fan-{}", &hash(text(&c["id"]).as_bytes())[..16]);
        let mut f = config::default_fan(
            &id,
            text(&c["name"]),
            c["temperature_sensor_id"].as_str().unwrap_or("@cpu"),
        );
        f["channel_id"] = c["id"].clone();
        f["rpm_sensor_id"] = c["rpm_sensor_id"].clone();
        if c["backend"] == "nvidia" {
            f["min_duty"] = c["min_duty"].clone();
            f["max_duty"] = c["max_duty"].clone();
        }
        f
    }
    pub fn scan(&mut self, persist: bool) -> Result<usize> {
        let snapshot = match self.hardware.sample(true) {
            Ok(snapshot) => {
                self.faults.clear(&Fault::Sampling);
                snapshot
            }
            Err(e) => {
                self.faults.set(Fault::Sampling, e.1.clone());
                return Err(e);
            }
        };
        self.discovery.last = self.now();
        // Apply protection to the newly discovered readings before any disk I/O.
        self.tick_with(snapshot, self.now())?;
        let mut config = (*self.config).clone();
        let mut placeholders = vec![
            config::default_fan("cpu-fan", "CPU 散热风扇", "@cpu"),
            config::default_fan("case-fan", "机箱风扇", "@cpu-gpu"),
        ];
        for f in &mut placeholders {
            f["points"] = json!([[25, 30], [40, 35], [55, 50], [70, 75], [85, 100]]);
            f["min_duty"] = json!(30);
        }
        let fans = config["fans"].as_array_mut().unwrap();
        fans.retain(|f| !placeholders.contains(f));
        let mut added = 0;
        for c in self.discovery() {
            if ["rotating", "present"].contains(&text(&c["connection"]))
                && !fans.iter().any(|f| f["channel_id"] == c["id"])
                && fans.len() < 64
            {
                let f = Self::discovered(&c);
                if !fans.iter().any(|old| old["id"] == f["id"]) {
                    fans.push(f);
                    added += 1;
                }
            }
        }
        self.commit(config::validate(&config)?, persist)?;
        // Reconcile newly added configurations using the same fresh scan.
        self.tick_with(self.snapshot.clone(), self.now())?;
        if persist {
            self.event(format!("扫描完成，新增 {added} 个在线设备；未启用新的控制"));
        }
        Ok(added)
    }
}
