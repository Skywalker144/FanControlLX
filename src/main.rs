mod common;
mod config;
mod engine;
mod hardware;
mod install;
mod lock;
mod nvml;
#[cfg(test)]
mod perf_tests;
mod runtime;
mod server;
mod state;
mod telemetry;
#[cfg(test)]
mod tests;
mod writer;
use common::*;
use hardware::Hardware;
use serde_json::json;
use std::{
    net::{IpAddr, TcpListener},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn cpu_time() -> f64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut t);
    }
    t.tv_sec as f64 + t.tv_nsec as f64 / 1e9
}
fn run() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--install-service") {
        return install::run(std::env::args().skip(2));
    }
    let mut host = "127.0.0.1".to_owned();
    let mut port = 8090u16;
    let mut data = std::env::current_dir()?.join("data");
    let (mut service, mut control, mut demo, mut diagnose, mut recover, mut gpu_writer) =
        (false, false, false, false, false, false);
    let mut fd = None;
    let mut benchmark = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--host" => host = args.next().ok_or_else(|| invalid("--host 缺少参数"))?,
            "--port" => {
                port = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .filter(|p| *p > 0)
                    .ok_or_else(|| invalid("--port 必须为 1–65535"))?
            }
            "--data-dir" => {
                data = PathBuf::from(args.next().ok_or_else(|| invalid("--data-dir 缺少参数"))?)
            }
            "--service" => {
                service = true;
                control = true;
            }
            "--control" => control = true,
            "--demo" => demo = true,
            "--diagnose" => diagnose = true,
            "--recover" => recover = true,
            "--gpu-writer" => gpu_writer = true,
            "--fd" => {
                fd = Some(
                    args.next()
                        .and_then(|s| s.parse::<i32>().ok())
                        .filter(|fd| *fd >= 3)
                        .ok_or_else(|| invalid("--fd 无效"))?,
                )
            }
            "--benchmark" => {
                benchmark = Some(
                    args.next()
                        .and_then(|s| s.parse::<u32>().ok())
                        .filter(|n| (1..=3600).contains(n))
                        .ok_or_else(|| invalid("--benchmark 需要 1–3600 个样本"))?,
                )
            }
            "--version" => {
                println!("FanControlLX {} (Rust)", server::VERSION);
                return Ok(());
            }
            "--help" | "-h" => {
                println!("FanControlLX {}\n--service  后台持久控制\n--control  临时允许控制\n--demo     隔离演示\n--host 127.0.0.1 --port 8090 --data-dir ./data\n--diagnose 只读诊断\n--benchmark N 只读采样 N 秒\n--recover  恢复遗留 NVIDIA 租约",server::VERSION);
                return Ok(());
            }
            _ => return Err(invalid(format!("未知参数：{arg}"))),
        }
    }
    if !data.is_absolute() {
        data = std::env::current_dir()?.join(data);
    }
    let stop = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(sig, stop.clone())?;
    }
    if gpu_writer || recover {
        if gpu_writer && fd.is_none() {
            return Err(invalid("GPU 守护进程需要父进程连接"));
        }
        return writer::run(&data, if recover { None } else { fd }, stop);
    }
    let mut hardware: Box<dyn Hardware> = if demo {
        Box::new(hardware::Demo::new())
    } else {
        Box::new(hardware::Real::new(
            if control && !diagnose && benchmark.is_none() {
                Some(data.clone())
            } else {
                None
            },
        ))
    };
    if diagnose || benchmark.is_some() {
        if let Some(count) = benchmark {
            let mut snapshot = hardware.sample(true)?;
            let start = Instant::now();
            let cpu = cpu_time();
            let mut durations = vec![];
            for _ in 0..count {
                let t = Instant::now();
                hardware.sample_into(false, &mut snapshot)?;
                durations.push(t.elapsed().as_secs_f64() * 1000.);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            println!(
                "{}",
                json!({"runtime":"rust","samples":durations.len(),"elapsed_s":start.elapsed().as_secs_f64(),"cpu_s":cpu_time()-cpu,"sample_wall_ms":durations,"process_status":std::fs::read_to_string("/proc/self/status").unwrap_or_default().lines().filter(|s|s.starts_with("VmRSS:")||s.starts_with("VmHWM:")).collect::<Vec<_>>()})
            );
        } else {
            let s = hardware.sample(true)?;
            let (gpu_error, gpu_control_error) = hardware.errors();
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"board":std::fs::read_to_string("/sys/class/dmi/id/board_name").unwrap_or_default().trim(),
                        "kernel":std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim(),
                        "uid":unsafe{libc::geteuid()},
                        "sensors":s.sensors,"fans":s.fans,"channels":s.channels,"chips":s.chips,"gpu_error":gpu_error,"gpu_control_error":gpu_control_error,"runtime":"rust"})
                )?
            );
        }
        return Ok(());
    }
    let ip: IpAddr = host.parse().map_err(|_| invalid("--host 必须为回环 IP"))?;
    if !ip.is_loopback() {
        return Err(invalid("仅监听回环地址，请使用 ssh -L 转发"));
    }
    std::fs::create_dir_all(&data)?;
    let _instance = lock::take(&data.join(if demo { "demo.lock" } else { "server.lock" }))?;
    let _hardware = if control && !demo {
        let lock = lock::take(&PathBuf::from("/tmp/fancontrollx-hardware.lock"))?;
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_digit())
            {
                let name = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
                if ["coolercontrold", "fancontrol", "fan2go", "thinkfan"].contains(&name.trim()) {
                    return Err(conflict("检测到其他风扇控制进程"));
                }
            }
        }
        Some(lock)
    } else {
        None
    };
    // Bind before acquiring any controls. Startup failure never leaves ownership.
    let listener = TcpListener::bind((ip, port))?;
    let mut engine = engine::Engine::new(
        hardware,
        data.join(if demo {
            "config.demo.json"
        } else {
            "config.json"
        }),
        control,
        demo,
        service,
    )?;
    engine.resume();
    let (mut controller, client) = runtime::channel(&engine);
    if let Err(e) = server::start(listener, client) {
        let _ = engine.shutdown();
        return Err(e);
    }
    println!(
        "FanControlLX {} · Rust · http://{}:{} · {}",
        server::VERSION,
        if ip.is_ipv6() {
            format!("[{ip}]")
        } else {
            ip.to_string()
        },
        port,
        data.display()
    );
    let mut next = Instant::now() + Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        if Instant::now() >= next {
            controller.invalidate();
            engine.cycle();
            controller.changed(&engine);
            next = Instant::now() + Duration::from_secs(1);
        }
        controller.serve_one(&mut engine, next.saturating_duration_since(Instant::now()));
    }
    controller.invalidate();
    let mut last = None;
    for _ in 0..10 {
        match engine.shutdown() {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(last.unwrap_or_else(|| conflict("停止恢复失败")))
}
fn main() {
    if let Err(e) = run() {
        eprintln!("FanControlLX: {e}");
        std::process::exit(1);
    }
}
