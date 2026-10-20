//! One-shot installer. No hardware sampling or control is started here.
use crate::{common::*, config, lock};
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const TEMPLATE: &str = include_str!("../systemd/fancontrollx.service.in");
const UNIT: &str = "/etc/systemd/system/fancontrollx.service";

fn quoted(path: &Path, command: bool) -> Result<String> {
    let s = path
        .to_str()
        .ok_or_else(|| invalid("安装路径必须为 UTF-8"))?;
    if s.chars().any(char::is_control) {
        return Err(invalid("安装路径不能包含控制字符"));
    }
    let mut s = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    if command {
        s = s.replace('$', "$$");
    }
    Ok(format!("\"{s}\""))
}
pub fn render(root: &Path, driver: bool) -> Result<String> {
    // Unlike ExecStart/RequiresMountsFor, WorkingDirectory takes the complete
    // unquoted value; surrounding quotes would become part of the path.
    quoted(root, false)?;
    let working = root.to_str().unwrap().replace('%', "%%");
    Ok(TEMPLATE
        .replace(
            "@DRIVER_PRE@",
            if driver {
                "ExecStartPre=/usr/sbin/modprobe nct6687 msi_fan_brute_force=1"
            } else {
                ""
            },
        )
        .replace("@PROJECT_DIR@", &quoted(root, false)?)
        .replace("@WORKING_DIR@", &working)
        .replace("@DATA_DIR@", &quoted(&root.join("data"), true)?)
        .replace(
            "@BINARY@",
            &quoted(&root.join("target/release/fancontrollx"), true)?,
        ))
}
pub fn prepare(raw: &Value, enable: bool) -> Result<Value> {
    let mut v = config::validate(raw)?;
    if enable {
        for f in v["fans"].as_array_mut().unwrap() {
            f["autostart"] = Value::Bool(
                yes(&f["identified"])
                    && !text(&f["channel_id"]).is_empty()
                    && ["fan", "pump"].contains(&text(&f["device_kind"])),
            );
        }
    }
    Ok(v)
}
trait Commands {
    fn output(&mut self, program: &Path, args: &[&str]) -> Result<String>;
}
struct System;
impl Commands for System {
    fn output(&mut self, program: &Path, args: &[&str]) -> Result<String> {
        let o = Command::new(program).args(args).output()?;
        if !o.status.success() {
            return Err(conflict(format!(
                "{}：{}",
                program.display(),
                String::from_utf8_lossy(&o.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
    }
}
fn command(c: &mut impl Commands, program: &str, args: &[&str]) -> Result<String> {
    c.output(Path::new(program), args)
}
struct Plan {
    root: PathBuf,
    module: PathBuf,
    target: PathBuf,
    unit: PathBuf,
    content: String,
    enable: bool,
    driver: bool,
}
impl Plan {
    fn preflight(
        root: &Path,
        kernel: &str,
        enable: bool,
        driver: bool,
        c: &mut impl Commands,
    ) -> Result<Self> {
        let binary = root.join("target/release/fancontrollx");
        if !binary.is_file() || fs::metadata(&binary)?.permissions().mode() & 0o111 == 0 {
            return Err(invalid("请先运行 ./build.sh 构建 Rust 后台"));
        }
        c.output(&binary, &["--version"])?;
        let raw: Value = serde_json::from_slice(&fs::read(root.join("data/config.json"))?)?;
        let v = prepare(&raw, enable)?;
        let module = root.join(".runtime/nct6687d/nct6687.signed.ko");
        if driver {
            if !module.is_file() {
                return Err(invalid("未找到已签名驱动；沿用系统驱动请加 --skip-driver"));
            }
            let path = module.to_str().ok_or_else(|| invalid("驱动路径无效"))?;
            let vermagic = command(c, "/usr/sbin/modinfo", &["-F", "vermagic", path])?;
            if vermagic.split_whitespace().next() != Some(kernel) {
                return Err(conflict("驱动与当前内核不匹配，请重新编译并签名"));
            }
            if command(c, "/usr/sbin/modinfo", &["-F", "signer", path])?.is_empty() {
                return Err(conflict("驱动没有签名，请先完成签名和证书登记"));
            }
        }
        let names: Vec<_> = array(&v["fans"])
            .iter()
            .filter(|f| {
                yes(&f["autostart"]) && yes(&f["identified"]) && !text(&f["channel_id"]).is_empty()
            })
            .map(|f| text(&f["name"]))
            .collect();
        println!(
            "后台自动控制：{}",
            if names.is_empty() {
                "无".into()
            } else {
                names.join("、")
            }
        );
        Ok(Self {
            root: root.into(),
            module,
            target: PathBuf::from(format!(
                "/lib/modules/{kernel}/updates/fancontrollx/nct6687.ko"
            )),
            unit: PathBuf::from(UNIT),
            content: render(root, driver)?,
            enable,
            driver,
        })
    }
    fn apply(&self, c: &mut impl Commands) -> Result<()> {
        let active = command(
            c,
            "/usr/bin/systemctl",
            &["is-active", "--quiet", "fancontrollx.service"],
        )
        .is_ok();
        if active {
            command(c, "/usr/bin/systemctl", &["stop", "fancontrollx.service"])?;
        }
        {
            // Never kill an unrelated foreground controller to obtain its lock.
            let _lock = lock::take(&self.root.join("data/server.lock"))?;
            let path = self.root.join("data/config.json");
            let raw = fs::read(&path)?;
            let v = prepare(&serde_json::from_slice(&raw)?, self.enable)?;
            let suffix = token()?;
            let backup = path.with_file_name(format!("config.before-service-{suffix}.json"));
            config::atomic(&backup, &raw)?;
            if self.unit.exists() {
                fs::copy(
                    &self.unit,
                    self.unit
                        .with_file_name(format!("fancontrollx.service.backup-{suffix}")),
                )?;
            }
            if self.driver {
                fs::create_dir_all(self.target.parent().unwrap())?;
                fs::copy(&self.module, &self.target)?;
                fs::set_permissions(&self.target, fs::Permissions::from_mode(0o644))?;
                command(c, "/usr/sbin/depmod", &["-a"])?;
            }
            config::save(&path, &v)?;
            config::atomic(&self.unit, self.content.as_bytes())?;
            fs::set_permissions(&self.unit, fs::Permissions::from_mode(0o644))?;
            println!("原配置已备份：{}", backup.display());
        }
        command(c, "/usr/bin/systemctl", &["daemon-reload"])?;
        command(
            c,
            "/usr/bin/systemctl",
            &["enable", "--now", "fancontrollx.service"],
        )?;
        Ok(())
    }
}
fn state() -> Result<Value> {
    let timeout = Duration::from_secs(2);
    let mut s = TcpStream::connect_timeout(&"127.0.0.1:8090".parse().unwrap(), timeout)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    s.write_all(b"GET /api/state HTTP/1.1\r\nHost: 127.0.0.1:8090\r\nConnection: close\r\n\r\n")?;
    let mut response = String::new();
    s.take(1024 * 1024).read_to_string(&mut response)?;
    let (header, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| conflict("服务接口响应无效"))?;
    if !header.starts_with("HTTP/1.1 200 ") {
        return Err(conflict("服务接口尚未就绪"));
    }
    Ok(serde_json::from_str(body)?)
}
pub fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let (mut dry, mut enable, mut driver) = (false, false, true);
    let mut root = None;
    let mut args = args;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--project-dir" => {
                root = Some(PathBuf::from(
                    args.next().ok_or_else(|| invalid("缺少项目目录"))?,
                ))
            }
            "--dry-run" => dry = true,
            "--enable-confirmed" => enable = true,
            "--skip-driver" => driver = false,
            "--help" | "-h" => {
                println!("安装 FanControlLX：--dry-run 仅预览；--skip-driver 沿用系统驱动；--enable-confirmed 启用已确认设备。默认保留控制开关。");
                return Ok(());
            }
            _ => return Err(invalid(format!("未知安装参数：{a}"))),
        }
    }
    let root = root
        .ok_or_else(|| invalid("请通过 ./install-service.sh 安装"))?
        .canonicalize()?;
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease")?;
    let plan = Plan::preflight(&root, kernel.trim(), enable, driver, &mut System)?;
    if dry {
        println!(
            "服务配置预览：\n{}\n仅预览，未修改系统、配置或风扇。",
            plan.content
        );
        return Ok(());
    }
    if unsafe { libc::geteuid() } != 0 {
        return Err(conflict("请使用 sudo ./install-service.sh 执行安装"));
    }
    if !Path::new("/run/systemd/system").is_dir() {
        return Err(conflict("当前环境未运行 systemd"));
    }
    plan.apply(&mut System)?;
    for _ in 0..30 {
        if let Ok(s) = state() {
            if yes(&s["background_service"])
                && s["runtime"] == "rust"
                && s["version"] == crate::server::VERSION
            {
                let count = s["outputs"]
                    .as_object()
                    .map(|o| o.values().filter(|v| yes(&v["active"])).count())
                    .unwrap_or(0);
                println!("后台服务已启动并设为开机自启，当前已接管 {count} 路。\nWebUI：http://127.0.0.1:8090");
                if s["pending_autostart"]
                    .as_object()
                    .is_some_and(|v| !v.is_empty())
                {
                    println!("待接管通道：{}", s["pending_autostart"]);
                }
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(conflict(
        "服务接口尚未就绪，请执行 sudo journalctl -u fancontrollx -n 50 --no-pager",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Temp;
    use serde_json::json;
    #[derive(Default)]
    struct Recorder {
        calls: Vec<String>,
        fail: Option<String>,
    }
    impl Commands for Recorder {
        fn output(&mut self, p: &Path, args: &[&str]) -> Result<String> {
            let s = format!("{} {}", p.display(), args.join(" "));
            self.calls.push(s.clone());
            if self.fail.as_ref().is_some_and(|f| s.contains(f)) {
                return Err(conflict("injected failure"));
            }
            Ok(if args.contains(&"vermagic") {
                "test-kernel SMP"
            } else {
                "signed"
            }
            .into())
        }
    }
    fn fixture() -> Temp {
        let t = Temp::new();
        fs::create_dir_all(t.0.join("target/release")).unwrap();
        let binary = t.0.join("target/release/fancontrollx");
        fs::write(&binary, "fixture").unwrap();
        fs::set_permissions(binary, fs::Permissions::from_mode(0o755)).unwrap();
        config::save(&t.0.join("data/config.json"), &config::default_config()).unwrap();
        t
    }
    #[test]
    fn flags_preserve_curves_and_only_enable_confirmed_bindings() {
        let mut a = config::default_fan("a", "A", "@cpu");
        a["identified"] = json!(true);
        a["channel_id"] = json!("chip/pwm1");
        a["device_kind"] = json!("fan");
        let b = config::default_fan("b", "B", "@gpu");
        let raw = json!({"version":1,"fans":[a,b]});
        let v = prepare(&raw, true).unwrap();
        assert_eq!(v["fans"][0]["points"], raw["fans"][0]["points"]);
        assert!(yes(&v["fans"][0]["autostart"]));
        assert!(!yes(&v["fans"][1]["autostart"]));
        assert!(!yes(&prepare(&raw, false).unwrap()["fans"][0]["autostart"]));
    }
    #[test]
    fn unit_quoting_recovery_and_optional_driver() {
        let unit = render(Path::new("/tmp/a b%$\""), false).unwrap();
        assert!(unit
            .contains("ExecStopPost=\"/tmp/a b%%$$\\\"/target/release/fancontrollx\" --recover"));
        assert!(!unit.contains("modprobe"));
        assert!(!unit.contains("PYTHON"));
        assert!(unit.contains("WorkingDirectory=/tmp/a b%%$\"\n"));
        assert!(render(Path::new("/tmp/app"), true)
            .unwrap()
            .contains("modprobe"));
        assert!(render(Path::new("/tmp/a\nb"), false).is_err());
    }
    #[test]
    fn preflight_failures_never_stop_service() {
        let t = fixture();
        let mut c = Recorder::default();
        assert!(Plan::preflight(&t.0, "test-kernel", false, true, &mut c).is_err());
        fs::create_dir_all(t.0.join(".runtime/nct6687d")).unwrap();
        fs::write(t.0.join(".runtime/nct6687d/nct6687.signed.ko"), "").unwrap();
        assert!(Plan::preflight(&t.0, "wrong-kernel", false, true, &mut c).is_err());
        c.fail = Some("signer".into());
        assert!(Plan::preflight(&t.0, "test-kernel", false, true, &mut c).is_err());
        fs::remove_file(t.0.join("target/release/fancontrollx")).unwrap();
        assert!(Plan::preflight(&t.0, "test-kernel", false, false, &mut c).is_err());
        assert!(c.calls.iter().all(|s| !s.contains("systemctl")));
    }
    #[test]
    fn install_backs_up_exact_config_and_preserves_policy() {
        let t = fixture();
        let original = b"{\"version\":1,\"fans\":[]}  \n";
        fs::write(t.0.join("data/config.json"), original).unwrap();
        let mut c = Recorder::default();
        let mut plan = Plan::preflight(&t.0, "test-kernel", false, false, &mut c).unwrap();
        plan.unit = t.0.join("fancontrollx.service");
        fs::write(&plan.unit, "old unit").unwrap();
        plan.apply(&mut c).unwrap();
        let backups: Vec<_> = fs::read_dir(t.0.join("data"))
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("config.before-service-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(backups[0].path()).unwrap(), original);
        assert_eq!(
            fs::metadata(&plan.unit).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(c.calls.last().unwrap().contains("enable --now"));
        assert!(c.calls.iter().all(|s| !s.contains("depmod")));
    }
    #[test]
    fn foreground_lock_prevents_installation_writes() {
        let t = fixture();
        let mut c = Recorder::default();
        let mut plan = Plan::preflight(&t.0, "test-kernel", false, false, &mut c).unwrap();
        plan.unit = t.0.join("fancontrollx.service");
        let _lock = lock::take(&t.0.join("data/server.lock")).unwrap();
        assert!(plan.apply(&mut c).is_err());
        assert!(!plan.unit.exists());
        assert!(c.calls.iter().all(|s| !s.contains("daemon-reload")));
    }
}
