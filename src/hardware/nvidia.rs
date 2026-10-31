//! NVIDIA discovery, telemetry and read-only fallback sampling.
use super::*;

impl Real {
    pub(super) fn gpu(&mut self, s: &mut Snapshot, force: bool) {
        if self.nvml.is_none() && Instant::now() >= self.gpu_retry {
            match Nvml::new() {
                Ok(n) => {
                    self.nvml = Some(n);
                    self.gpu_at = None;
                }
                Err(e) => {
                    self.gpu_error = Some(e.1);
                    self.gpu_retry = Instant::now() + Duration::from_secs(30);
                }
            }
        }
        let Some(n) = self.nvml.as_mut() else {
            self.clear_gpu(s);
            self.fallback_gpu(s);
            return;
        };
        let sample = (|| -> Result<()> {
            if force
                || self.gpu_at.is_none()
                || (self.gpu_invalidated
                    && self
                        .gpu_at
                        .is_some_and(|at| at.elapsed() >= Duration::from_secs(30)))
            {
                let devices = n.devices()?;
                let mut metadata = vec![];
                for device in devices {
                    let mut channels = vec![];
                    match n.uint(b"nvmlDeviceGetNumFans\0", device.handle) {
                        Ok(count) => {
                            for index in 0..count {
                                let key = format!("nvidia/{}/fan{index}", device.uuid);
                                let mut reason = None;
                                let mut limits = (30, 100);
                                match n.limits(device.handle).and_then(|l| {
                                    n.original(&key)?;
                                    Ok(l)
                                }) {
                                    Ok(l) => limits = l,
                                    Err(e) => reason = Some(e.1),
                                }
                                if reason.is_none()
                                    && (self.data.is_none() || unsafe { libc::geteuid() } != 0)
                                {
                                    reason = Some("NVIDIA 调速需要 sudo 控制会话或后台服务".into());
                                }
                                channels.push(json!({"id":key,"name":format!("{} · 风扇 {}",device.name,index+1),"chip":"nvidia","backend":"nvidia",
                                "writable":reason.is_none(),"reason":reason,"path":"NVML","watchdog":false,"software_watchdog":true,
                                "min_duty":limits.0,"max_duty":limits.1,"present":true,"curve_zero_auto":true,
                                "temperature_sensor_id":format!("nvidia/{}",device.uuid),"rpm_sensor_id":format!("{key}/rpm"),"rpm_is_feedback":false,"index":index}));
                            }
                        }
                        Err(e) => self.control_error = Some(e.1),
                    }
                    let sid = format!("nvidia/{}", device.uuid);
                    let sensor = json!({"id":sid,"name":device.name,"chip":"nvidia","kind":"gpu","value":null});
                    let chip = json!({"id":sid,"name":device.name,"path":"NVML"});
                    metadata.push(GpuEntry {
                        device,
                        sensor,
                        chip,
                        channels,
                    });
                }
                self.gpu_entries = metadata;
                static GENERATION: AtomicU64 = AtomicU64::new(1);
                self.gpu_generation = GENERATION.fetch_add(1, Ordering::Relaxed);
                self.gpu_at = Some(Instant::now());
                self.gpu_invalidated = false;
            }
            if s.gpu_generation != self.gpu_generation {
                s.sensors.truncate(self.sensors.len());
                s.fans.truncate(self.fans.len());
                s.channels.truncate(self.channel_order.len());
                s.chips.truncate(self.chips.len());
                for entry in &self.gpu_entries {
                    s.sensors.push(entry.sensor.clone());
                    s.chips.push(entry.chip.clone());
                    for meta in &entry.channels {
                        let mut channel = meta.clone();
                        channel.as_object_mut().unwrap().remove("index");
                        s.fans
                            .push(json!({"id":channel["rpm_sensor_id"],"name":channel["name"],
                            "chip":"nvidia","rpm":null,"rpm_is_feedback":false}));
                        s.channels.push(channel);
                    }
                }
                s.gpu_generation = self.gpu_generation;
            }
            let mut temp_error = false;
            let mut errors = if self.gpu_entries.is_empty() {
                String::new()
            } else {
                self.control_error.take().unwrap_or_default()
            };
            errors.clear();
            let mut fan_index = self.fans.len();
            let mut channel_index = self.channel_order.len();
            for (device_index, entry) in self.gpu_entries.iter().enumerate() {
                let d = &entry.device;
                let temp = n.temperature(d.handle).ok();
                temp_error |= temp.is_none();
                update_field(
                    &mut s.sensors[self.sensors.len() + device_index],
                    "value",
                    json!(temp),
                );
                for meta in &entry.channels {
                    let index = meta["index"].as_u64().unwrap() as u32;
                    let c = &mut s.channels[channel_index];
                    let policy = n.fan_uint(b"nvmlDeviceGetFanControlPolicy_v2\0", d.handle, index);
                    let duty = n.fan_uint(b"nvmlDeviceGetFanSpeed_v2\0", d.handle, index);
                    let failed = policy.is_err() || duty.is_err();
                    let reason = if failed {
                        "NVIDIA 读数不可用"
                    } else {
                        text(&meta["reason"])
                    };
                    update_field(c, "writable", json!(!failed && yes(&meta["writable"])));
                    if text(&c["reason"]) != reason {
                        update_field(
                            c,
                            "reason",
                            if failed {
                                json!(reason)
                            } else {
                                meta["reason"].clone()
                            },
                        );
                    }
                    // Do not let a read error bypass the discovery backoff.
                    self.gpu_invalidated |= failed;
                    update_field(c, "mode", json!(policy.ok()));
                    update_field(c, "duty", json!(duty.ok()));
                    if !reason.is_empty() {
                        if !errors.is_empty() {
                            errors.push('；');
                        }
                        errors.push_str(reason);
                    }
                    update_field(
                        &mut s.fans[fan_index],
                        "rpm",
                        json!(n.rpm(d.handle, index).ok()),
                    );
                    fan_index += 1;
                    channel_index += 1;
                }
            }
            self.gpu_error = if temp_error {
                Some("GPU 温度不可用".into())
            } else {
                None
            };
            let control_failed = !errors.is_empty();
            if control_failed {
                self.control_error = Some(errors);
            } else if !self.gpu_entries.is_empty() {
                self.control_error = None;
            }
            self.gpu_invalidated |= temp_error || control_failed;
            Ok(())
        })();
        if let Err(e) = sample {
            self.gpu_error = Some(e.1.clone());
            self.control_error = Some(e.1);
            self.nvml = None;
            self.gpu_entries.clear();
            self.gpu_retry = Instant::now() + Duration::from_secs(10);
            self.clear_gpu(s);
        }
        if !s.sensors.iter().any(|v| v["kind"] == "gpu") {
            self.clear_gpu(s);
            self.fallback_gpu(s);
        }
    }
    fn clear_gpu(&self, s: &mut Snapshot) {
        s.sensors.truncate(self.sensors.len());
        s.fans.truncate(self.fans.len());
        s.channels.truncate(self.channel_order.len());
        s.chips.truncate(self.chips.len());
        s.gpu_generation = 0;
    }
    fn fallback_gpu(&mut self, s: &mut Snapshot) {
        // Compatibility fallback only. Never fork nvidia-smi on the normal NVML
        // path. Back off failures and bound a hung subprocess to two seconds.
        if self.fallback_at.is_none_or(|a| {
            a.elapsed() >= Duration::from_secs(if self.fallback_failed { 30 } else { 3 })
        }) {
            self.fallback_at = Some(Instant::now());
            self.fallback.clear();
            use std::process::{Command, Stdio};
            let query = (|| -> Result<String> {
                let mut child = Command::new("nvidia-smi")
                    .args([
                        "--query-gpu=uuid,name,temperature.gpu",
                        "--format=csv,noheader,nounits",
                    ])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()?;
                let limit = Instant::now() + Duration::from_secs(2);
                while child.try_wait()?.is_none() {
                    if Instant::now() >= limit {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(conflict("nvidia-smi 超时"));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let out = child.wait_with_output()?;
                if !out.status.success() {
                    return Err(conflict("nvidia-smi 查询失败"));
                }
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            })();
            match query {
                Ok(out) => {
                    for line in out.lines() {
                        let parts: Vec<_> = line.split(',').map(str::trim).collect();
                        if parts.len() == 3 {
                            if let Ok(t) = parts[2].parse::<f64>() {
                                if (-40.0..=150.).contains(&t) {
                                    self.fallback.push(json!({"id":format!("nvidia/{}",parts[0]),"name":parts[1],"chip":"nvidia","kind":"gpu","value":t}));
                                }
                            }
                        }
                    }
                    self.fallback_failed = self.fallback.is_empty();
                    if !self.fallback_failed {
                        self.gpu_error = None;
                    }
                }
                Err(e) => {
                    self.gpu_error = Some(e.1);
                    self.fallback_failed = true;
                }
            }
        }
        s.sensors.extend(self.fallback.clone());
    }
}
