use crate::{
    common::*,
    config,
    engine::{compute, source_values, Engine, Memory},
    hardware::{Demo, Hardware, Real, Snapshot},
    writer::{Gpu, Writer},
};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub(crate) struct Temp(pub(crate) PathBuf);
impl Temp {
    pub(crate) fn new() -> Self {
        let p = std::env::temp_dir().join(format!("fancontrollx-test-{}", token().unwrap()));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Mock {
    sample: Snapshot,
    samples: usize,
    duties: std::collections::HashMap<String, f64>,
    acquires: usize,
    writes: usize,
    restores: usize,
    fail_restore: bool,
    fail_write: bool,
    acquired_duties: Vec<f64>,
}
#[derive(Clone)]
struct MockHardware(Arc<Mutex<Mock>>);
impl Hardware for MockHardware {
    fn sample(&mut self, _: bool) -> Result<Snapshot> {
        let mut m = self.0.lock().unwrap();
        m.samples += 1;
        let mut s = m.sample.clone();
        for c in &mut s.channels {
            c["duty"] = json!(m.duties[text(&c["id"])]);
        }
        Ok(s)
    }
    fn acquire(&mut self, k: &str, d: f64) -> Result<Value> {
        let mut m = self.0.lock().unwrap();
        m.acquires += 1;
        m.acquired_duties.push(d);
        let l = json!({"key":k,"duty":m.duties[k]});
        m.duties.insert(k.into(), d);
        Ok(l)
    }
    fn write(&mut self, k: &str, d: f64) -> Result<Value> {
        let mut m = self.0.lock().unwrap();
        if m.fail_write {
            return Err(conflict("write failure"));
        }
        m.writes += 1;
        m.duties.insert(k.into(), d);
        Ok(Value::Null)
    }
    fn restore(&mut self, l: &Value) -> Result<()> {
        let mut m = self.0.lock().unwrap();
        if m.fail_restore {
            return Err(conflict("restore failure"));
        }
        m.restores += 1;
        m.duties.insert(text(&l["key"]).into(), num(&l["duty"]));
        Ok(())
    }
    fn disarm(&mut self, _: &[Value]) -> Result<()> {
        Ok(())
    }
}
fn setup(managed: bool) -> (Temp, Engine, MockHardware, String) {
    let t = Temp::new();
    let s = Demo::new().sample(false).unwrap();
    let h = MockHardware(Arc::new(Mutex::new(Mock {
        duties: s
            .channels
            .iter()
            .map(|c| (text(&c["id"]).into(), num(&c["duty"])))
            .collect(),
        sample: s,
        samples: 0,
        acquires: 0,
        writes: 0,
        restores: 0,
        fail_restore: false,
        fail_write: false,
        acquired_duties: vec![],
    })));
    let mut e = Engine::new(
        Box::new(h.clone()),
        t.0.join("config.json"),
        true,
        false,
        managed,
    )
    .unwrap();
    let id = text(&e.config["fans"][0]["id"]).to_owned();
    let key = e.config["fans"][0]["channel_id"].clone();
    e.request(
        "/api/identity",
        &json!({"channel_id":key,"name":"Case","device_kind":"fan"}),
    )
    .unwrap();
    (t, e, h, id)
}
fn policy() -> (Value, Snapshot, Memory) {
    let mut f = config::default_fan("f", "Fan", "@cpu");
    f["channel_id"] = json!("demo/pwm1");
    f["rpm_sensor_id"] = json!("demo/fan1");
    let mut s = Demo::new().sample(false).unwrap();
    s.sensors[0]["value"] = json!(50);
    s.sensors[1]["value"] = json!(40);
    (f, s, Memory::default())
}
#[test]
fn config_roundtrip_and_original_v1_backup() {
    let t = Temp::new();
    let p = t.0.join("config.json");
    let original = b"{\"version\":1,\"fans\":[{\"id\":\"f\",\"name\":\"Fan\"}]}  \n";
    fs::write(&p, original).unwrap();
    let v = config::validate(&serde_json::from_slice(original).unwrap()).unwrap();
    config::save(&p, &v).unwrap();
    config::save(&p, &v).unwrap();
    assert_eq!(
        fs::read(t.0.join("config.before-v2.json")).unwrap(),
        original
    );
    assert_eq!(config::validate(&v).unwrap(), v);
}
#[test]
fn config_invalid_inputs_rejected() {
    for v in [
        json!({"version":true,"fans":[]}),
        json!({"version":2,"fans":null}),
        json!({"version":2,"fans":[{"id":"f","name":"F","manual_duty":true}]}),
        json!({"version":2,"fans":[{"id":"f","name":"F","sensor_ids":[]}]}),
        json!({"version":2,"fans":[{"id":"f","name":"F","curve_id":"absent"}]}),
    ] {
        assert!(config::validate(&v).is_err());
    }
    let mut f = config::default_fan("f", "F", "@cpu");
    f["points"] = json!([[50, 20], [50, 80]]);
    assert!(config::validate(&json!({"version":2,"fans":[f]})).is_err());
}
#[test]
fn sources_require_every_selected_group() {
    let (_, mut s, _) = policy();
    s.sensors.retain(|s| s["kind"] != "gpu");
    assert!(source_values(&json!(["@cpu-gpu"]), &s.sensors).is_none());
    s.sensors[0]["value"] = Value::Null;
    assert!(source_values(&json!(["@cpu"]), &s.sensors).is_none());
}
#[test]
fn raw_critical_bypasses_smoothing_limits_and_manual() {
    let (mut f, mut s, mut m) = policy();
    f["mode"] = json!("manual");
    f["manual_duty"] = json!(20);
    f["max_duty"] = json!(30);
    f["aggregation"] = json!("average");
    f["sensor_ids"] = json!(["@cpu", "@gpu"]);
    compute(&f, &json!([]), &s, &mut m, true, 1.);
    s.sensors[0]["value"] = json!(100);
    let o = compute(&f, &json!([]), &s, &mut m, true, 2.);
    assert_eq!(num(&o["target"]), 100.);
    assert!(o["fault"].is_string());
}
#[test]
fn missing_temperature_protects_immediately() {
    let (f, mut s, mut m) = policy();
    s.sensors[0]["value"] = Value::Null;
    let o = compute(&f, &json!([]), &s, &mut m, true, 2.);
    assert_eq!(num(&o["target"]), 100.);
}
#[test]
fn stall_protection_exempts_intentional_zero_and_nvml_intended_rpm() {
    let (mut f, mut s, mut m) = policy();
    m.active_since = Some(0.);
    f["mode"] = json!("manual");
    f["manual_duty"] = json!(0);
    s.fans[0]["rpm"] = json!(0);
    let o = compute(&f, &json!([]), &s, &mut m, true, 11.);
    assert!(o["fault"].is_null());
    assert_eq!(num(&o["target"]), 0.);
    f["manual_duty"] = json!(50);
    let o = compute(&f, &json!([]), &s, &mut m, true, 12.);
    assert_eq!(num(&o["target"]), 100.);
    s.fans[0]["rpm_is_feedback"] = json!(false);
    let o = compute(&f, &json!([]), &s, &mut m, true, 13.);
    assert!(o["fault"].is_null());
}
#[test]
fn gpu_zero_auto_and_own_temperature_guard() {
    let (mut f, mut s, mut m) = policy();
    f["points"] = json!([[30, 0], [80, 0]]);
    f["min_duty"] = json!(30);
    s.channels[0]["curve_zero_auto"] = json!(true);
    s.channels[0]["min_duty"] = json!(30);
    s.channels[0]["temperature_sensor_id"] = json!("demo/gpu");
    let o = compute(&f, &json!([]), &s, &mut m, true, 1.);
    assert!(yes(&o["automatic"]));
    assert_eq!(num(&o["target"]), 0.);
    s.sensors[1]["value"] = json!(90);
    let o = compute(&f, &json!([]), &s, &mut m, true, 2.);
    assert_eq!(num(&o["target"]), 100.);
    assert!(!yes(&o["automatic"]));
    s.sensors[1]["value"] = Value::Null;
    assert_eq!(
        num(&compute(&f, &json!([]), &s, &mut m, true, 3.)["target"]),
        100.
    );
    s.sensors[1]["value"] = json!(40);
    assert!(yes(
        &compute(&f, &json!([]), &s, &mut m, true, 4.)["automatic"]
    ));
    f["mode"] = json!("manual");
    f["manual_duty"] = json!(0);
    let o = compute(&f, &json!([]), &s, &mut m, true, 5.);
    assert!(!yes(&o["automatic"]));
    assert_eq!(num(&o["target"]), 30.);
}
#[test]
fn shared_curves_preserve_independent_sources() {
    let (mut f, s, _) = policy();
    f["curve_id"] = json!("shared");
    f["response_s"] = json!(0);
    let curves = json!([{"id":"shared","name":"S","points":[[30,0],[80,100]]}]);
    let a = compute(&f, &curves, &s, &mut Memory::default(), false, 1.);
    f["sensor_ids"] = json!(["@gpu"]);
    let b = compute(&f, &curves, &s, &mut Memory::default(), false, 1.);
    assert_eq!(num(&a["target"]), 40.);
    assert_eq!(num(&b["target"]), 20.);
}
#[test]
fn discovery_and_edits_never_acquire() {
    let (_t, mut e, h, id) = setup(false);
    assert_eq!(e.config["fans"].as_array().unwrap().len(), 3);
    e.request("/api/fan", &json!({"id":id,"changes":{"name":"New"}}))
        .unwrap();
    assert_eq!(h.0.lock().unwrap().acquires, 0);
    assert_eq!(h.0.lock().unwrap().writes, 0);
}
#[test]
fn takeover_release_and_live_edit_keep_original_lease() {
    let (_t, mut e, h, id) = setup(false);
    e.activate(&id).unwrap();
    let original = e.leases[&id].clone();
    e.request(
        "/api/fan",
        &json!({"id":id,"changes":{"points":[[20,35],[90,90]]}}),
    )
    .unwrap();
    assert_eq!(e.leases[&id], original);
    assert_eq!(h.0.lock().unwrap().acquires, 1);
    e.release(&id, true).unwrap();
    assert_eq!(h.0.lock().unwrap().duties["demo/pwm1"], 38.);
}
#[test]
fn managed_restart_and_stop_policy() {
    let (t, mut e, h, id) = setup(true);
    e.request("/api/activate", &json!({"id":id})).unwrap();
    assert!(e.enabled.contains(&id));
    e.shutdown().unwrap();
    let mut next = Engine::new(
        Box::new(h.clone()),
        t.0.join("config.json"),
        true,
        false,
        true,
    )
    .unwrap();
    next.resume();
    assert!(next.enabled.contains(&id));
    next.request("/api/release", &json!({"id":id})).unwrap();
    assert!(!yes(&next.config["fans"][0]["autostart"]));
    next.shutdown().unwrap();
    let mut next = Engine::new(Box::new(h), t.0.join("config.json"), true, false, true).unwrap();
    next.resume();
    assert!(next.enabled.is_empty());
}
#[test]
fn monitor_and_unconfirmed_never_acquire() {
    let (_t, mut e, h, id) = setup(true);
    e.allow = false;
    assert!(e.activate(&id).is_err());
    e.request("/api/activate", &json!({"id":id})).unwrap();
    assert!(e.enabled.is_empty());
    assert_eq!(h.0.lock().unwrap().acquires, 0);
    e.allow = true;
    let unconfirmed = text(&e.config["fans"][1]["id"]).to_owned();
    assert!(e.activate(&unconfirmed).is_err());
}
#[test]
fn identification_expiry_resumes_lease_and_stop_all_never_resumes() {
    let (_t, mut e, h, id) = setup(true);
    e.request("/api/activate", &json!({"id":id})).unwrap();
    let lease = e.leases[&id].clone();
    e.request(
        "/api/identify",
        &json!({"channel_id":"demo/pwm1","duty":70,"device_kind":"fan"}),
    )
    .unwrap();
    assert!(!e.enabled.contains(&id));
    let s = h.clone().sample(false).unwrap();
    e.tick_with(s, 100.).unwrap();
    assert!(e.enabled.contains(&id));
    assert_eq!(e.leases[&id], lease);
    e.request(
        "/api/identify",
        &json!({"channel_id":"demo/pwm1","duty":70,"device_kind":"fan"}),
    )
    .unwrap();
    e.request("/api/release-all", &json!({})).unwrap();
    assert!(e.enabled.is_empty());
    assert!(e.leases.is_empty());
    assert_eq!(h.0.lock().unwrap().duties["demo/pwm1"], 38.);
}
#[test]
fn failed_restore_retains_lease_and_is_retried() {
    let (_t, mut e, h, id) = setup(false);
    e.activate(&id).unwrap();
    h.0.lock().unwrap().fail_restore = true;
    assert!(e.release(&id, true).is_err());
    assert!(e.leases.contains_key(&id));
    assert!(!e.enabled.contains(&id));
    assert!(e.activate(&id).is_err());
    h.0.lock().unwrap().fail_restore = false;
    e.tick().unwrap();
    assert!(e.leases.is_empty());
    assert_eq!(h.0.lock().unwrap().duties["demo/pwm1"], 38.);
}
#[test]
fn failed_identification_stop_does_not_resume_on_retry() {
    let (_t, mut e, h, id) = setup(true);
    e.request("/api/activate", &json!({"id":id})).unwrap();
    e.request(
        "/api/identify",
        &json!({"channel_id":"demo/pwm1","duty":70,"device_kind":"fan"}),
    )
    .unwrap();
    h.0.lock().unwrap().fail_restore = true;
    assert!(e.request("/api/release-all", &json!({})).is_err());
    h.0.lock().unwrap().fail_restore = false;
    e.tick().unwrap();
    e.resume();
    assert!(e.enabled.is_empty());
    assert!(e.leases.is_empty());
}
#[test]
fn concurrent_edits_and_null_expectations() {
    let (_t, mut e, _h, id) = setup(false);
    e.request(
        "/api/fan",
        &json!({"id":id,"changes":{"manual_duty":50.0},"expected":null}),
    )
    .unwrap();
    e.request(
        "/api/fan",
        &json!({"id":id,"changes":{"manual_duty":60},"expected":{"manual_duty":50}}),
    )
    .unwrap();
    assert_eq!(
        e.request(
            "/api/fan",
            &json!({"id":id,"changes":{"manual_duty":70},"expected":{"manual_duty":50}})
        )
        .unwrap_err()
        .0,
        409
    );
    let curve=e.request("/api/curve",&json!({"curve":{"name":"S","points":[[20,0],[80,100]]},"expected":null,"assign_to":id,"expected_fan":null})).unwrap();
    assert_eq!(e.config["fans"][0]["curve_id"], curve["id"]);
    assert!(e
        .request("/api/curve/delete", &json!({"id":curve["id"]}))
        .is_err());
}
#[test]
fn binding_change_requires_reconfirmation_and_releases() {
    let (_t, mut e, h, id) = setup(true);
    e.request("/api/activate", &json!({"id":id})).unwrap();
    e.request(
        "/api/fan",
        &json!({"id":id,"changes":{"channel_id":"offline/pwm1"}}),
    )
    .unwrap();
    assert_eq!(e.config["fans"][0]["identified"], false);
    assert_eq!(e.config["fans"][0]["autostart"], false);
    assert!(e.enabled.is_empty());
    assert_eq!(h.0.lock().unwrap().restores, 1);
}
#[test]
fn reorder_preserves_leases_and_rejects_duplicates() {
    let (_t, mut e, h, id) = setup(true);
    e.request("/api/activate", &json!({"id":id})).unwrap();
    let before = e.config.clone();
    let mut ids: Vec<_> = array(&before["fans"])
        .iter()
        .map(|f| f["id"].clone())
        .collect();
    ids.reverse();
    e.request("/api/fan-order", &json!({"ids":ids})).unwrap();
    assert_eq!(h.0.lock().unwrap().acquires, 1);
    assert!(e
        .request("/api/fan-order", &json!({"ids":[id,id,id]}))
        .is_err());
}
#[test]
fn failed_save_does_not_release_active_hardware() {
    let (t, mut e, h, id) = setup(true);
    e.request("/api/activate", &json!({"id":id})).unwrap();
    fs::remove_file(t.0.join("config.json")).unwrap();
    fs::create_dir(t.0.join("config.json")).unwrap();
    assert!(e
        .request("/api/fan", &json!({"id":id,"changes":{"autostart":false}}))
        .is_err());
    assert!(e.enabled.contains(&id));
    assert_eq!(h.0.lock().unwrap().restores, 0);
}
#[test]
fn state_deltas_omit_configuration_and_old_history() {
    let (_t, e, _h, _id) = setup(false);
    let full = e.state(None, None);
    let delta = e.state(full["config_revision"].as_str(), Some(num(&full["time"])));
    assert!(delta.get("config").is_none());
    assert!(array(&delta["history"]).is_empty());
    assert_eq!(delta["history_delta"], true);
    assert!(e.state(Some("stale"), None).get("config").is_some());
}
#[test]
fn sysfs_metadata_cache_and_watchdog_are_independent_of_duty() {
    let t = Temp::new();
    let hw = t.0.join("hwmon0");
    fs::create_dir(&hw).unwrap();
    for (name, value) in [
        ("name", "nct6687"),
        ("temp1_input", "50000"),
        ("temp1_label", "CPU"),
        ("fan1_input", "1000"),
        ("pwm1", "100"),
        ("pwm1_enable", "2"),
        ("fan_control_watchdog", "0"),
    ] {
        fs::write(hw.join(name), value).unwrap();
    }
    let mut h = Real::with_root(t.0.clone(), None);
    let s = h.sample(true).unwrap();
    let key = text(&s.channels[0]["id"]).to_owned();
    fs::write(hw.join("temp1_label"), "Changed").unwrap();
    fs::write(hw.join("temp1_input"), "60000").unwrap();
    let next = h.sample(false).unwrap();
    assert_eq!(next.sensors[0]["name"], "CPU");
    assert_eq!(num(&next.sensors[0]["value"]), 60.);
    let lease = h.acquire(&key, 50.).unwrap();
    assert_eq!(
        fs::read_to_string(hw.join("fan_control_watchdog")).unwrap(),
        "15\n"
    );
    h.write(&key, 50.).unwrap();
    h.restore(&lease).unwrap();
    h.disarm(&[lease]).unwrap();
    assert_eq!(fs::read_to_string(hw.join("pwm1")).unwrap(), "100\n");
    assert_eq!(fs::read_to_string(hw.join("pwm1_enable")).unwrap(), "2\n");
    assert_eq!(
        fs::read_to_string(hw.join("fan_control_watchdog")).unwrap(),
        "0\n"
    );
}
#[test]
fn sysfs_renumber_keeps_identity_and_restores_original_mode() {
    use std::os::unix::fs::symlink;
    let t = Temp::new();
    let class = t.0.join("class");
    let device = t.0.join("device/hwmon");
    let old = device.join("hwmon0");
    fs::create_dir_all(&old).unwrap();
    fs::create_dir(&class).unwrap();
    for (name, value) in [
        ("name", "nct6687"),
        ("fan1_input", "1000"),
        ("pwm1", "100"),
        ("pwm1_enable", "2"),
        ("fan_control_watchdog", "0"),
    ] {
        fs::write(old.join(name), value).unwrap();
    }
    symlink(&old, class.join("hwmon0")).unwrap();
    let mut h = Real::with_root(class.clone(), None);
    let s = h.sample(true).unwrap();
    let key = text(&s.channels[0]["id"]).to_owned();
    let lease = h.acquire(&key, 50.).unwrap();
    let new = device.join("hwmon7");
    fs::rename(&old, &new).unwrap();
    fs::remove_file(class.join("hwmon0")).unwrap();
    symlink(&new, class.join("hwmon7")).unwrap();
    let s = h.sample(true).unwrap();
    assert_eq!(text(&s.channels[0]["id"]), key);
    h.restore(&lease).unwrap();
    h.disarm(&[lease]).unwrap();
    assert_eq!(fs::read_to_string(new.join("pwm1")).unwrap(), "100\n");
    assert_eq!(fs::read_to_string(new.join("pwm1_enable")).unwrap(), "2\n");
    assert_eq!(
        fs::read_to_string(new.join("fan_control_watchdog")).unwrap(),
        "0\n"
    );
}

struct FakeGpu {
    policy: u32,
    target: u32,
    writes: usize,
    fail_restore: bool,
    journal: PathBuf,
    fail_write: bool,
}
impl Gpu for FakeGpu {
    fn original(&mut self, key: &str) -> Result<Value> {
        Ok(
            json!({"channel_id":key,"policy":self.policy,"target":if self.policy==1{Some(self.target)}else{None}}),
        )
    }
    fn write(&mut self, _: &str, d: f64) -> Result<Value> {
        assert!(self.journal.exists());
        if self.fail_write {
            return Err(conflict("write failed"));
        }
        self.writes += 1;
        self.policy = if d == 0. { 0 } else { 1 };
        self.target = d as u32;
        Ok(json!({"duty":d,"automatic":d==0.}))
    }
    fn restore(&mut self, l: &Value) -> Result<()> {
        if self.fail_restore {
            return Err(conflict("restore failed"));
        }
        self.policy = num(&l["policy"]) as u32;
        self.target = num(&l["target"]) as u32;
        Ok(())
    }
}
#[test]
fn writer_journals_before_write_and_restores_original_policy() {
    let t = Temp::new();
    let journal = t.0.join("nvidia-leases.json");
    let gpu = FakeGpu {
        policy: 1,
        target: 42,
        writes: 0,
        fail_restore: false,
        journal: journal.clone(),
        fail_write: false,
    };
    let mut w = Writer::new(gpu, journal.clone()).unwrap();
    let key = "nvidia/GPU-abcd/fan0";
    let lease = w
        .dispatch(&json!({"action":"acquire","channel_id":key,"duty":0}))
        .unwrap();
    assert_eq!(w.gpu.policy, 0);
    w.dispatch(&json!({"action":"write","channel_id":key,"token":lease["token"],"duty":65}))
        .unwrap();
    w.dispatch(&json!({"action":"restore","channel_id":key,"token":lease["token"]}))
        .unwrap();
    assert_eq!(w.gpu.policy, 1);
    assert_eq!(w.gpu.target, 42);
    assert!(w.leases.is_empty());
}
#[test]
fn writer_failed_restore_survives_restart_and_stale_tokens_fail() {
    let t = Temp::new();
    let journal = t.0.join("nvidia-leases.json");
    let key = "nvidia/GPU-abcd/fan0";
    let gpu = FakeGpu {
        policy: 0,
        target: 0,
        writes: 0,
        fail_restore: false,
        journal: journal.clone(),
        fail_write: false,
    };
    let mut w = Writer::new(gpu, journal.clone()).unwrap();
    let lease = w
        .dispatch(&json!({"action":"acquire","channel_id":key,"duty":50}))
        .unwrap();
    w.gpu.fail_restore = true;
    assert!(!w.expire(true).is_empty());
    assert!(journal.exists());
    assert!(w
        .dispatch(&json!({"action":"write","channel_id":key,"token":"old","duty":70}))
        .is_err());
    let gpu = FakeGpu {
        policy: 1,
        target: 50,
        writes: 0,
        fail_restore: false,
        journal: journal.clone(),
        fail_write: false,
    };
    let mut next = Writer::new(gpu, journal).unwrap();
    assert!(next
        .dispatch(&json!({"action":"write","channel_id":key,"token":lease["token"],"duty":70}))
        .is_err());
    assert!(next.expire(true).is_empty());
    assert_eq!(next.gpu.policy, 0);
}
#[test]
fn writer_journal_failure_never_writes() {
    let t = Temp::new();
    let parent = t.0.join("journal-dir");
    fs::create_dir(&parent).unwrap();
    let journal = parent.join("leases.json");
    let gpu = FakeGpu {
        policy: 0,
        target: 0,
        writes: 0,
        fail_restore: false,
        journal: journal.clone(),
        fail_write: false,
    };
    let mut w = Writer::new(gpu, journal).unwrap();
    fs::remove_dir(&parent).unwrap();
    fs::write(&parent, "not a directory").unwrap();
    assert!(w
        .dispatch(&json!({"action":"acquire","channel_id":"nvidia/GPU-abcd/fan0","duty":50}))
        .is_err());
    assert_eq!(w.gpu.writes, 0);
    assert!(w.leases.is_empty());
}

// Behavioral coverage migrated from the retired Python implementation.
#[test]
fn interpolation_defaults_limits_and_zero_motherboard_curve() {
    let (mut f, s, _) = policy();
    assert_eq!(f["points"], json!([[30, 20], [50, 50], [80, 100]]));
    for (temperature, duty) in [(0., 20.), (40., 35.), (120., 100.)] {
        assert_eq!(crate::engine::interpolate(&f["points"], temperature), duty);
    }
    f["points"] = json!([[20, 0], [80, 0]]);
    f["min_duty"] = json!(30);
    let o = compute(&f, &json!([]), &s, &mut Memory::default(), false, 1.);
    assert_eq!(num(&o["target"]), 30.);
    assert_eq!(o["automatic"], false);
    for d in [0, 10, 20] {
        f["mode"] = json!("manual");
        f["manual_duty"] = json!(d);
        f["min_duty"] = json!(0);
        f["max_duty"] = json!(d);
        assert_eq!(
            num(&compute(&f, &json!([]), &s, &mut Memory::default(), false, 1.)["target"]),
            d as f64
        );
    }
}
#[test]
fn takeover_starts_at_half_with_limits_or_full_for_emergency() {
    for (min, max, temp, expected) in [
        (0, 100, 50, 50.),
        (70, 100, 50, 70.),
        (0, 40, 50, 40.),
        (0, 40, 100, 100.),
    ] {
        let (_t, mut e, h, id) = setup(false);
        h.0.lock().unwrap().sample.sensors[0]["value"] = json!(temp);
        e.request(
            "/api/fan",
            &json!({"id":id,"changes":{"min_duty":min,"max_duty":max}}),
        )
        .unwrap();
        e.activate(&id).unwrap();
        assert_eq!(h.0.lock().unwrap().acquired_duties, vec![expected]);
        e.shutdown().unwrap();
    }
}
#[test]
fn invalid_configs_do_not_change_running_policy() {
    let (_t, mut e, h, id) = setup(true);
    e.request("/api/activate", &json!({"id":id})).unwrap();
    let original = e.config.clone();
    let lease = e.leases[&id].clone();
    let changes = [
        json!({"manual_duty":true}),
        json!({"manual_duty":"NaN"}),
        json!({"manual_duty":101}),
        json!({"min_duty":70,"max_duty":20}),
        json!({"sensor_ids":[]}),
        json!({"sensor_ids":["@cpu","@cpu"]}),
        json!({"mode":"invalid"}),
        json!({"aggregation":"invalid"}),
        json!({"points":[[50,20],[50,80]]}),
        json!({"curve_id":"absent"}),
        json!({"identified":true}),
        json!({"step_up":0}),
    ];
    for changes in changes {
        assert!(e
            .request("/api/fan", &json!({"id":id,"changes":changes}))
            .is_err());
        assert_eq!(e.config, original);
        assert_eq!(e.leases[&id], lease);
    }
    let mut bad = original.clone();
    bad["fans"][1]["channel_id"] = bad["fans"][0]["channel_id"].clone();
    assert!(e.request("/api/config", &bad).is_err());
    assert!(e.enabled.contains(&id));
    assert_eq!(h.0.lock().unwrap().acquires, 1);
}
#[test]
fn corrupt_saved_config_is_not_overwritten() {
    let t = Temp::new();
    let p = t.0.join("config.json");
    fs::write(&p, "{invalid").unwrap();
    assert!(Engine::new(Box::new(Demo::new()), p.clone(), true, true, true).is_err());
    assert_eq!(fs::read_to_string(p).unwrap(), "{invalid");
}
#[test]
fn foreground_restart_never_resumes_saved_service_policy() {
    let (t, mut e, h, id) = setup(true);
    e.request("/api/activate", &json!({"id":id})).unwrap();
    e.shutdown().unwrap();
    let mut next = Engine::new(
        Box::new(h.clone()),
        t.0.join("config.json"),
        true,
        false,
        false,
    )
    .unwrap();
    next.resume();
    assert!(next.enabled.is_empty());
    assert_eq!(h.0.lock().unwrap().acquires, 1);
}
#[test]
fn only_confirmed_bound_devices_resume_and_late_device_keeps_policy() {
    let (t, mut e, h, id) = setup(true);
    e.request("/api/activate", &json!({"id":id})).unwrap();
    e.shutdown().unwrap();
    let channels = h.0.lock().unwrap().sample.channels.clone();
    h.0.lock().unwrap().sample.channels.clear();
    let mut next = Engine::new(
        Box::new(h.clone()),
        t.0.join("config.json"),
        true,
        false,
        true,
    )
    .unwrap();
    next.resume();
    assert!(next.enabled.is_empty());
    assert!(yes(&next.config["fans"][0]["autostart"]));
    h.0.lock().unwrap().sample.channels = channels;
    next.tick().unwrap();
    next.pending.get_mut(&id).unwrap().1 = 0.;
    next.resume();
    assert!(next.enabled.contains(&id));
    next.request("/api/release-all", &json!({})).unwrap();
    assert!(array(&next.config["fans"])
        .iter()
        .all(|f| !yes(&f["autostart"])));
}
#[test]
fn offline_and_zero_rpm_discovery_never_invent_connections() {
    let (t, _e, h, _id) = setup(false);
    {
        let mut m = h.0.lock().unwrap();
        m.sample.channels.clear();
    }
    let e = Engine::new(
        Box::new(h.clone()),
        t.0.join("empty.json"),
        true,
        false,
        false,
    )
    .unwrap();
    assert!(array(&e.config["fans"]).is_empty());
    let sample = Demo::new().sample(false).unwrap();
    {
        let mut m = h.0.lock().unwrap();
        m.sample = sample;
        for f in &mut m.sample.fans {
            f["rpm"] = json!(0);
        }
    }
    let e = Engine::new(
        Box::new(h.clone()),
        t.0.join("zero.json"),
        true,
        false,
        false,
    )
    .unwrap();
    assert!(array(&e.config["fans"]).is_empty());
    assert!(!e.discovery().is_empty());
    assert_eq!(h.0.lock().unwrap().acquires, 0);
}
#[test]
fn identity_and_rescan_preserve_curve_source_and_do_not_activate() {
    let (_t, mut e, h, id) = setup(false);
    e.request(
        "/api/fan",
        &json!({"id":id,"changes":{"points":[[20,35],[90,90]],"sensor_ids":["@gpu"]}}),
    )
    .unwrap();
    let points = e.config["fans"][0]["points"].clone();
    e.request(
        "/api/identity",
        &json!({"channel_id":"demo/pwm1","name":"CPU Fan","device_kind":"fan"}),
    )
    .unwrap();
    let samples = h.0.lock().unwrap().samples;
    e.scan(true).unwrap();
    e.scan(true).unwrap();
    assert_eq!(
        h.0.lock().unwrap().samples - samples,
        2,
        "One fresh hardware sample per manual scan"
    );
    assert_eq!(array(&e.config["fans"]).len(), 3);
    assert_eq!(e.config["fans"][0]["points"], points);
    assert_eq!(e.config["fans"][0]["sensor_ids"], json!(["@gpu"]));
    assert_eq!(e.config["fans"][0]["name"], "CPU Fan");
    assert_eq!(h.0.lock().unwrap().acquires, 0);
}
#[test]
fn identification_validation_single_test_and_shutdown_restore() {
    let (_t, mut e, h, _id) = setup(false);
    for request in [
        json!({"channel_id":"absent","duty":100}),
        json!({"channel_id":"demo/pwm1","duty":101}),
    ] {
        assert!(e.request("/api/identify", &request).is_err());
    }
    e.allow = false;
    assert!(e
        .request(
            "/api/identify",
            &json!({"channel_id":"demo/pwm1","duty":100})
        )
        .is_err());
    e.allow = true;
    let config = e.config.clone();
    e.request(
        "/api/identify",
        &json!({"channel_id":"demo/pwm1","duty":70,"device_kind":"fan"}),
    )
    .unwrap();
    e.request(
        "/api/identify",
        &json!({"channel_id":"demo/pwm2","duty":100}),
    )
    .unwrap();
    assert_eq!(h.0.lock().unwrap().duties["demo/pwm1"], 38.);
    e.request(
        "/api/identify",
        &json!({"channel_id":"demo/pwm1","duty":70,"device_kind":"fan"}),
    )
    .unwrap();
    assert!(e.state(None, None)["identification"].get("lease").is_none());
    h.0.lock().unwrap().sample.sensors[0]["value"] = json!(100);
    e.tick().unwrap();
    assert_eq!(h.0.lock().unwrap().duties["demo/pwm1"], 100.);
    e.shutdown().unwrap();
    assert_eq!(h.0.lock().unwrap().duties["demo/pwm1"], 38.);
    assert_eq!(e.config, config);
}
#[test]
fn confirmed_pump_uses_full_output_defaults() {
    let (_t, mut e, _, _) = setup(false);
    e.request(
        "/api/identity",
        &json!({"channel_id":"demo/pwm2","name":"Pump","device_kind":"pump"}),
    )
    .unwrap();
    let f = array(&e.config["fans"])
        .iter()
        .find(|f| f["channel_id"] == "demo/pwm2")
        .unwrap();
    assert_eq!(f["min_duty"], 100);
    assert_eq!(f["max_duty"], 100);
}
#[test]
fn scoped_edits_and_shared_curve_creation_are_atomic() {
    let (_t, mut e, h, id) = setup(true);
    e.request("/api/activate", &json!({"id":id})).unwrap();
    let other = e.config["fans"][1]["id"].clone();
    e.request(
        "/api/fan",
        &json!({"id":other,"changes":{"manual_duty":77}}),
    )
    .unwrap();
    let c=e.request("/api/curve",&json!({"curve":{"name":"S","points":[[20,20],[80,100]]},"assign_to":id,"expected_fan":{"curve_id":null}})).unwrap();
    let before = e.config.clone();
    assert!(e.request("/api/curve",&json!({"curve":{"name":"stale","points":[[20,20],[80,100]]},"assign_to":id,"expected_fan":{"curve_id":null}})).is_err());
    assert_eq!(e.config, before);
    assert_eq!(e.config["fans"][1]["manual_duty"], 77);
    assert_eq!(h.0.lock().unwrap().acquires, 1);
    e.request("/api/fan", &json!({"id":id,"changes":{"curve_id":null}}))
        .unwrap();
    e.request("/api/curve/delete", &json!({"id":c["id"]}))
        .unwrap();
    assert!(e
        .request("/api/fan", &json!({"id":id,"changes":{"curve_id":c["id"]}}))
        .is_err());
}
#[test]
fn smoothing_hysteresis_response_and_rate_limits_use_elapsed_time() {
    let (mut f, mut s, mut m) = policy();
    f["points"] = json!([[0, 0], [100, 100]]);
    f["smoothing_s"] = json!(0);
    f["response_s"] = json!(3);
    f["step_up"] = json!(10);
    f["step_down"] = json!(5);
    assert_eq!(
        num(&compute(&f, &json!([]), &s, &mut m, true, 0.)["target"]),
        50.
    );
    s.sensors[0]["value"] = json!(51);
    assert_eq!(
        num(&compute(&f, &json!([]), &s, &mut m, true, 1.)["target"]),
        50.
    );
    s.sensors[0]["value"] = json!(80);
    assert_eq!(
        num(&compute(&f, &json!([]), &s, &mut m, true, 2.)["target"]),
        50.
    );
    assert_eq!(
        num(&compute(&f, &json!([]), &s, &mut m, true, 3.)["target"]),
        60.
    );
    f["response_s"] = json!(0);
    s.sensors[0]["value"] = json!(30);
    assert_eq!(
        num(&compute(&f, &json!([]), &s, &mut m, true, 4.)["target"]),
        55.
    );
    f["smoothing_s"] = json!(5);
    let mut m = Memory::default();
    compute(&f, &json!([]), &s, &mut m, false, 0.);
    s.sensors[0]["value"] = json!(50);
    assert_eq!(
        num(&compute(&f, &json!([]), &s, &mut m, false, 1.)["smoothed"]),
        40.
    );
}
#[test]
fn background_discovery_only_for_live_editors_or_missing_devices() {
    let mut d = crate::engine::Discovery::default();
    assert!(!d.due(600., false));
    d.editor_until = 615.;
    assert!(d.due(600., false));
    assert!(!d.due(610., false));
    assert!(!d.due(630., false)); // Closed/hidden/crashed tab lease expired.
    assert!(d.due(630., true)); // Missing configured device still reconnects.
    assert!(!d.due(631., true)); // Recovery is bounded, not every tick.
    d.editor_until = 700.;
    assert!(d.due(660., false));
}
#[test]
fn viewer_has_no_configuration_tokens_or_discovery_and_never_writes() {
    let (_t, mut e, h, id) = setup(true);
    let config = e.config.clone();
    for _ in 0..10 {
        let v = e.view_state();
        for key in ["config", "csrf_token", "discovery", "history", "events"] {
            assert!(v.get(key).is_none());
        }
        assert!(array(&v["fans"]).iter().any(|f| f["id"] == id));
    }
    e.request("/api/editor-heartbeat", &json!({})).unwrap();
    assert_eq!(e.config, config);
    assert_eq!(h.0.lock().unwrap().acquires, 0);
}

#[test]
fn viewer_resolves_shared_and_independent_curves_including_manual_mode() {
    let (_t, mut e, _h, id) = setup(true);
    let fan = array(&e.config["fans"])
        .iter()
        .find(|f| f["id"] == id)
        .unwrap();
    let view = e.view_state();
    let shown = array(&view["fans"]).iter().find(|f| f["id"] == id).unwrap();
    assert_eq!(shown["points"], fan["points"]);
    assert!(shown["curve_name"].is_null());
    let curve = e
        .request(
            "/api/curve",
            &json!({
                "curve":{"name":"Shared display", "points":[[20,10],[80,90]]},
                "assign_to":id,"expected_fan":{"curve_id":null}
            }),
        )
        .unwrap();
    let curve = array(&e.config["curves"])
        .iter()
        .find(|c| c["id"] == curve["id"])
        .unwrap()
        .clone();
    e.request("/api/fan", &json!({"id":id,"changes":{"mode":"manual"}}))
        .unwrap();
    let view = e.view_state();
    let shown = array(&view["fans"]).iter().find(|f| f["id"] == id).unwrap();
    assert_eq!(shown["points"], curve["points"]);
    assert_eq!(shown["curve_name"], "Shared display");
    assert_eq!(shown["mode"], "manual");
    e.request(
        "/api/curve",
        &json!({"curve":{
        "id":curve["id"],"name":"Updated display","points":[[20,0],[90,100]]
    },"expected":curve}),
    )
    .unwrap();
    let view = e.view_state();
    let shown = array(&view["fans"]).iter().find(|f| f["id"] == id).unwrap();
    assert_eq!(shown["points"], json!([[20, 0], [90, 100]]));
    assert_eq!(shown["curve_name"], "Updated display");
}

#[test]
fn gpu_zero_curve_switches_and_restart_keep_one_original_lease() {
    let (t, mut e, h, id) = setup(true);
    {
        let mut m = h.0.lock().unwrap();
        let c = &mut m.sample.channels[0];
        c["curve_zero_auto"] = json!(true);
        c["min_duty"] = json!(30);
        c["temperature_sensor_id"] = json!("demo/gpu");
        m.sample.fans[0]["rpm_is_feedback"] = json!(false);
    }
    e.request("/api/fan",&json!({"id":id,"changes":{"points":[[20,0],[80,0]],"min_duty":30,"response_s":0,"smoothing_s":0}})).unwrap();
    e.request("/api/activate", &json!({"id":id})).unwrap();
    assert_eq!(h.0.lock().unwrap().acquired_duties, vec![0.]);
    let lease = e.leases[&id].clone();
    e.request(
        "/api/fan",
        &json!({"id":id,"changes":{"points":[[20,70],[80,70]]}}),
    )
    .unwrap();
    assert_eq!(e.leases[&id], lease);
    e.request(
        "/api/fan",
        &json!({"id":id,"changes":{"points":[[20,0],[80,0]]}}),
    )
    .unwrap();
    assert_eq!(h.0.lock().unwrap().duties["demo/pwm1"], 0.);
    e.shutdown().unwrap();
    let mut next = Engine::new(
        Box::new(h.clone()),
        t.0.join("config.json"),
        true,
        false,
        true,
    )
    .unwrap();
    next.resume();
    assert_eq!(h.0.lock().unwrap().acquired_duties, vec![0., 0.]);
    h.0.lock().unwrap().sample.sensors[1]["value"] = Value::Null;
    next.tick().unwrap();
    assert_eq!(h.0.lock().unwrap().duties["demo/pwm1"], 100.);
    h.0.lock().unwrap().sample.sensors[1]["value"] = json!(40);
    next.tick().unwrap();
    assert_eq!(h.0.lock().unwrap().duties["demo/pwm1"], 0.);
}
#[test]
fn writer_channel_deadlines_are_independent_and_failed_acquire_restores() {
    use std::time::{Duration, Instant};
    let t = Temp::new();
    let journal = t.0.join("leases.json");
    let gpu = FakeGpu {
        policy: 0,
        target: 0,
        writes: 0,
        fail_restore: false,
        fail_write: false,
        journal: journal.clone(),
    };
    let mut w = Writer::new(gpu, journal.clone()).unwrap();
    let a = "nvidia/GPU-abcd/fan0";
    let b = "nvidia/GPU-abcd/fan1";
    let lease = w
        .dispatch(&json!({"action":"acquire","channel_id":a,"duty":60}))
        .unwrap();
    let other = w
        .dispatch(&json!({"action":"acquire","channel_id":b,"duty":70}))
        .unwrap();
    w.deadlines
        .insert(a.into(), Instant::now() - Duration::from_secs(1));
    assert!(w.expire(false).is_empty());
    assert!(!w.leases.contains_key(a));
    assert!(w.leases.contains_key(b));
    assert!(w
        .dispatch(&json!({"action":"write","channel_id":a,"token":lease["token"],"duty":50}))
        .is_err());
    w.dispatch(&json!({"action":"write","channel_id":b,"token":other["token"],"duty":80}))
        .unwrap();
    w.expire(true);
    w.gpu.fail_write = true;
    assert!(w
        .dispatch(&json!({"action":"acquire","channel_id":a,"duty":60}))
        .is_err());
    assert!(w.leases.is_empty());
    assert!(array(
        &serde_json::from_slice::<Value>(&fs::read(journal).unwrap()).unwrap()["leases"]
    )
    .is_empty());
}
