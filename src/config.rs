use crate::common::*;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::Path,
};

pub fn default_fan(id: &str, name: &str, source: &str) -> Value {
    json!({"id":id,"name":name,"channel_id":null,"rpm_sensor_id":null,
        "identified":false,"device_kind":"unknown","autostart":false,"curve_id":null,
        "sensor_ids":[source],"aggregation":"max","mode":"curve",
        "points":[[30,20],[50,50],[80,100]],"manual_duty":50,"min_duty":0,"max_duty":100,
        "hysteresis":2,"response_s":3,"smoothing_s":5,"step_up":25,"step_down":5,"critical_temp":85})
}
pub fn default_config() -> Value {
    json!({"version":2,"fans":[],"curves":[]})
}
fn name(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| (1..=100).contains(&s.trim().chars().count()))
}
pub fn points(v: &Value) -> Result<()> {
    let p = v
        .as_array()
        .filter(|a| (2..=32).contains(&a.len()))
        .ok_or_else(|| invalid("曲线需要 2–32 个节点"))?;
    let mut prev = -21.;
    for pair in p {
        if pair.as_array().is_none_or(|a| a.len() != 2) {
            return Err(invalid("曲线节点必须是 [温度, 百分比]"));
        }
        let t = number(&pair[0], -20., 120., "节点温度")?;
        number(&pair[1], 0., 100., "节点输出")?;
        if t <= prev {
            return Err(invalid("曲线温度必须递增，不能重复"));
        }
        prev = t;
    }
    Ok(())
}
pub fn validate(v: &Value) -> Result<Value> {
    if !v.is_object() || !matches!(v["version"].as_u64(), Some(1 | 2)) {
        return Err(invalid("配置版本必须为 1 或 2"));
    }
    let empty = vec![];
    let curves = match v.get("curves") {
        Some(c) => c.as_array().ok_or_else(|| invalid("curves 必须是列表"))?,
        None => &empty,
    };
    if curves.len() > 128 {
        return Err(invalid("最多保存 128 条曲线"));
    }
    let mut ids = HashSet::new();
    let mut library = vec![];
    for c in curves {
        if !name(&c["id"]) || !name(&c["name"]) || !ids.insert(text(&c["id"])) {
            return Err(invalid("曲线 ID 或名称无效、重复"));
        }
        points(&c["points"])?;
        library.push(json!({"id":c["id"],"name":text(&c["name"]).trim(),"points":c["points"]}));
    }
    let fans = v["fans"]
        .as_array()
        .filter(|a| a.len() <= 64)
        .ok_or_else(|| invalid("fans 必须是列表，最多 64 个风扇"))?;
    let mut fan_ids = HashSet::new();
    let mut channels = HashSet::new();
    let mut result = vec![];
    for item in fans {
        let obj = item
            .as_object()
            .ok_or_else(|| invalid("风扇配置必须是对象"))?;
        let mut f = default_fan("", "", "@cpu");
        for (k, v) in f.as_object_mut().unwrap() {
            if let Some(x) = obj.get(k) {
                *v = x.clone();
            }
        }
        if !name(&f["id"]) || !name(&f["name"]) || !fan_ids.insert(text(&f["id"]).to_owned()) {
            return Err(invalid("风扇 ID 或名称无效、重复"));
        }
        if !f["identified"].is_boolean()
            || !f["autostart"].is_boolean()
            || !["unknown", "fan", "pump"].contains(&text(&f["device_kind"]))
        {
            return Err(invalid("设备类型、确认状态或持续控制开关无效"));
        }
        for k in ["channel_id", "rpm_sensor_id"] {
            if !f[k].is_null() && f[k].as_str().is_none_or(|s| s.chars().count() > 256) {
                return Err(invalid(format!("{k} 无效")));
            }
        }
        if !text(&f["channel_id"]).is_empty() && !channels.insert(text(&f["channel_id"]).to_owned())
        {
            return Err(invalid("一个 PWM 通道只能绑定一个风扇配置"));
        }
        if !["curve", "manual"].contains(&text(&f["mode"]))
            || !["max", "min", "average"].contains(&text(&f["aggregation"]))
        {
            return Err(invalid("控制模式或传感器混合方式无效"));
        }
        if !f["curve_id"].is_null()
            && (!f["curve_id"].is_string() || !ids.contains(text(&f["curve_id"])))
        {
            return Err(invalid("所选曲线不存在"));
        }
        let sensors = f["sensor_ids"]
            .as_array()
            .filter(|a| (1..=64).contains(&a.len()))
            .ok_or_else(|| invalid("请至少选择一个有效温度源"))?;
        let mut seen = HashSet::new();
        for s in sensors {
            if s.as_str()
                .is_none_or(|s| s.is_empty() || s.chars().count() > 256)
                || !seen.insert(text(s))
            {
                return Err(invalid("温度源无效或重复"));
            }
        }
        for (k, l, h) in [
            ("manual_duty", 0., 100.),
            ("min_duty", 0., 100.),
            ("max_duty", 0., 100.),
            ("hysteresis", 0., 15.),
            ("response_s", 0., 30.),
            ("smoothing_s", 0., 60.),
            ("step_up", 1., 100.),
            ("step_down", 1., 100.),
            ("critical_temp", 50., 105.),
        ] {
            number(&f[k], l, h, k)?;
        }
        if num(&f["min_duty"]) > num(&f["max_duty"]) {
            return Err(invalid("最低输出不能高于最高输出"));
        }
        points(&f["points"])?;
        result.push(f);
    }
    Ok(json!({"version":2,"fans":result,"curves":library}))
}
// Same-directory rename, durable contents and directory, mode 0600. Fail before
// changing live policy if persistence fails. Preserve the old v1 byte sequence.
pub fn save(path: &Path, value: &Value) -> Result<()> {
    let parent = path.parent().ok_or_else(|| invalid("配置路径无效"))?;
    fs::create_dir_all(parent)?;
    if value["version"] == 2 && path.exists() {
        let old = fs::read(path)?;
        let parsed: Value = serde_json::from_slice(&old)?;
        let backup = path.with_file_name(format!(
            "{}.before-v2.json",
            path.file_stem().unwrap().to_string_lossy()
        ));
        if parsed["version"] == 1 && !backup.exists() {
            atomic(&backup, &old)?;
        }
    }
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic(path, &bytes)
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap();
    let temp = parent.join(format!(".config-{}", token()?));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        if unsafe { libc::geteuid() } == 0 {
            let owner = fs::metadata(parent)?;
            if unsafe { libc::fchown(file.as_raw_fd(), owner.uid(), owner.gid()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(temp);
    result
}
