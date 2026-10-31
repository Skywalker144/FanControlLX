//! Pure control policy: temperature aggregation, smoothing and protection.
use crate::{common::*, hardware::Snapshot, telemetry::Inputs};
use serde_core::{ser::SerializeMap, Serialize, Serializer};
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    sync::Arc,
};

#[derive(Default)]
pub struct Memory {
    pub samples: VecDeque<(f64, f64)>,
    pub(super) sample_sum: f64,
    pub last: f64,
    pub changed: f64,
    pub temp: Option<f64>,
    pub duty: Option<f64>,
    pub active_since: Option<f64>,
}
#[cfg(test)]
pub fn interpolate(points: &Value, t: f64) -> f64 {
    interpolate_points(
        &array(points)
            .iter()
            .map(|p| (num(&p[0]), num(&p[1])))
            .collect::<Vec<_>>(),
        t,
    )
}
fn interpolate_points(points: &[(f64, f64)], t: f64) -> f64 {
    if t <= points[0].0 {
        return points[0].1;
    }
    for pair in points.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if t <= b.0 {
            return a.1 + (b.1 - a.1) * (t - a.0) / (b.0 - a.0);
        }
    }
    points.last().unwrap().1
}
// Aggregate in place: safety checks use the maximum before smoothing, even
// when the configured control aggregation is average or minimum.
#[derive(Default)]
pub struct SourceValues {
    count: usize,
    sum: f64,
    min: f64,
    max: f64,
}
impl SourceValues {
    fn push(&mut self, value: f64) {
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.count += 1;
        self.sum += value;
    }
}
pub(super) fn source_value(id: &Value, sensors: &[Value]) -> Option<f64> {
    sensors.iter().find(|s| s["id"] == *id)?["value"].as_f64()
}
pub fn source_values(ids: &Value, sensors: &[Value]) -> Option<SourceValues> {
    let mut result = SourceValues::default();
    for id in array(ids) {
        let kinds: &[&str] = match text(id) {
            "@cpu" => &["cpu"],
            "@gpu" => &["gpu"],
            "@cpu-gpu" => &["cpu", "gpu"],
            _ => &[],
        };
        if kinds.is_empty() {
            result.push(source_value(id, sensors)?);
        } else {
            for kind in kinds {
                let mut maximum: Option<f64> = None;
                for sensor in sensors.iter().filter(|s| text(&s["kind"]) == *kind) {
                    // Missing readings cannot be hidden by a valid maximum.
                    let value = sensor["value"].as_f64()?;
                    maximum = Some(maximum.map_or(value, |m| m.max(value)));
                }
                result.push(maximum?);
            }
        }
    }
    (result.count > 0).then_some(result)
}

// Resolve numeric policy, curve points and source selections on configuration
// changes. The original JSON stays available for persistence and the editor.
pub(super) struct Policy {
    smoothing_s: f64,
    critical_temp: f64,
    hysteresis: f64,
    manual_duty: f64,
    min_duty: f64,
    max_duty: f64,
    response_s: f64,
    step_down: f64,
    step_up: f64,
    manual: bool,
    points: Arc<[(f64, f64)]>,
    sources: Vec<String>,
    aggregation: Aggregation,
}
enum Aggregation {
    Maximum,
    Minimum,
    Average,
}
impl Policy {
    fn new(f: &Value, points: Arc<[(f64, f64)]>) -> Self {
        Self {
            smoothing_s: num(&f["smoothing_s"]),
            critical_temp: num(&f["critical_temp"]),
            hysteresis: num(&f["hysteresis"]),
            manual_duty: num(&f["manual_duty"]),
            min_duty: num(&f["min_duty"]),
            max_duty: num(&f["max_duty"]),
            response_s: num(&f["response_s"]),
            step_down: num(&f["step_down"]),
            step_up: num(&f["step_up"]),
            manual: f["mode"] == "manual",
            points,
            sources: array(&f["sensor_ids"])
                .iter()
                .flat_map(|id| match text(id) {
                    "@cpu-gpu" => vec!["@cpu".to_owned(), "@gpu".to_owned()],
                    id => vec![id.to_owned()],
                })
                .collect(),
            aggregation: match text(&f["aggregation"]) {
                "average" => Aggregation::Average,
                "min" => Aggregation::Minimum,
                _ => Aggregation::Maximum,
            },
        }
    }
    pub(super) fn values(&self, s: &Snapshot, inputs: &Inputs) -> Option<SourceValues> {
        let mut result = SourceValues::default();
        for id in &self.sources {
            result.push(match id.as_str() {
                "@cpu" => inputs.cpu,
                "@gpu" => inputs.gpu,
                _ => inputs.temperature(s, id),
            }?);
        }
        (result.count > 0).then_some(result)
    }
    fn points(value: &Value) -> Arc<[(f64, f64)]> {
        array(value)
            .iter()
            .map(|p| (num(&p[0]), num(&p[1])))
            .collect()
    }
    pub(super) fn all(config: &Value) -> Vec<Self> {
        let mut shared = HashMap::<&str, Arc<[(f64, f64)]>>::new();
        array(&config["fans"])
            .iter()
            .map(|f| {
                let curve = array(&config["curves"])
                    .iter()
                    .find(|c| c["id"] == f["curve_id"]);
                let points = match curve {
                    Some(c) => shared
                        .entry(text(&c["id"]))
                        .or_insert_with(|| Self::points(&c["points"]))
                        .clone(),
                    None => Self::points(&f["points"]),
                };
                Self::new(f, points)
            })
            .collect()
    }
}

// Control results contain numeric values and status messages.
// HTTP workers serialize these fields from an immutable snapshot.
#[derive(Clone)]
pub struct Output {
    pub(super) temperature: Option<f64>,
    pub(super) smoothed: Option<f64>,
    pub(super) target: f64,
    pub(super) rpm: Option<f64>,
    pub(super) status: &'static str,
    pub(super) requested: f64,
    pub(super) actual: Option<f64>,
    pub(super) automatic: bool,
    pub(super) active: bool,
    pub(super) fault: Option<Cow<'static, str>>,
}
impl Serialize for Output {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut m = serializer.serialize_map(Some(10))?;
        m.serialize_entry("temperature", &self.temperature.map(rounded))?;
        m.serialize_entry("smoothed", &self.smoothed.map(rounded))?;
        m.serialize_entry("target", &rounded(self.target))?;
        m.serialize_entry("rpm", &self.rpm)?;
        m.serialize_entry("status", &self.status)?;
        m.serialize_entry("requested", &rounded(self.requested))?;
        m.serialize_entry("actual", &self.actual)?;
        m.serialize_entry("automatic", &self.automatic)?;
        m.serialize_entry("active", &self.active)?;
        m.serialize_entry("fault", &self.fault)?;
        m.end()
    }
}
#[cfg(test)]
pub fn compute(
    f: &Value,
    curves: &Value,
    s: &Snapshot,
    m: &mut Memory,
    active: bool,
    now: f64,
) -> Value {
    let mut inputs = Inputs::default();
    inputs.refresh(s);
    serde_json::to_value(calculate(
        f,
        &Policy::new(
            f,
            Policy::points(
                array(curves)
                    .iter()
                    .find(|c| c["id"] == f["curve_id"])
                    .map(|c| &c["points"])
                    .unwrap_or(&f["points"]),
            ),
        ),
        s,
        &inputs,
        m,
        active,
        now,
    ))
    .unwrap()
}
pub(super) fn calculate(
    f: &Value,
    policy: &Policy,
    s: &Snapshot,
    inputs: &Inputs,
    m: &mut Memory,
    active: bool,
    now: f64,
) -> Output {
    let c = inputs.channels.get(&s.channels, text(&f["channel_id"]));
    let values = policy.values(s, inputs);
    let raw = values.as_ref().map(|v| match policy.aggregation {
        Aggregation::Average => v.sum / v.count as f64,
        Aggregation::Minimum => v.min,
        Aggregation::Maximum => v.max,
    });
    while m
        .samples
        .front()
        .is_some_and(|p| p.0 < now - policy.smoothing_s)
    {
        m.sample_sum -= m.samples.pop_front().unwrap().1;
    }
    if m.samples.is_empty() {
        m.sample_sum = 0.;
    }
    if let Some(raw) = raw {
        m.samples.push_back((now, raw));
        m.sample_sum += raw;
    }
    let smoothed = raw.map(|raw| {
        if m.samples.is_empty() {
            raw
        } else {
            m.sample_sum / m.samples.len() as f64
        }
    });
    let mut fault: Option<Cow<'static, str>> = None;
    let mut automatic = false;
    let mut requested = None;
    let mut duty = if values.is_none() {
        fault = Some("温度源不可用，保护输出 100%".into());
        100.
    } else if values
        .as_ref()
        .is_some_and(|v| v.max >= policy.critical_temp)
    {
        fault = Some("达到保护温度，输出 100%".into());
        100.
    } else {
        let mut effective = smoothed.unwrap();
        if let Some(temp) = m.temp.filter(|t| (effective - t).abs() < policy.hysteresis) {
            effective = temp;
        } else {
            m.temp = Some(effective);
        }
        let mut d = if policy.manual {
            policy.manual_duty
        } else {
            interpolate_points(&policy.points, effective)
        };
        automatic = c.is_some_and(|c| yes(&c["curve_zero_auto"])) && !policy.manual && d == 0.;
        if !automatic {
            d = d.clamp(policy.min_duty, policy.max_duty);
        }
        requested = Some(d);
        if automatic {
            if m.duty != Some(0.) {
                m.changed = now;
            }
        } else if let Some(mut previous) = m.duty {
            if c.is_some_and(|c| yes(&c["curve_zero_auto"])) && previous == 0. {
                previous = policy.min_duty.max(num(&c.unwrap()["min_duty"]));
            }
            if !policy.manual && now - m.changed < policy.response_s {
                d = previous;
            }
            let dt = (now - m.last).clamp(0., 5.);
            d = d.clamp(
                previous - policy.step_down * dt,
                previous + policy.step_up * dt,
            );
            if (d - previous).abs() >= 0.1 {
                m.changed = now;
            }
        }
        d
    };
    if let Some(c) = c.filter(|c| c["temperature_sensor_id"].is_string()) {
        let own = inputs.temperature(s, text(&c["temperature_sensor_id"]));
        if own.is_none() {
            duty = 100.;
            fault = Some("显卡自身温度不可用，保护输出 100%".into());
        } else if own.is_some_and(|t| t >= 85f64.min(policy.critical_temp)) {
            duty = 100.;
            fault = Some("显卡达到保护温度，输出 100%".into());
        }
    }
    let tach = inputs.fans.get(&s.fans, text(&f["rpm_sensor_id"]));
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
    Output {
        temperature: raw,
        smoothed,
        target: duty,
        rpm,
        status: if f["channel_id"].is_null() {
            "待绑定通道"
        } else if c.is_none() {
            "通道离线"
        } else {
            "未接管"
        },
        requested: requested.unwrap_or(duty),
        actual: c.and_then(|c| c["duty"].as_f64()),
        automatic,
        active,
        fault,
    }
}
