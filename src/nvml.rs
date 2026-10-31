//! Only this module crosses the optional NVML C ABI. Handles belong to an
//! initialized library and are invalidated together on an error/reconnect.
use crate::common::*;
use libloading::Library;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    ffi::{c_char, c_void, CStr, CString},
    time::{Duration, Instant},
};
type Handle = *mut c_void;
type Init = unsafe extern "C" fn() -> i32;
type Uint = unsafe extern "C" fn(Handle, *mut u32) -> i32;
type FanUint = unsafe extern "C" fn(Handle, u32, *mut u32) -> i32;
type StringFn = unsafe extern "C" fn(Handle, *mut c_char, u32) -> i32;
type SetFan = unsafe extern "C" fn(Handle, u32, u32) -> i32;
type AutoFan = unsafe extern "C" fn(Handle, u32) -> i32;
#[repr(C)]
struct Speed {
    version: u32,
    fan: u32,
    speed: u32,
}

// Optional entry points stay optional: older drivers may still provide readable
// temperatures. Resolve once per library lifetime, including missing functions.
struct Api {
    shutdown: Option<Init>,
    error: Option<unsafe extern "C" fn(i32) -> *const c_char>,
    count: Option<unsafe extern "C" fn(*mut u32) -> i32>,
    by_index: Option<unsafe extern "C" fn(u32, *mut Handle) -> i32>,
    by_uuid: Option<unsafe extern "C" fn(*const c_char, *mut Handle) -> i32>,
    uuid: Option<StringFn>,
    name: Option<StringFn>,
    num_fans: Option<Uint>,
    policy: Option<FanUint>,
    speed: Option<FanUint>,
    target: Option<FanUint>,
    temperature: Option<FanUint>,
    limits: Option<unsafe extern "C" fn(Handle, *mut u32, *mut u32) -> i32>,
    rpm: Option<unsafe extern "C" fn(Handle, *mut Speed) -> i32>,
    set: Option<SetFan>,
    auto: Option<AutoFan>,
}
fn load<T: Copy>(lib: &Library, name: &[u8]) -> Option<T> {
    // Each caller supplies the exact NVML signature. Nvml owns the Library for
    // as long as these copied function pointers can be invoked.
    unsafe { lib.get::<T>(name).ok().map(|f| *f) }
}
fn required<T: Copy>(function: Option<T>, name: &str) -> Result<T> {
    function.ok_or_else(|| conflict(format!("驱动接口不可用: {name}")))
}
pub struct Nvml {
    _lib: Library,
    api: Api,
    handles: HashMap<String, usize>,
    limits: HashMap<usize, (Instant, (u32, u32))>,
    batch_active: bool,
    batch_temperatures: HashMap<usize, (Instant, bool)>,
}
#[derive(Clone)]
pub struct Device {
    pub uuid: String,
    pub name: String,
    pub handle: usize,
}
impl Nvml {
    pub fn new() -> Result<Self> {
        let lib =
            unsafe { Library::new("libnvidia-ml.so.1") }.map_err(|e| conflict(e.to_string()))?;
        let init = required(load::<Init>(&lib, b"nvmlInit_v2\0"), "nvmlInit_v2")?;
        let api = Api {
            shutdown: load(&lib, b"nvmlShutdown\0"),
            error: load(&lib, b"nvmlErrorString\0"),
            count: load(&lib, b"nvmlDeviceGetCount_v2\0"),
            by_index: load(&lib, b"nvmlDeviceGetHandleByIndex_v2\0"),
            by_uuid: load(&lib, b"nvmlDeviceGetHandleByUUID\0"),
            uuid: load(&lib, b"nvmlDeviceGetUUID\0"),
            name: load(&lib, b"nvmlDeviceGetName\0"),
            num_fans: load(&lib, b"nvmlDeviceGetNumFans\0"),
            policy: load(&lib, b"nvmlDeviceGetFanControlPolicy_v2\0"),
            speed: load(&lib, b"nvmlDeviceGetFanSpeed_v2\0"),
            target: load(&lib, b"nvmlDeviceGetTargetFanSpeed\0"),
            temperature: load(&lib, b"nvmlDeviceGetTemperature\0"),
            limits: load(&lib, b"nvmlDeviceGetMinMaxFanSpeed\0"),
            rpm: load(&lib, b"nvmlDeviceGetFanSpeedRPM\0"),
            set: load(&lib, b"nvmlDeviceSetFanSpeed_v2\0"),
            auto: load(&lib, b"nvmlDeviceSetDefaultFanSpeed_v2\0"),
        };
        let n = Self {
            _lib: lib,
            api,
            handles: HashMap::new(),
            limits: HashMap::new(),
            batch_active: false,
            batch_temperatures: HashMap::new(),
        };
        n.check(unsafe { init() })?;
        Ok(n)
    }
    fn check(&self, code: i32) -> Result<()> {
        if code == 0 {
            return Ok(());
        }
        let message = unsafe {
            self.api.error.and_then(|f| {
                let pointer = f(code);
                (!pointer.is_null()).then(|| CStr::from_ptr(pointer).to_string_lossy().into_owned())
            })
        };
        Err(conflict(
            message.unwrap_or_else(|| format!("NVML 错误 {code}")),
        ))
    }
    pub fn uint(&self, name: &[u8], handle: usize) -> Result<u32> {
        let mut v = 0;
        unsafe {
            let f = match name {
                b"nvmlDeviceGetNumFans\0" => required(self.api.num_fans, "nvmlDeviceGetNumFans")?,
                _ => return Err(invalid("未知 NVML 数值接口")),
            };
            self.check(f(handle as Handle, &mut v))?;
        }
        Ok(v)
    }
    pub fn fan_uint(&self, name: &[u8], handle: usize, fan: u32) -> Result<u32> {
        let mut v = 0;
        unsafe {
            let function = match name {
                b"nvmlDeviceGetFanControlPolicy_v2\0" => self.api.policy,
                b"nvmlDeviceGetFanSpeed_v2\0" => self.api.speed,
                b"nvmlDeviceGetTargetFanSpeed\0" => self.api.target,
                b"nvmlDeviceGetTemperature\0" => self.api.temperature,
                _ => return Err(invalid("未知 NVML 风扇接口")),
            };
            let f = required(
                function,
                std::str::from_utf8(name)
                    .unwrap_or("NVML")
                    .trim_end_matches('\0'),
            )?;
            self.check(f(handle as Handle, fan, &mut v))?;
        }
        Ok(v)
    }
    fn string(&self, name: &[u8], handle: usize) -> Result<String> {
        let mut bytes = [0 as c_char; 128];
        unsafe {
            let function = match name {
                b"nvmlDeviceGetUUID\0" => self.api.uuid,
                b"nvmlDeviceGetName\0" => self.api.name,
                _ => return Err(invalid("未知 NVML 字符串接口")),
            };
            let f = required(
                function,
                std::str::from_utf8(name)
                    .unwrap_or("NVML")
                    .trim_end_matches('\0'),
            )?;
            self.check(f(handle as Handle, bytes.as_mut_ptr(), 128))?;
            bytes[127] = 0;
            Ok(CStr::from_ptr(bytes.as_ptr())
                .to_string_lossy()
                .into_owned())
        }
    }
    pub fn devices(&mut self) -> Result<Vec<Device>> {
        let mut count = 0;
        unsafe {
            let f = required(self.api.count, "nvmlDeviceGetCount_v2")?;
            self.check(f(&mut count))?;
        }
        let mut result = vec![];
        self.handles.clear();
        self.limits.clear();
        for i in 0..count {
            let mut h = std::ptr::null_mut();
            unsafe {
                let f = required(self.api.by_index, "nvmlDeviceGetHandleByIndex_v2")?;
                self.check(f(i, &mut h))?;
            }
            let uuid = self.string(b"nvmlDeviceGetUUID\0", h as usize)?;
            let name = self.string(b"nvmlDeviceGetName\0", h as usize)?;
            self.handles.insert(uuid.clone(), h as usize);
            result.push(Device {
                uuid,
                name,
                handle: h as usize,
            });
        }
        Ok(result)
    }
    pub fn handle(&mut self, uuid: &str) -> Result<usize> {
        if let Some(h) = self.handles.get(uuid) {
            return Ok(*h);
        }
        let name = CString::new(uuid).map_err(|_| invalid("无效 GPU UUID"))?;
        let mut h = std::ptr::null_mut();
        unsafe {
            let f = required(self.api.by_uuid, "nvmlDeviceGetHandleByUUID")?;
            self.check(f(name.as_ptr(), &mut h))?;
        }
        self.handles.insert(uuid.to_owned(), h as usize);
        Ok(h as usize)
    }
    pub fn limits(&mut self, h: usize) -> Result<(u32, u32)> {
        if let Some((at, v)) = self.limits.get(&h) {
            if at.elapsed() < Duration::from_secs(60) {
                return Ok(*v);
            }
        }
        let (mut min, mut max) = (0, 0);
        unsafe {
            let f = required(self.api.limits, "nvmlDeviceGetMinMaxFanSpeed")?;
            self.check(f(h as Handle, &mut min, &mut max))?;
        }
        if min > max || !(20..=100).contains(&max) {
            return Err(conflict("驱动报告的 NVIDIA 风扇范围无效"));
        }
        let v = (min.max(20), max);
        self.limits.insert(h, (Instant::now(), v));
        Ok(v)
    }
    pub fn temperature(&self, h: usize) -> Result<f64> {
        let v = self.fan_uint(b"nvmlDeviceGetTemperature\0", h, 0)?;
        if v > 150 {
            return Err(conflict("GPU 温度无效"));
        }
        Ok(v as f64)
    }
    pub fn rpm(&self, h: usize, fan: u32) -> Result<u32> {
        let mut s = Speed {
            version: std::mem::size_of::<Speed>() as u32 | (1 << 24),
            fan,
            speed: 0,
        };
        unsafe {
            let f = required(self.api.rpm, "nvmlDeviceGetFanSpeedRPM")?;
            self.check(f(h as Handle, &mut s))?;
        }
        Ok(s.speed)
    }
    pub fn original(&mut self, key: &str) -> Result<Value> {
        let (uuid, fan) = parse(key)?;
        let h = self.handle(uuid)?;
        if fan >= self.uint(b"nvmlDeviceGetNumFans\0", h)? {
            return Err(conflict("NVIDIA 风扇索引不存在"));
        }
        self.limits(h)?;
        required(self.api.set, "nvmlDeviceSetFanSpeed_v2")?;
        required(self.api.auto, "nvmlDeviceSetDefaultFanSpeed_v2")?;
        let policy = self.fan_uint(b"nvmlDeviceGetFanControlPolicy_v2\0", h, fan)?;
        if policy > 1 {
            return Err(conflict("未知 NVIDIA 控制策略"));
        }
        let target = if policy == 1 {
            Some(self.fan_uint(b"nvmlDeviceGetTargetFanSpeed\0", h, fan)?)
        } else {
            None
        };
        if target.is_some_and(|t| t > 100) {
            return Err(conflict("无法恢复原来的 NVIDIA 手动输出"));
        }
        Ok(json!({"channel_id":key,"policy":policy,"target":target}))
    }
    fn set(&self, h: usize, fan: u32, duty: u32) -> Result<()> {
        unsafe {
            let f = required(self.api.set, "nvmlDeviceSetFanSpeed_v2")?;
            self.check(f(h as Handle, fan, duty))
        }
    }
    fn auto(&self, h: usize, fan: u32) -> Result<()> {
        unsafe {
            let f = required(self.api.auto, "nvmlDeviceSetDefaultFanSpeed_v2")?;
            self.check(f(h as Handle, fan))
        }
    }
    pub fn begin_batch(&mut self) {
        self.batch_temperatures.clear();
        self.batch_active = true;
    }
    pub fn end_batch(&mut self) {
        self.batch_active = false;
        self.batch_temperatures.clear();
    }
    fn hot(&mut self, handle: usize) -> bool {
        // Never reuse parent telemetry or readings from an earlier request.
        // A slow batch rechecks temperature before writing subsequent fans.
        if self.batch_active {
            if let Some((at, hot)) = self.batch_temperatures.get(&handle) {
                if at.elapsed() < Duration::from_millis(50) {
                    return *hot;
                }
            }
        }
        let started = Instant::now();
        let hot = self.temperature(handle).map_or(true, |t| t >= 85.);
        if self.batch_active {
            self.batch_temperatures.insert(handle, (started, hot));
        }
        hot
    }
    pub fn write(&mut self, key: &str, duty: f64) -> Result<Value> {
        if !duty.is_finite() || !(0.0..=100.).contains(&duty) {
            return Err(invalid("无效输出"));
        }
        let (uuid, fan) = parse(key)?;
        let h = self.handle(uuid)?;
        let (min, max) = self.limits(h)?;
        let hot = self.hot(h);
        if duty == 0. && !hot {
            if self.fan_uint(b"nvmlDeviceGetFanControlPolicy_v2\0", h, fan)? != 0 {
                self.auto(h, fan)?;
            }
            return Ok(json!({"automatic":true,"duty":0,"fault":null}));
        }
        let output = if hot {
            max
        } else {
            duty.clamp(min as f64, max as f64).round_ties_even() as u32
        };
        // Always reassert active output; independent temperature check and lease
        // renewal remain mandatory even when the requested duty did not change.
        self.set(h, fan, output)?;
        Ok(
            json!({"automatic":false,"duty":output,"fault":if hot{Some("显卡自身温度异常，使用最大输出")}else{None}}),
        )
    }
    pub fn restore(&mut self, lease: &Value) -> Result<()> {
        let (uuid, fan) = parse(text(&lease["channel_id"]))?;
        // Recovery must resolve a fresh handle after a driver/device reset.
        self.handles.remove(uuid);
        let h = self.handle(uuid)?;
        if lease["policy"] == 0 {
            self.auto(h, fan)
        } else {
            self.set(
                h,
                fan,
                number(&lease["target"], 0., 100., "原目标输出")? as u32,
            )
        }
    }
}
impl Drop for Nvml {
    fn drop(&mut self) {
        unsafe {
            if let Some(f) = self.api.shutdown {
                f();
            }
        }
    }
}
pub fn parse(key: &str) -> Result<(&str, u32)> {
    let parts: Vec<_> = key.split('/').collect();
    if parts.len() != 3
        || parts[0] != "nvidia"
        || !parts[1].starts_with("GPU-")
        || parts[1].len() <= 4
        || !parts[1][4..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-')
    {
        return Err(invalid("无效 NVIDIA 风扇通道"));
    }
    let fan = parts[2]
        .strip_prefix("fan")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid("无效 NVIDIA 风扇索引"))?;
    Ok((parts[1], fan))
}
