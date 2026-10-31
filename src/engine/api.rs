//! HTTP operations and optimistic edit validation.
use super::*;

impl Engine {
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
                let mut c = (*self.config).clone();
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
                let mut c = (*self.config).clone();
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
                let mut c = (*self.config).clone();
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
                let mut c = (*self.config).clone();
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
                let mut c = (*self.config).clone();
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
                self.update(&c, None)?;
                self.event(format!("已保存设备：{}", name.trim()));
                return Ok(json!({"ok":true,"id":id}));
            }
            "/api/activate" | "/api/release" => {
                self.stop_identification(None, true)?;
                let id = text(&v["id"]);
                let enable = path == "/api/activate";
                if self.managed {
                    let mut c = (*self.config).clone();
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
                    let mut c = (*self.config).clone();
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
