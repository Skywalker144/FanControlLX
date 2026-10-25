//! Reuse topology lookups, but read every value from the current sample.
use crate::common::text;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Default)]
pub struct RowIndex {
    positions: HashMap<String, usize>,
    len: usize,
}
impl RowIndex {
    pub fn refresh(&mut self, rows: &[Value]) {
        if self.len == rows.len()
            && rows.iter().enumerate().all(|(i, row)| {
                self.positions
                    .get(text(&row["id"]))
                    .is_some_and(|p| *p == i)
            })
        {
            return;
        }
        self.positions.clear();
        self.len = rows.len();
        for (i, row) in rows.iter().enumerate() {
            // For duplicate IDs, the first entry takes precedence.
            self.positions
                .entry(text(&row["id"]).to_owned())
                .or_insert(i);
        }
    }
    pub fn get<'a>(&self, rows: &'a [Value], id: &str) -> Option<&'a Value> {
        self.positions.get(id).and_then(|i| rows.get(*i))
    }
}

#[derive(Default)]
pub struct Inputs {
    pub sensors: RowIndex,
    pub fans: RowIndex,
    pub channels: RowIndex,
    pub cpu: Option<f64>,
    pub gpu: Option<f64>,
}
impl Inputs {
    pub fn refresh(&mut self, s: &crate::hardware::Snapshot) {
        self.sensors.refresh(&s.sensors);
        self.fans.refresh(&s.fans);
        self.channels.refresh(&s.channels);
        // One missing group member must still trigger protection, even when
        // another sensor in that group has a valid reading.
        let (mut cpu, mut gpu) = (None::<f64>, None::<f64>);
        let (mut cpu_missing, mut gpu_missing) = (false, false);
        for row in &s.sensors {
            let (value, missing) = match text(&row["kind"]) {
                "cpu" => (&mut cpu, &mut cpu_missing),
                "gpu" => (&mut gpu, &mut gpu_missing),
                _ => continue,
            };
            if let Some(t) = row["value"].as_f64() {
                *value = Some(value.map_or(t, |old| old.max(t)));
            } else {
                *missing = true;
            }
        }
        self.cpu = cpu.filter(|_| !cpu_missing);
        self.gpu = gpu.filter(|_| !gpu_missing);
    }
    pub fn temperature(&self, s: &crate::hardware::Snapshot, id: &str) -> Option<f64> {
        self.sensors.get(&s.sensors, id)?["value"].as_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::Snapshot;
    use serde_json::json;

    #[test]
    fn indexes_follow_reordering_removal_and_replacement_without_stale_values() {
        let mut rows = vec![json!({"id":"a","value":40}), json!({"id":"b","value":70})];
        let mut index = RowIndex::default();
        index.refresh(&rows);
        rows.swap(0, 1);
        index.refresh(&rows);
        assert_eq!(index.get(&rows, "a").unwrap()["value"], 40);
        rows[1]["value"] = Value::Null;
        index.refresh(&rows);
        assert!(index.get(&rows, "a").unwrap()["value"].is_null());
        rows[1] = json!({"id":"c","value":90});
        index.refresh(&rows);
        assert!(index.get(&rows, "a").is_none());
        assert_eq!(index.get(&rows, "c").unwrap()["value"], 90);
        rows.clear();
        index.refresh(&rows);
        assert!(index.get(&rows, "c").is_none());
        assert!(index.positions.is_empty());
        rows = vec![json!({"id":"a","value":40}), json!({"id":"a","value":70})];
        index.refresh(&rows);
        assert_eq!(index.get(&rows, "a").unwrap()["value"], 40);
    }

    #[test]
    fn grouped_inputs_require_all_members_and_follow_kind_changes() {
        let mut s = Snapshot {
            sensors: vec![
                json!({"id":"a","kind":"cpu","value":40}),
                json!({"id":"b","kind":"cpu","value":80}),
                json!({"id":"c","kind":"gpu","value":50}),
            ],
            ..Snapshot::default()
        };
        let mut inputs = Inputs::default();
        inputs.refresh(&s);
        assert_eq!((inputs.cpu, inputs.gpu), (Some(80.), Some(50.)));
        s.sensors[0]["value"] = Value::Null;
        inputs.refresh(&s);
        assert_eq!((inputs.cpu, inputs.gpu), (None, Some(50.)));
        s.sensors[0]["kind"] = json!("other");
        s.sensors[1]["kind"] = json!("gpu");
        inputs.refresh(&s);
        assert_eq!((inputs.cpu, inputs.gpu), (None, Some(80.)));
        s.sensors.clear();
        inputs.refresh(&s);
        assert_eq!((inputs.cpu, inputs.gpu), (None, None));
    }
}
