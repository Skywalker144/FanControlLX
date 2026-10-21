use crate::{
    common::*,
    config,
    hardware::{Hardware, Snapshot},
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::PathBuf,
    time::Instant,
};

#[derive(Default)]
pub struct Memory {
    pub samples: VecDeque<(f64, f64)>,
    pub last: f64,
    pub changed: f64,
    pub temp: Option<f64>,
    pub duty: Option<f64>,
    pub active_since: Option<f64>,
}
pub fn interpolate(points: &Value, t: f64) -> f64 {
    let p = array(points);
    if t <= num(&p[0][0]) {
        return num(&p[0][1]);
    }
    for pair in p.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if t <= num(&b[0]) {
            return num(&a[1])
                + (num(&b[1]) - num(&a[1])) * (t - num(&a[0])) / (num(&b[0]) - num(&a[0]));
        }
    }
    num(&p.last().unwrap()[1])
}
pub fn source_values(ids: &Value, sensors: &[Value]) -> Option<Vec<f64>> {
    let mut result = vec![];
    for id in array(ids) {
        let kinds: &[&str] = match text(id) {
            "@cpu" => &["cpu"],
            "@gpu" => &["gpu"],
            "@cpu-gpu" => &["cpu", "gpu"],
            _ => &[],
        };
        if kinds.is_empty() {
            result.push(sensors.iter().find(|s| s["id"] == *id)?["value"].as_f64()?);
        } else {
            for kind in kinds {
                let values: Option<Vec<_>> = sensors
                    .iter()
                    .filter(|s| text(&s["kind"]) == *kind)
                    .map(|s| s["value"].as_f64())
                    .collect();
                let values = values?;
                if values.is_empty() {
                    return None;
                }
                result.push(values.into_iter().fold(f64::NEG_INFINITY, f64::max));
            }
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}
pub fn compute(
    f: &Value,
    curves: &Value,
    s: &Snapshot,
    m: &mut Memory,
    active: bool,
    now: f64,
) -> Value {
    let c = s.channels.iter().find(|c| c["id"] == f["channel_id"]);
    let values = source_values(&f["sensor_ids"], &s.sensors);
    let raw = values.as_ref().map(|v| match text(&f["aggregation"]) {
        "average" => v.iter().sum::<f64>() / v.len() as f64,
        "min" => v.iter().copied().fold(f64::INFINITY, f64::min),
        _ => v.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    });
    if let Some(raw) = raw {
        m.samples.push_back((now, raw));
    }
    while m
        .samples
        .front()
        .is_some_and(|p| p.0 < now - num(&f["smoothing_s"]))
    {
        m.samples.pop_front();
    }
    let smoothed = raw.map(|raw| {
        if m.samples.is_empty() {
            raw
        } else {
            m.samples.iter().map(|p| p.1).sum::<f64>() / m.samples.len() as f64
        }
    });
    let mut fault: Option<String> = None;
    let mut automatic = false;
    let mut requested = None;
    let mut duty = if values.is_none() {
        fault = Some("温度源不可用，保护输出 100%".into());
        100.
    } else if values
        .as_ref()
        .is_some_and(|v| v.iter().any(|v| *v >= num(&f["critical_temp"])))
    {
        fault = Some("达到保护温度，输出 100%".into());
        100.
    } else {
        let mut effective = smoothed.unwrap();
        if let Some(temp) = m
            .temp
            .filter(|t| (effective - t).abs() < num(&f["hysteresis"]))
        {
            effective = temp;
        } else {
            m.temp = Some(effective);
        }
        let points = array(curves)
            .iter()
            .find(|c| c["id"] == f["curve_id"])
            .map(|c| &c["points"])
            .unwrap_or(&f["points"]);
        let mut d = if f["mode"] == "manual" {
            num(&f["manual_duty"])
        } else {
            interpolate(points, effective)
        };
        automatic =
            c.is_some_and(|c| yes(&c["curve_zero_auto"])) && f["mode"] == "curve" && d == 0.;
        if !automatic {
            d = d.clamp(num(&f["min_duty"]), num(&f["max_duty"]));
        }
        requested = Some(d);
        if automatic {
            if m.duty != Some(0.) {
                m.changed = now;
            }
        } else if let Some(mut previous) = m.duty {
            if c.is_some_and(|c| yes(&c["curve_zero_auto"])) && previous == 0. {
                previous = num(&f["min_duty"]).max(num(&c.unwrap()["min_duty"]));
            }
            if f["mode"] != "manual" && now - m.changed < num(&f["response_s"]) {
                d = previous;
            }
            let dt = (now - m.last).clamp(0., 5.);
            d = d.clamp(
                previous - num(&f["step_down"]) * dt,
                previous + num(&f["step_up"]) * dt,
            );
            if (d - previous).abs() >= 0.1 {
                m.changed = now;
            }
        }
        d
    };
    if let Some(c) = c.filter(|c| c["temperature_sensor_id"].is_string()) {
        let own = source_values(&json!([c["temperature_sensor_id"]]), &s.sensors);
        if own.is_none() {
            duty = 100.;
            fault = Some("显卡自身温度不可用，保护输出 100%".into());
        } else if own.is_some_and(|v| v.iter().any(|t| *t >= 85f64.min(num(&f["critical_temp"])))) {
            duty = 100.;
            fault = Some("显卡达到保护温度，输出 100%".into());
        }
    }
    let tach = s.fans.iter().find(|v| v["id"] == f["rpm_sensor_id"]);
    let rpm = tach.and_then(|v| v["rpm"].as_f64());
    if active
        && duty > 0.
        && now - m.active_since.unwrap_or(now) > 10.
        && !text(&f["rpm_sensor_id"]).is_empty()
        && tach.is_none_or(|v| v["rpm_is_feedback"] != false)
        && rpm.is_none_or(|v| v <= 0.)
    {
        duty = 100.;
        fault = Some("测速信号丢失或风扇停转，保护输出 100%".into());
        requested = Some(100.);
    }
    if fault.is_some() {
        automatic = false;
    }
    if let Some(c) = c.filter(|_| !automatic) {
        let lo = c["min_duty"].as_f64().unwrap_or(0.);
        let hi = c["max_duty"].as_f64().unwrap_or(100.);
        duty = duty.clamp(lo, hi);
        requested = requested.map(|v| v.clamp(lo, hi));
    }
    if fault.is_some() {
        requested = Some(duty);
    }
    m.last = now;
    m.duty = Some(duty);
    json!({"temperature":raw.map(rounded),"smoothed":smoothed.map(rounded),"target":rounded(duty),"rpm":rpm,
        "status":if f["channel_id"].is_null(){"待绑定通道"}else if c.is_none(){"通道离线"}else{"未接管"},
        "requested":rounded(requested.unwrap_or(duty)),"actual":c.map(|c|&c["duty"]),"automatic":automatic,"active":active,"fault":fault})
}
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
pub struct Engine {
    pub hardware: Box<dyn Hardware>,
    pub config: Value,
    pub snapshot: Snapshot,
    pub outputs: Value,
    pub enabled: HashSet<String>,
    pub leases: HashMap<String, Value>,
    pub memory: HashMap<String, Memory>,
    pub pending: HashMap<String, (String, f64)>,
    pub error: Option<String>,
    pub demo: bool,
    pub managed: bool,
    pub allow: bool,
    pub stopping: bool,
    path: PathBuf,
    start: Instant,
    identification: Option<Identification>,
    history: VecDeque<Value>,
    events: VecDeque<Value>,
    revision: String,
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
            config,
            snapshot: Snapshot::default(),
            outputs: json!({}),
            enabled: HashSet::new(),
            leases: HashMap::new(),
            memory: HashMap::new(),
            pending: HashMap::new(),
            error: None,
            demo,
            managed,
            allow: allow || demo,
            stopping: false,
            path,
            start: Instant::now(),
            identification: None,
            history: VecDeque::new(),
            events: VecDeque::new(),
            revision: String::new(),
            disarmed: vec![],
            discovery: Discovery::default(),
        };
        e.scan(false)?;
        e.queue();
        Ok(e)
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
        self.config = v;
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
        let recovery = array(&self.config["fans"]).iter().any(|f| {
            !text(&f["channel_id"]).is_empty()
                && (!self
                    .snapshot
                    .channels
                    .iter()
                    .any(|c| c["id"] == f["channel_id"])
                    || ((yes(&f["autostart"]) || self.enabled.contains(text(&f["id"])))
                        && source_values(&f["sensor_ids"], &self.snapshot.sensors).is_none()))
        });
        let now = self.now();
        let force = self.discovery.due(now, recovery);
        let sample = self.hardware.sample(force)?;
        self.tick_with(sample, self.now())
    }
    pub fn tick_with(&mut self, s: Snapshot, now: f64) -> Result<()> {
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
        let mut outputs = json!({});
        // Indexing gives each fan a short borrow. Only failure metadata is
        // copied before recovery; the full configuration is never cloned here.
        for index in 0..array(&self.config["fans"]).len() {
            let f = &self.config["fans"][index];
            let id = text(&f["id"]).to_owned();
            let active = self.enabled.contains(&id);
            let m = self.memory.entry(id.clone()).or_insert_with(|| Memory {
                last: now,
                changed: now,
                ..Memory::default()
            });
            let mut o = compute(f, &self.config["curves"], &s, m, active, now);
            if active {
                let key = text(&f["channel_id"]);
                let result = if s
                    .channels
                    .iter()
                    .any(|c| text(&c["id"]) == key && yes(&c["writable"]))
                {
                    self.hardware.write(key, m.duty.unwrap())
                } else {
                    Err(conflict("控制通道离线或不可写"))
                };
                match result {
                    Ok(r) => {
                        if r["fault"].is_string() {
                            o["automatic"] = json!(false);
                            o["target"] = r["duty"].clone();
                            o["requested"] = r["duty"].clone();
                            m.duty = r["duty"].as_f64();
                            if o["fault"].is_null() {
                                o["fault"] = r["fault"].clone();
                            }
                        }
                        o["status"] = json!(if !o["fault"].is_null() {
                            "保护运行"
                        } else if yes(&o["automatic"]) {
                            "显卡自动调速"
                        } else {
                            "正在控制"
                        });
                    }
                    Err(e) => {
                        let autostart = yes(&f["autostart"]);
                        let name = text(&f["name"]).to_owned();
                        let _ = self.release(&id, true);
                        if self.managed && autostart {
                            self.pending.insert(id.clone(), (e.1.clone(), now + 10.));
                        }
                        o["status"] = json!("控制失败");
                        o["fault"] = json!(e.1);
                        o["active"] = json!(false);
                        self.event(format!("{name}：{e}"));
                    }
                }
            }
            outputs[id] = o;
        }
        self.outputs = outputs;
        self.history.push_back(json!({"time":wall(),"cpu":Self::group(&s.sensors,"cpu"),"gpu":Self::group(&s.sensors,"gpu")}));
        if self.history.len() > 180 {
            self.history.pop_front();
        }
        self.snapshot = s;
        Ok(())
    }
    pub fn cycle(&mut self) {
        if let Err(e) = self.tick() {
            self.error = Some(e.1.clone());
            self.event(format!("控制周期失败：{e}"));
            let _ = self.release_all();
            self.queue();
        }
        self.resume();
    }
    fn group(sensors: &[Value], kind: &str) -> Option<f64> {
        sensors
            .iter()
            .filter(|s| text(&s["kind"]) == kind)
            .filter_map(|s| s["value"].as_f64())
            .reduce(f64::max)
    }
    pub fn state(&self, since: Option<&str>, after: Option<f64>) -> Value {
        let (gpu_error, gpu_control_error) = self.hardware.errors();
        let identification=self.identification.as_ref().map(|t|json!({"channel_id":t.channel,"duty":t.duty,"effective_duty":t.effective,"fault":t.fault,"remaining_s":(t.expires-self.now()).max(0.).ceil(),"stopping":t.stopping}));
        let mut v = json!({"sensors":self.snapshot.sensors,"fans":self.snapshot.fans,"channels":self.snapshot.channels,"chips":self.snapshot.chips,"outputs":self.outputs,
            "history":self.history.iter().filter(|h|after.is_none_or(|t|num(&h["time"])>t)).collect::<Vec<_>>(),"history_delta":after.is_some(),
            "events":self.events,"demo":self.demo,"control_allowed":self.allow,"error":self.error,"gpu_error":gpu_error,"gpu_control_error":gpu_control_error,
            "time":wall(),"discovery":self.discovery(),"identification":identification,"background_service":self.managed,"config_revision":self.revision,"workspace_api":2,
            "pending_autostart":self.pending.iter().map(|(id,p)|(id.clone(),json!(p.0))).collect::<serde_json::Map<_,_>>(),"runtime":"rust"});
        if since != Some(&self.revision) {
            v["config"] = self.config.clone();
        }
        v
    }
    pub fn view_state(&self) -> Value {
        // Only expose cached readings and the resolved curve for display; omit
        // full configuration, tokens, discovery metadata, logs and history.
        json!({"time":wall(),"sample_time":self.history.back().map(|h|&h["time"]),
            "runtime":"rust","demo":self.demo,"background_service":self.managed,"error":self.error,
            "cpu":Self::group(&self.snapshot.sensors,"cpu"),"gpu":Self::group(&self.snapshot.sensors,"gpu"),
            "fans":array(&self.config["fans"]).iter().map(|f|{
                let curve = array(&self.config["curves"]).iter().find(|c| c["id"] == f["curve_id"]);
                json!({
                    "id":f["id"],"name":f["name"],"mode":f["mode"],
                    "points":curve.map(|c|&c["points"]).unwrap_or(&f["points"]),
                    "curve_name":curve.map(|c|&c["name"]),
                    "output":self.outputs[text(&f["id"])],
                    "pending":self.pending.get(text(&f["id"])).map(|p|&p.0)
                })
            }).collect::<Vec<_>>()})
    }
    pub fn discovery(&self) -> Vec<Value> {
        self.snapshot.channels.iter().map(|c|{
            let f=array(&self.config["fans"]).iter().find(|f|f["channel_id"]==c["id"]);
            let rpm_id=f.filter(|f|!text(&f["rpm_sensor_id"]).is_empty()).map(|f|f["rpm_sensor_id"].clone()).unwrap_or_else(||c["rpm_sensor_id"].clone());
            let rpm=self.snapshot.fans.iter().find(|s|s["id"]==rpm_id).map(|s|s["rpm"].clone()).unwrap_or(Value::Null);
            let mut result=c.clone();let obj=result.as_object_mut().unwrap();
            obj.extend(json!({"rpm_sensor_id":rpm_id,"rpm":rpm,"connection":if num(&rpm)>0.{"rotating"}else if yes(&c["present"]){"present"}else{"unconfirmed"},
                "fan_id":f.map(|f|&f["id"]),"display_name":f.map(|f|&f["name"]).unwrap_or(&c["name"]),"identified":f.is_some_and(|f|yes(&f["identified"])),"device_kind":f.map(|f|&f["device_kind"]).unwrap_or(&json!("unknown"))}).as_object().unwrap().clone());
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
        let snapshot = self.hardware.sample(true)?;
        self.discovery.last = self.now();
        // Apply protection to the newly discovered readings before any disk I/O.
        self.tick_with(snapshot, self.now())?;
        let mut config = self.config.clone();
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
        // Reconcile newly added configurations using the same fresh scan; do
        // not read the hardware three times for one discovery operation.
        self.tick_with(self.snapshot.clone(), self.now())?;
        if persist {
            self.event(format!("扫描完成，新增 {added} 个在线设备；未启用新的控制"));
        }
        Ok(added)
    }
    fn identification_duty(duty: f64, s: &[Value], c: Option<&Value>) -> (f64, Option<String>) {
        if let Some(c) = c.filter(|c| c["temperature_sensor_id"].is_string()) {
            if source_values(&json!([c["temperature_sensor_id"]]), s)
                .is_none_or(|v| v.iter().any(|t| *t >= 85.))
            {
                return (100., Some("显卡温度异常，识别测试保持 100%".into()));
            }
        } else if source_values(&json!(["@cpu"]), s).is_none()
            || s.iter()
                .filter(|s| s["kind"] == "cpu" || s["kind"] == "gpu")
                .any(|s| s["value"].as_f64().is_some_and(|t| t >= 85.))
        {
            return (100., Some("温度异常，识别测试保持 100%".into()));
        }
        (duty, None)
    }
    fn identify(&mut self, key: &str, duty: f64, kind: &str) -> Result<()> {
        if !(50.0..=100.).contains(&duty) || !["unknown", "fan", "pump"].contains(&kind) {
            return Err(invalid("识别输出或设备类型无效"));
        }
        if !self.allow {
            return Err(invalid("识别测试需要控制权限"));
        }
        self.tick()?;
        let c = self
            .discovery()
            .into_iter()
            .find(|c| text(&c["id"]) == key)
            .ok_or_else(|| invalid("识别通道不存在或已离线"))?;
        if !yes(&c["writable"]) {
            return Err(invalid(text(&c["reason"])));
        }
        if (kind == "pump" || (text(&c["name"]).to_lowercase().contains("pump") && kind != "fan"))
            && duty != 100.
        {
            return Err(invalid("水泵或未确认的 Pump 接口只允许 100% 识别"));
        }
        if self
            .identification
            .as_ref()
            .is_some_and(|t| t.channel != key)
        {
            self.stop_identification(None, true)?;
        }
        let (effective, fault) = Self::identification_duty(duty, &self.snapshot.sensors, Some(&c));
        if self.identification.is_none() {
            let active = array(&self.config["fans"])
                .iter()
                .find(|f| text(&f["channel_id"]) == key && self.enabled.contains(text(&f["id"])))
                .map(|f| text(&f["id"]).to_owned());
            let lease = if let Some(id) = &active {
                self.hardware.write(key, effective)?;
                self.enabled.remove(id);
                self.leases[id].clone()
            } else {
                self.hardware.acquire(key, effective)?
            };
            self.identification = Some(Identification {
                channel: key.into(),
                lease,
                resume: active,
                stopping: false,
                resume_requested: true,
                duty,
                effective,
                fault,
                expires: self.now() + 15.,
            });
            self.event(format!("开始识别 {}；15 秒无操作后恢复", text(&c["name"])));
        } else {
            if let Err(e) = self.hardware.write(key, effective) {
                self.stop_identification(None, true)?;
                return Err(e);
            }
            let expires = self.now() + 15.;
            let t = self.identification.as_mut().unwrap();
            t.duty = duty;
            t.effective = effective;
            t.fault = fault;
            t.expires = expires;
        }
        Ok(())
    }
    pub fn stop_identification(&mut self, key: Option<&str>, mut resume: bool) -> Result<()> {
        let Some(t) = self.identification.as_mut() else {
            return Ok(());
        };
        if key.is_some_and(|k| k != t.channel) {
            return Ok(());
        }
        if t.stopping {
            resume &= t.resume_requested;
        }
        t.resume_requested = resume;
        t.stopping = true;
        if let Some(id) = t.resume.as_ref().filter(|_| resume && !self.stopping) {
            self.enabled.insert(id.clone());
            self.memory.entry(id.clone()).or_default().duty = Some(t.effective);
            self.identification = None;
            self.event("识别测试结束，已恢复此前的控制模式");
            return Ok(());
        }
        if let Err(e) = self.hardware.restore(&t.lease) {
            self.error = Some(format!("识别结束后恢复失败，正在重试：{e}"));
            return Err(e);
        }
        let t = self.identification.take().unwrap();
        if let Some(id) = t.resume {
            self.leases.remove(&id);
        }
        self.disarmed.push(t.lease);
        self.maybe_disarm()?;
        self.event("识别测试结束，已恢复接管前状态");
        Ok(())
    }
    fn maybe_disarm(&mut self) -> Result<()> {
        if self.leases.is_empty() && self.identification.is_none() {
            self.hardware.disarm(&self.disarmed)?;
            self.disarmed.clear();
        }
        Ok(())
    }
    pub fn activate(&mut self, id: &str) -> Result<()> {
        if self.stopping {
            return Err(conflict("服务正在停止"));
        }
        if !self.allow {
            return Err(invalid("当前为监控模式"));
        }
        let f = array(&self.config["fans"])
            .iter()
            .find(|f| text(&f["id"]) == id)
            .cloned()
            .ok_or_else(|| invalid("风扇不存在"))?;
        if !yes(&f["identified"]) {
            return Err(invalid("请先确认设备类型并命名"));
        }
        if self.enabled.contains(id) {
            return Ok(());
        }
        if self.leases.contains_key(id) {
            return Err(conflict("上一次控制状态尚未恢复"));
        }
        self.stop_identification(None, true)?;
        self.tick()?;
        let c = self
            .snapshot
            .channels
            .iter()
            .find(|c| c["id"] == f["channel_id"])
            .ok_or_else(|| invalid("请先绑定在线控制通道"))?;
        if !yes(&c["writable"]) {
            return Err(conflict(text(&c["reason"])));
        }
        let o = &self.outputs[id];
        let low = num(&f["min_duty"]).max(num(&c["min_duty"]));
        let high = num(&f["max_duty"])
            .min(c["max_duty"].as_f64().unwrap_or(100.))
            .max(low);
        let initial = if yes(&o["automatic"]) {
            0.
        } else if !o["fault"].is_null() {
            100.
        } else {
            50f64.clamp(low, high)
        };
        let l = self.hardware.acquire(text(&f["channel_id"]), initial)?;
        self.leases.insert(id.into(), l);
        self.enabled.insert(id.into());
        let now = self.now();
        let m = self.memory.entry(id.into()).or_default();
        m.active_since = Some(now);
        m.duty = Some(initial);
        self.event(format!("已接管 {}", text(&f["name"])));
        self.tick()
    }
    pub fn release(&mut self, id: &str, cancel: bool) -> Result<()> {
        if cancel {
            self.pending.remove(id);
        }
        let Some(l) = self.leases.get(id).cloned() else {
            self.enabled.remove(id);
            return Ok(());
        };
        if let Err(e) = self.hardware.restore(&l) {
            self.enabled.remove(id);
            self.error = Some(format!("恢复控制状态失败：{e}"));
            return Err(e);
        }
        self.leases.remove(id);
        self.enabled.remove(id);
        self.disarmed.push(l);
        self.maybe_disarm()?;
        self.event(format!("已释放 {id}，恢复接管前状态"));
        Ok(())
    }
    pub fn release_all(&mut self) -> Result<()> {
        self.pending.clear();
        let mut errors = vec![];
        if let Err(e) = self.stop_identification(None, false) {
            errors.push(e.1);
        }
        for id in self.leases.keys().cloned().collect::<Vec<_>>() {
            if let Err(e) = self.release(&id, true) {
                errors.push(e.1);
            }
        }
        if errors.is_empty() {
            self.maybe_disarm()
        } else {
            Err(conflict(errors.join("；")))
        }
    }
    pub fn shutdown(&mut self) -> Result<()> {
        self.stopping = true;
        self.release_all()
    }
    pub fn update(&mut self, v: &Value, expected: Option<&Value>) -> Result<()> {
        if expected
            .filter(|e| !e.is_null())
            .is_some_and(|e| e != &self.revision)
        {
            return Err(conflict("配置已在另一页面更新，请重新打开后再操作"));
        }
        let mut v = config::validate(v)?;
        let previous = self.config.clone();
        for f in v["fans"].as_array_mut().unwrap() {
            if array(&previous["fans"])
                .iter()
                .any(|old| old["id"] == f["id"] && old["channel_id"] != f["channel_id"])
            {
                f["identified"] = json!(false);
                f["device_kind"] = json!("unknown");
                f["autostart"] = json!(false);
            }
        }
        config::save(&self.path, &v)?;
        self.stop_identification(None, true)?;
        let mut release_error = None;
        for id in self.leases.keys().cloned().collect::<Vec<_>>() {
            let wanted = array(&v["fans"]).iter().find(|f| {
                text(&f["id"]) == id
                    && yes(&f["identified"])
                    && !text(&f["channel_id"]).is_empty()
                    && if self.managed {
                        yes(&f["autostart"])
                    } else {
                        self.enabled.contains(&id)
                    }
            });
            let old = array(&previous["fans"])
                .iter()
                .find(|f| text(&f["id"]) == id);
            if wanted.is_none()
                || old.is_none()
                || wanted.unwrap()["channel_id"] != old.unwrap()["channel_id"]
            {
                if let Err(e) = self.release(&id, true) {
                    release_error = Some(e);
                }
            }
        }
        for f in array(&v["fans"]) {
            let old = array(&previous["fans"])
                .iter()
                .find(|old| old["id"] == f["id"]);
            if old.is_some_and(|old| {
                ["sensor_ids", "aggregation", "smoothing_s"]
                    .iter()
                    .any(|k| f[*k] != old[*k])
            }) {
                if let Some(m) = self.memory.get_mut(text(&f["id"])) {
                    m.samples.clear();
                    m.temp = None;
                }
            }
        }
        self.commit(v, false)?;
        self.memory.retain(|id, _| {
            array(&self.config["fans"])
                .iter()
                .any(|f| text(&f["id"]) == id)
        });
        self.tick()?;
        self.queue();
        self.resume();
        self.event("配置已保存并应用");
        if let Some(e) = release_error {
            return Err(e);
        }
        Ok(())
    }
    pub fn request(&mut self, path: &str, v: &Value) -> Result<Value> {
        if self.stopping {
            return Err(conflict("服务正在停止"));
        }
        if !v.is_object() {
            return Err(invalid("请求必须为 JSON 对象"));
        }
        match path {
            "/api/editor-heartbeat" => {
                self.discovery.editor_until = self.now() + 15.;
                return Ok(json!({"ok":true}));
            }
            "/api/config" => self.update(v, v.get("expected_revision"))?,
            "/api/fan" => {
                let changes = v["changes"]
                    .as_object()
                    .filter(|m| !m.is_empty())
                    .ok_or_else(|| invalid("风扇设置字段无效"))?;
                let defaults = config::default_fan("", "", "@cpu");
                if changes
                    .keys()
                    .any(|k| k == "id" || k == "identified" || defaults.get(k).is_none())
                {
                    return Err(invalid("风扇设置字段无效"));
                }
                let mut c = self.config.clone();
                let f = c["fans"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|f| f["id"] == v["id"])
                    .ok_or_else(|| invalid("风扇不存在"))?;
                Self::expected_fields(f, v.get("expected"))?;
                f.as_object_mut().unwrap().extend(changes.clone());
                self.update(&c, None)?;
            }
            "/api/fan-order" => {
                let current: Vec<_> = array(&self.config["fans"])
                    .iter()
                    .map(|f| f["id"].clone())
                    .collect();
                let ids = v["ids"]
                    .as_array()
                    .ok_or_else(|| invalid("排序必须包含所有风扇"))?;
                let unique: HashSet<_> = ids.iter().map(text).collect();
                if ids.len() != current.len()
                    || unique.len() != current.len()
                    || ids
                        .iter()
                        .any(|id| !id.is_string() || !current.contains(id))
                {
                    return Err(invalid("排序必须包含当前所有风扇，且不能重复"));
                }
                if v.get("expected")
                    .filter(|e| !e.is_null())
                    .is_some_and(|e| *e != json!(current))
                {
                    return Err(conflict("风扇顺序已更新，请刷新后重试"));
                }
                let mut c = self.config.clone();
                c["fans"] = json!(ids
                    .iter()
                    .map(|id| array(&self.config["fans"])
                        .iter()
                        .find(|f| f["id"] == *id)
                        .unwrap())
                    .collect::<Vec<_>>());
                self.commit(c, true)?;
                self.event("风扇卡片顺序已保存");
            }
            "/api/curve" => {
                let curve = &v["curve"];
                if !curve.is_object() {
                    return Err(invalid("曲线必须是对象"));
                }
                let mut c = self.config.clone();
                let pos = array(&c["curves"])
                    .iter()
                    .position(|p| p["id"] == curve["id"]);
                if v.get("expected")
                    .filter(|e| !e.is_null())
                    .is_some_and(|e| pos.is_none_or(|i| !equivalent(&c["curves"][i], e)))
                {
                    return Err(conflict("此曲线已更新，请重新打开后再操作"));
                }
                let id = if text(&curve["id"]).is_empty() {
                    token()?
                } else {
                    text(&curve["id"]).to_owned()
                };
                let saved = json!({"id":id,"name":curve["name"],"points":curve["points"]});
                if let Some(i) = pos {
                    c["curves"][i] = saved;
                } else {
                    c["curves"].as_array_mut().unwrap().push(saved);
                }
                if !text(&v["assign_to"]).is_empty() {
                    let f = c["fans"]
                        .as_array_mut()
                        .unwrap()
                        .iter_mut()
                        .find(|f| f["id"] == v["assign_to"])
                        .ok_or_else(|| invalid("风扇不存在"))?;
                    Self::expected_fields(f, v.get("expected_fan"))?;
                    f["curve_id"] = json!(id);
                }
                self.update(&c, None)?;
                return Ok(json!({"ok":true,"id":id}));
            }
            "/api/curve/delete" => {
                let mut c = self.config.clone();
                let curve = array(&c["curves"])
                    .iter()
                    .find(|p| p["id"] == v["id"])
                    .ok_or_else(|| invalid("曲线不存在"))?;
                if v.get("expected")
                    .filter(|e| !e.is_null())
                    .is_some_and(|e| !equivalent(e, curve))
                {
                    return Err(conflict("此曲线已更新，请刷新后重试"));
                }
                if array(&c["fans"]).iter().any(|f| f["curve_id"] == v["id"]) {
                    return Err(invalid("请先为引用此曲线的风扇选择其他曲线"));
                }
                c["curves"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|p| p["id"] != v["id"]);
                self.update(&c, None)?;
            }
            "/api/scan" => return Ok(json!({"ok":true,"added":self.scan(true)?})),
            "/api/identify" => self.identify(
                text(&v["channel_id"]),
                number(v.get("duty").unwrap_or(&json!(100)), 50., 100., "识别输出")?,
                v["device_kind"].as_str().unwrap_or("unknown"),
            )?,
            "/api/identify-stop" => self.stop_identification(v["channel_id"].as_str(), true)?,
            "/api/identity" => {
                let name = v["name"]
                    .as_str()
                    .filter(|s| (1..=100).contains(&s.trim().chars().count()))
                    .ok_or_else(|| invalid("请输入 1–100 个字符的名称"))?;
                let kind = text(&v["device_kind"]);
                if !["fan", "pump"].contains(&kind) {
                    return Err(invalid("请确认风扇或水泵"));
                }
                self.tick()?;
                let candidate = self
                    .discovery()
                    .into_iter()
                    .find(|c| c["id"] == v["channel_id"])
                    .ok_or_else(|| invalid("通道不存在或已离线"))?;
                if candidate["backend"] == "nvidia" && kind != "fan" {
                    return Err(invalid("NVIDIA 通道只能确认为风扇"));
                }
                let mut c = self.config.clone();
                let i = array(&c["fans"])
                    .iter()
                    .position(|f| f["channel_id"] == v["channel_id"])
                    .unwrap_or_else(|| {
                        c["fans"]
                            .as_array_mut()
                            .unwrap()
                            .push(Self::discovered(&candidate));
                        array(&c["fans"]).len() - 1
                    });
                let f = &mut c["fans"][i];
                f["name"] = json!(name.trim());
                f["identified"] = json!(true);
                f["device_kind"] = json!(kind);
                if kind == "pump" {
                    f["mode"] = json!("manual");
                    f["manual_duty"] = json!(100);
                    f["min_duty"] = json!(100);
                    f["max_duty"] = json!(100);
                }
                let id = f["id"].clone();
                let c = config::validate(&c)?;
                self.stop_identification(v["channel_id"].as_str(), true)?;
                self.commit(c, true)?;
                self.tick()?;
                self.event(format!("已保存设备：{}", name.trim()));
                return Ok(json!({"ok":true,"id":id}));
            }
            "/api/activate" | "/api/release" => {
                self.stop_identification(None, true)?;
                let id = text(&v["id"]);
                let enable = path == "/api/activate";
                if self.managed {
                    let mut c = self.config.clone();
                    let f = c["fans"]
                        .as_array_mut()
                        .unwrap()
                        .iter_mut()
                        .find(|f| text(&f["id"]) == id)
                        .ok_or_else(|| invalid("风扇不存在"))?;
                    if enable && (!yes(&f["identified"]) || text(&f["channel_id"]).is_empty()) {
                        return Err(invalid("请先识别并绑定设备"));
                    }
                    f["autostart"] = json!(enable);
                    self.commit(c, true)?;
                    if enable {
                        self.pending.insert(id.into(), ("等待后台接管".into(), 0.));
                        self.resume();
                    } else {
                        self.release(id, true)?;
                    }
                } else if enable {
                    self.activate(id)?;
                } else {
                    self.release(id, true)?;
                }
                self.tick()?;
            }
            "/api/release-all" => {
                if self.managed {
                    let mut c = self.config.clone();
                    for f in c["fans"].as_array_mut().unwrap() {
                        f["autostart"] = json!(false);
                    }
                    self.commit(c, true)?;
                }
                self.release_all()?;
                self.tick()?;
            }
            _ => return Err(Error(404, "未找到接口".into())),
        }
        Ok(json!({"ok":true}))
    }
    fn expected_fields(f: &Value, expected: Option<&Value>) -> Result<()> {
        if let Some(e) = expected.filter(|e| !e.is_null()) {
            if e.as_object()
                .is_none_or(|m| m.iter().any(|(k, v)| !equivalent(&f[k], v)))
            {
                return Err(conflict("此风扇的设置已更新，请重新打开后再操作"));
            }
        }
        Ok(())
    }
}
