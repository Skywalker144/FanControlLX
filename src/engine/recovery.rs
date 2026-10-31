//! Identification, takeover, lease restoration and watchdog recovery.
use super::*;

impl Engine {
    pub(super) fn identification_duty(
        duty: f64,
        s: &[Value],
        c: Option<&Value>,
    ) -> (f64, Option<String>) {
        if let Some(c) = c.filter(|c| c["temperature_sensor_id"].is_string()) {
            if source_value(&c["temperature_sensor_id"], s).is_none_or(|t| t >= 85.) {
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
    pub(super) fn identify(&mut self, key: &str, duty: f64, kind: &str) -> Result<()> {
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
            self.faults.clear(&Fault::Identification);
            self.event("识别测试结束，已恢复此前的控制模式");
            return Ok(());
        }
        if let Err(e) = self.hardware.restore(&t.lease) {
            self.faults.set(
                Fault::Identification,
                format!("识别结束后恢复失败，正在重试：{e}"),
            );
            return Err(e);
        }
        self.faults.clear(&Fault::Identification);
        let t = self.identification.take().unwrap();
        if let Some(id) = t.resume {
            self.faults.clear(&Fault::Restore(id.clone()));
            self.leases.remove(&id);
        }
        self.disarmed.push(t.lease);
        self.maybe_disarm()?;
        self.event("识别测试结束，已恢复接管前状态");
        Ok(())
    }
    pub(super) fn maybe_disarm(&mut self) -> Result<()> {
        if self.leases.is_empty() && self.identification.is_none() {
            if let Err(e) = self.hardware.disarm(&self.disarmed) {
                self.faults
                    .set(Fault::Disarm, format!("停止看门狗失败，正在重试：{e}"));
                return Err(e);
            }
            self.disarmed.clear();
            self.faults.clear(&Fault::Disarm);
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
        let initial = if o.automatic {
            0.
        } else if o.fault.is_some() {
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
            self.faults.set(
                Fault::Restore(id.into()),
                format!("恢复 {id} 控制状态失败：{e}"),
            );
            return Err(e);
        }
        self.faults.clear(&Fault::Restore(id.into()));
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
}
