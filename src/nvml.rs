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
pub struct Nvml {
    lib: Library,
    handles: HashMap<String, usize>,
    limits: HashMap<usize, (Instant, (u32, u32))>,
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
        let n = Self {
            lib,
            handles: HashMap::new(),
            limits: HashMap::new(),
        };
        unsafe {
            let f = n.sym::<unsafe extern "C" fn() -> i32>(b"nvmlInit_v2\0")?;
            n.check(f())?;
        }
        Ok(n)
    }
    unsafe fn sym<T>(&self, name: &[u8]) -> Result<libloading::Symbol<'_, T>> {
        self.lib
            .get(name)
            .map_err(|e| conflict(format!("驱动接口不可用: {e}")))
    }
    fn check(&self, code: i32) -> Result<()> {
        if code == 0 {
            return Ok(());
        }
        let message = unsafe {
            self.sym::<unsafe extern "C" fn(i32) -> *const c_char>(b"nvmlErrorString\0")
                .ok()
                .map(|f| CStr::from_ptr(f(code)).to_string_lossy().into_owned())
        };
        Err(conflict(
            message.unwrap_or_else(|| format!("NVML 错误 {code}")),
        ))
    }
    pub fn uint(&self, name: &[u8], handle: usize) -> Result<u32> {
        let mut v = 0;
        unsafe {
            let f = self.sym::<unsafe extern "C" fn(Handle, *mut u32) -> i32>(name)?;
            self.check(f(handle as Handle, &mut v))?;
        }
        Ok(v)
    }
    pub fn fan_uint(&self, name: &[u8], handle: usize, fan: u32) -> Result<u32> {
        let mut v = 0;
        unsafe {
            let f = self.sym::<unsafe extern "C" fn(Handle, u32, *mut u32) -> i32>(name)?;
            self.check(f(handle as Handle, fan, &mut v))?;
        }
        Ok(v)
    }
    fn string(&self, name: &[u8], handle: usize) -> Result<String> {
        let mut bytes = [0 as c_char; 128];
        unsafe {
            let f = self.sym::<unsafe extern "C" fn(Handle, *mut c_char, u32) -> i32>(name)?;
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
            let f =
                self.sym::<unsafe extern "C" fn(*mut u32) -> i32>(b"nvmlDeviceGetCount_v2\0")?;
            self.check(f(&mut count))?;
        }
        let mut result = vec![];
        self.handles.clear();
        self.limits.clear();
        for i in 0..count {
            let mut h = std::ptr::null_mut();
            unsafe {
                let f = self.sym::<unsafe extern "C" fn(u32, *mut Handle) -> i32>(
                    b"nvmlDeviceGetHandleByIndex_v2\0",
                )?;
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
            let f = self.sym::<unsafe extern "C" fn(*const c_char, *mut Handle) -> i32>(
                b"nvmlDeviceGetHandleByUUID\0",
            )?;
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
            let f = self.sym::<unsafe extern "C" fn(Handle, *mut u32, *mut u32) -> i32>(
                b"nvmlDeviceGetMinMaxFanSpeed\0",
            )?;
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
        #[repr(C)]
        struct Speed {
            version: u32,
            fan: u32,
            speed: u32,
        }
        let mut s = Speed {
            version: std::mem::size_of::<Speed>() as u32 | (1 << 24),
            fan,
            speed: 0,
        };
        unsafe {
            let f = self.sym::<unsafe extern "C" fn(Handle, *mut Speed) -> i32>(
                b"nvmlDeviceGetFanSpeedRPM\0",
            )?;
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
        unsafe {
            self.sym::<unsafe extern "C" fn(Handle, u32, u32) -> i32>(
                b"nvmlDeviceSetFanSpeed_v2\0",
            )?;
            self.sym::<unsafe extern "C" fn(Handle, u32) -> i32>(
                b"nvmlDeviceSetDefaultFanSpeed_v2\0",
            )?;
        }
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
            let f = self.sym::<unsafe extern "C" fn(Handle, u32, u32) -> i32>(
                b"nvmlDeviceSetFanSpeed_v2\0",
            )?;
            self.check(f(h as Handle, fan, duty))
        }
    }
    fn auto(&self, h: usize, fan: u32) -> Result<()> {
        unsafe {
            let f = self.sym::<unsafe extern "C" fn(Handle, u32) -> i32>(
                b"nvmlDeviceSetDefaultFanSpeed_v2\0",
            )?;
            self.check(f(h as Handle, fan))
        }
    }
    pub fn write(&mut self, key: &str, duty: f64) -> Result<Value> {
        if !duty.is_finite() || !(0.0..=100.).contains(&duty) {
            return Err(invalid("无效输出"));
        }
        let (uuid, fan) = parse(key)?;
        let h = self.handle(uuid)?;
        let (min, max) = self.limits(h)?;
        let hot = self.temperature(h).map_or(true, |t| t >= 85.);
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
            if let Ok(f) = self.sym::<unsafe extern "C" fn() -> i32>(b"nvmlShutdown\0") {
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
