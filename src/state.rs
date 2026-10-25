//! Immutable, on-demand HTTP snapshots serialized by HTTP workers.
use crate::{
    common::*,
    engine::{HistorySample, Output},
    hardware::Snapshot,
};
use serde_core::{
    ser::{SerializeMap, SerializeSeq},
    Serialize, Serializer,
};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

pub struct ReadSnapshot {
    pub config: Arc<Value>,
    pub editor: Option<EditorSnapshot>,
    pub outputs: HashMap<String, Output>,
    pub pending: HashMap<String, String>,
    pub history: VecDeque<HistorySample>,
    pub demo: bool,
    pub managed: bool,
    pub error: Option<String>,
    pub revision: String,
    pub view_revision: String,
    pub history_cursor: u64,
    pub cpu: Option<f64>,
    pub gpu: Option<f64>,
}

pub struct EditorSnapshot {
    pub hardware: Snapshot,
    pub events: VecDeque<Value>,
    pub identification: Option<Value>,
    pub allow: bool,
    pub gpu_error: Option<String>,
    pub gpu_control_error: Option<String>,
}

#[derive(Clone, Copy)]
pub enum Query<'a> {
    Editor {
        since: Option<&'a str>,
        after: Option<f64>,
    },
    Viewer {
        since: Option<&'a str>,
        after: Option<u64>,
    },
}
#[derive(Clone, Copy)]
pub struct Metadata<'a> {
    pub token: &'a str,
    pub hostname: &'a str,
}
pub struct Response<'a> {
    snapshot: &'a ReadSnapshot,
    query: Query<'a>,
    metadata: Option<Metadata<'a>>,
}
impl ReadSnapshot {
    pub fn response<'a>(
        &'a self,
        query: Query<'a>,
        metadata: Option<Metadata<'a>>,
    ) -> Response<'a> {
        Response {
            snapshot: self,
            query,
            metadata,
        }
    }
}

// Serialize history, fan cards and discovery through iterators borrowing the snapshot.
struct Rows<I>(I);
impl<I: Clone + Iterator> Serialize for Rows<I>
where
    I::Item: Serialize,
{
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(None)?;
        for row in self.0.clone() {
            seq.serialize_element(&row)?;
        }
        seq.end()
    }
}

impl Serialize for Response<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let s = self.snapshot;
        let mut m = serializer.serialize_map(None)?;
        m.serialize_entry("time", &wall())?;
        m.serialize_entry("runtime", "rust")?;
        m.serialize_entry("demo", &s.demo)?;
        m.serialize_entry("background_service", &s.managed)?;
        m.serialize_entry("error", &s.error)?;
        if let Some(meta) = self.metadata {
            m.serialize_entry("hostname", meta.hostname)?;
            m.serialize_entry("version", crate::server::VERSION)?;
            if matches!(self.query, Query::Editor { .. }) {
                m.serialize_entry("csrf_token", meta.token)?;
            }
        }
        match self.query {
            Query::Editor { since, after } => {
                let editor = s.editor.as_ref().ok_or_else(|| {
                    <S::Error as serde_core::ser::Error>::custom("Editor snapshot required")
                })?;
                m.serialize_entry("sensors", &editor.hardware.sensors)?;
                m.serialize_entry("fans", &editor.hardware.fans)?;
                m.serialize_entry("channels", &editor.hardware.channels)?;
                m.serialize_entry("chips", &editor.hardware.chips)?;
                m.serialize_entry("outputs", &s.outputs)?;
                m.serialize_entry("pending_autostart", &s.pending)?;
                m.serialize_entry("history_delta", &after.is_some())?;
                m.serialize_entry(
                    "history",
                    &Rows(
                        s.history
                            .iter()
                            .filter(|h| after.is_none_or(|t| h.time > t)),
                    ),
                )?;
                m.serialize_entry("events", &editor.events)?;
                m.serialize_entry("control_allowed", &editor.allow)?;
                m.serialize_entry("gpu_error", &editor.gpu_error)?;
                m.serialize_entry("gpu_control_error", &editor.gpu_control_error)?;
                m.serialize_entry("identification", &editor.identification)?;
                m.serialize_entry("config_revision", &s.revision)?;
                m.serialize_entry("workspace_api", &2)?;
                m.serialize_entry(
                    "discovery",
                    &Rows(editor.hardware.channels.iter().map(|channel| Discovery {
                        snapshot: s,
                        hardware: &editor.hardware,
                        channel,
                    })),
                )?;
                if since != Some(s.revision.as_str()) {
                    m.serialize_entry("config", &s.config)?;
                }
            }
            Query::Viewer { since, after } => {
                let unchanged = since == Some(s.view_revision.as_str());
                let after = after.filter(|cursor| {
                    unchanged && *cursor <= s.history_cursor && s.history_cursor - cursor <= 80
                });
                let count = after.map_or(80, |cursor| (s.history_cursor - cursor) as usize);
                m.serialize_entry("sample_time", &s.history.back().map(|h| h.time))?;
                m.serialize_entry("cpu", &s.cpu)?;
                m.serialize_entry("gpu", &s.gpu)?;
                m.serialize_entry("view_revision", &s.view_revision)?;
                m.serialize_entry("fans_delta", &unchanged)?;
                m.serialize_entry("history_cursor", &s.history_cursor)?;
                m.serialize_entry("history_delta", &after.is_some())?;
                m.serialize_entry(
                    "history",
                    &Rows(s.history.iter().skip(s.history.len().saturating_sub(count))),
                )?;
                m.serialize_entry(
                    "fans",
                    &Rows(array(&s.config["fans"]).iter().map(|fan| Fan {
                        snapshot: s,
                        fan,
                        unchanged,
                    })),
                )?;
            }
        }
        m.end()
    }
}

struct Fan<'a> {
    snapshot: &'a ReadSnapshot,
    fan: &'a Value,
    unchanged: bool,
}
impl Serialize for Fan<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let (s, f) = (self.snapshot, self.fan);
        let id = text(&f["id"]);
        let mut m = serializer.serialize_map(None)?;
        m.serialize_entry("id", &f["id"])?;
        m.serialize_entry("output", &s.outputs.get(id))?;
        m.serialize_entry("pending", &s.pending.get(id))?;
        if !self.unchanged {
            let curve = array(&s.config["curves"])
                .iter()
                .find(|c| c["id"] == f["curve_id"]);
            m.serialize_entry("name", &f["name"])?;
            m.serialize_entry("mode", &f["mode"])?;
            m.serialize_entry(
                "points",
                curve.map(|c| &c["points"]).unwrap_or(&f["points"]),
            )?;
            m.serialize_entry("curve_name", &curve.map(|c| &c["name"]))?;
        }
        m.end()
    }
}

struct Discovery<'a> {
    snapshot: &'a ReadSnapshot,
    hardware: &'a Snapshot,
    channel: &'a Value,
}
impl Serialize for Discovery<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let (s, c) = (self.snapshot, self.channel);
        let f = array(&s.config["fans"])
            .iter()
            .find(|f| f["channel_id"] == c["id"]);
        let rpm_id = f
            .filter(|f| !text(&f["rpm_sensor_id"]).is_empty())
            .map(|f| &f["rpm_sensor_id"])
            .unwrap_or(&c["rpm_sensor_id"]);
        let rpm = self
            .hardware
            .fans
            .iter()
            .find(|tach| tach["id"] == *rpm_id)
            .map(|tach| &tach["rpm"])
            .unwrap_or(&Value::Null);
        const OVERRIDES: &[&str] = &[
            "rpm_sensor_id",
            "rpm",
            "connection",
            "fan_id",
            "display_name",
            "identified",
            "device_kind",
        ];
        let mut m = serializer.serialize_map(None)?;
        if let Some(fields) = c.as_object() {
            for (key, value) in fields {
                if !OVERRIDES.contains(&key.as_str()) {
                    m.serialize_entry(key, value)?;
                }
            }
        }
        m.serialize_entry("rpm_sensor_id", rpm_id)?;
        m.serialize_entry("rpm", rpm)?;
        m.serialize_entry(
            "connection",
            if num(rpm) > 0. {
                "rotating"
            } else if yes(&c["present"]) {
                "present"
            } else {
                "unconfirmed"
            },
        )?;
        m.serialize_entry("fan_id", &f.map(|f| &f["id"]))?;
        m.serialize_entry("display_name", f.map(|f| &f["name"]).unwrap_or(&c["name"]))?;
        m.serialize_entry("identified", &f.is_some_and(|f| yes(&f["identified"])))?;
        if let Some(f) = f {
            m.serialize_entry("device_kind", &f["device_kind"])?;
        } else {
            m.serialize_entry("device_kind", "unknown")?;
        }
        m.end()
    }
}
