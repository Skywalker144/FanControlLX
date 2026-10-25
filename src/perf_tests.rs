//! Opt-in, deterministic hot-path measurements; never accesses real hardware.
//! cargo test --release --locked benchmark_ -- --ignored --nocapture --test-threads=1
use crate::{
    config,
    engine::Engine,
    hardware::{Demo, Hardware},
    state::Query,
    tests::Temp,
};
use serde_json::json;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    time::Instant,
};

struct CountingAllocator;
thread_local! {
    static COUNTS: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
}
fn count(bytes: usize) {
    let _ = COUNTS.try_with(|c| {
        if let Some((calls, total)) = c.get() {
            c.set(Some((calls + 1, total + bytes as u64)));
        }
    });
}
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count(size);
        System.realloc(ptr, layout, size)
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

pub(crate) fn measure(label: &str, iterations: usize, mut run: impl FnMut()) {
    for _ in 0..200 {
        run();
    }
    COUNTS.with(|c| c.set(Some((0, 0))));
    let start = Instant::now();
    for _ in 0..iterations {
        run();
    }
    let elapsed = start.elapsed().as_secs_f64();
    let (allocations, bytes) = COUNTS.with(|c| c.replace(None).unwrap());
    println!(
        "{}",
        json!({"scenario":label,"iterations":iterations,
        "ns_per_iteration":elapsed * 1e9 / iterations as f64,
        "allocations_per_iteration":allocations as f64 / iterations as f64,
        "allocated_bytes_per_iteration":bytes as f64 / iterations as f64})
    );
}

#[test]
#[ignore = "opt-in synthetic performance measurement"]
fn benchmark_control_cycles() {
    for count in [3, 64] {
        let temp = Temp::new();
        let path = temp.0.join("config.json");
        let curves: Vec<_> = (0..128)
            .map(|i| {
                json!({"id":format!("c{i}"),"name":format!("Curve {i}"),
            "points":(0..32).map(|p|json!([p*3,(p*4).min(100)])).collect::<Vec<_>>()})
            })
            .collect();
        let devices = Demo::new().sample(true).unwrap();
        let fans: Vec<_> = (0..count)
            .map(|i| {
                let mut fan = config::default_fan(&format!("f{i}"), "Fan", "@cpu-gpu");
                fan["curve_id"] = json!(format!("c{i}"));
                fan["smoothing_s"] = json!(0);
                if let Some(channel) = devices.channels.get(i) {
                    fan["channel_id"] = channel["id"].clone();
                }
                fan
            })
            .collect();
        config::save(
            &path,
            &config::validate(&json!({"version":2,"fans":fans,"curves":curves})).unwrap(),
        )
        .unwrap();
        let mut engine = Engine::new(Box::new(Demo::new()), path, false, true, false).unwrap();
        assert_eq!(engine.config["fans"].as_array().unwrap().len(), count);
        measure(
            &format!("demo_control_{count}_configured_fans"),
            10000,
            || engine.tick().unwrap(),
        );
        let full = engine.state(None, None);
        let revision = full["config_revision"].as_str().unwrap();
        let view = engine.view_state(None, None);
        let view_revision = view["view_revision"].as_str().unwrap();
        let cursor = view["history_cursor"].as_u64().unwrap();
        // Measure snapshot capture and encoding together. Requests in the same
        // production generation share a snapshot.
        for delta in [false, true] {
            let editor = Query::Editor {
                since: delta.then_some(revision),
                after: delta.then_some(f64::MAX),
            };
            let viewer = Query::Viewer {
                since: delta.then_some(view_revision),
                after: delta.then_some(cursor),
            };
            for (kind, query) in [("editor", editor), ("viewer", viewer)] {
                measure(
                    &format!("{kind}_encode_{count}_fans_delta_{delta}"),
                    1000,
                    || {
                        std::hint::black_box(
                            serde_json::to_vec(
                                &engine.read_snapshot(kind == "editor").response(query, None),
                            )
                            .unwrap(),
                        );
                    },
                );
                let snapshot = engine.read_snapshot(kind == "editor");
                measure(
                    &format!("{kind}_shared_encode_{count}_fans_delta_{delta}"),
                    1000,
                    || {
                        std::hint::black_box(
                            serde_json::to_vec(&snapshot.response(query, None)).unwrap(),
                        );
                    },
                );
            }
        }
        measure(&format!("snapshot_capture_{count}_fans"), 2000, || {
            std::hint::black_box(engine.read_snapshot(true));
        });
        let mut config = (*engine.config).clone();
        for fan in config["fans"].as_array_mut().unwrap() {
            fan["smoothing_s"] = json!(60);
        }
        engine.update(&config, None).unwrap();
        // Isolate policy calculation from Demo's allocation-heavy sampler.
        // Advance deterministic time so the smoothing window stays full.
        let mut now = engine.now();
        measure(&format!("policy_60s_smoothing_{count}_fans"), 10000, || {
            now += 0.5;
            let sample = std::mem::take(&mut engine.snapshot);
            engine.tick_with(sample, now).unwrap();
        });
    }
}
