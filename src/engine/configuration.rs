//! Persist desired configuration, then reconcile hardware ownership.
use super::*;

impl Engine {
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
        // Persistence is the commit point. Publish the same desired policy in
        // memory before any fallible hardware work; never report an old revision
        // for a configuration that will be loaded on restart.
        self.commit(v, true)?;
        let desired = self.config.clone();
        let mut errors = Vec::new();
        let resume_identification = self.identification.as_ref().is_some_and(|t| {
            t.resume.as_ref().is_some_and(|id| {
                array(&desired["fans"]).iter().any(|f| {
                    text(&f["id"]) == id
                        && text(&f["channel_id"]) == t.channel
                        && yes(&f["identified"])
                        && (!self.managed || yes(&f["autostart"]))
                })
            })
        });
        if let Err(e) = self.stop_identification(None, resume_identification) {
            errors.push(e.1);
        }
        for id in self.leases.keys().cloned().collect::<Vec<_>>() {
            let wanted = array(&desired["fans"]).iter().find(|f| {
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
                self.enabled.remove(&id);
                self.outputs.remove(&id);
                // A failed identification restore still owns this same lease.
                // Its retry must finish before a new binding can be acquired.
                if self
                    .identification
                    .as_ref()
                    .is_some_and(|t| t.resume.as_ref() == Some(&id))
                {
                    continue;
                }
                if let Err(e) = self.release(&id, true) {
                    errors.push(e.1);
                }
            }
        }
        for f in array(&desired["fans"]) {
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
                    m.sample_sum = 0.;
                    m.temp = None;
                }
            }
        }
        self.memory.retain(|id, _| {
            array(&self.config["fans"])
                .iter()
                .any(|f| text(&f["id"]) == id)
        });
        if let Err(e) = self.tick() {
            errors.push(e.1);
        }
        self.queue();
        self.resume();
        if !errors.is_empty() {
            let message = format!(
                "配置已保存；部分硬件尚未应用，后台将重试：{}",
                errors.join("；")
            );
            self.event(&message);
            return Err(conflict(message));
        }
        self.event("配置已保存并应用");
        Ok(())
    }
}
