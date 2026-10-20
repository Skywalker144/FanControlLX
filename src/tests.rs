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

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
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
    duties: std::collections::HashMap<String, f64>,
    acquires: usize,
    writes: usize,
    restores: usize,
    fail_restore: bool,
    fail_write: bool,
}
#[derive(Clone)]
struct MockHardware(Arc<Mutex<Mock>>);
impl Hardware for MockHardware {
    fn sample(&mut self, _: bool) -> Result<Snapshot> {
        let m = self.0.lock().unwrap();
        let mut s = m.sample.clone();
        for c in &mut s.channels {
            c["duty"] = json!(m.duties[text(&c["id"])]);
        }
        Ok(s)
    }
    fn acquire(&mut self, k: &str, d: f64) -> Result<Value> {
        let mut m = self.0.lock().unwrap();
        m.acquires += 1;
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
        acquires: 0,
        writes: 0,
        restores: 0,
        fail_restore: false,
        fail_write: false,
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
}
impl Gpu for FakeGpu {
    fn original(&mut self, key: &str) -> Result<Value> {
        Ok(
            json!({"channel_id":key,"policy":self.policy,"target":if self.policy==1{Some(self.target)}else{None}}),
        )
    }
    fn write(&mut self, _: &str, d: f64) -> Result<Value> {
        assert!(self.journal.exists());
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
