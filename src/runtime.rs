//! One owner for hardware/control state. HTTP workers exchange bounded commands;
//! they never hold a control lock or serialize JSON on the control thread.
use crate::{
    common::*,
    engine::Engine,
    state::{Metadata, Query, ReadSnapshot},
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, SyncSender},
    Arc,
};
use std::time::{Duration, Instant};

pub enum Operation {
    State {
        since: Option<String>,
        after: Option<f64>,
    },
    View {
        since: Option<String>,
        after: Option<u64>,
    },
    Write {
        path: String,
        body: Value,
    },
}

struct Command {
    operation: Operation,
    expires: Instant,
    reply: SyncSender<Result<Response>>,
}

pub enum Response {
    State {
        snapshot: Arc<ReadSnapshot>,
        since: Option<String>,
        after: Option<f64>,
    },
    View {
        snapshot: Arc<ReadSnapshot>,
        since: Option<String>,
        after: Option<u64>,
    },
    Write(Value),
}
impl Response {
    pub fn encode(&self, metadata: Metadata<'_>) -> Result<Vec<u8>> {
        Ok(match self {
            Self::State {
                snapshot,
                since,
                after,
            } => serde_json::to_vec(&snapshot.response(
                Query::Editor {
                    since: since.as_deref(),
                    after: *after,
                },
                Some(metadata),
            ))?,
            Self::View {
                snapshot,
                since,
                after,
            } => serde_json::to_vec(&snapshot.response(
                Query::Viewer {
                    since: since.as_deref(),
                    after: *after,
                },
                Some(metadata),
            ))?,
            Self::Write(value) => serde_json::to_vec(value)?,
        })
    }
}

#[derive(Clone)]
pub struct Client {
    sender: SyncSender<Command>,
    generation: Arc<AtomicU64>,
    healthy: Arc<AtomicBool>,
    managed: bool,
}

pub struct Controller {
    receiver: Receiver<Command>,
    client: Client,
    snapshot: Option<Arc<ReadSnapshot>>,
}

pub fn channel(engine: &Engine) -> (Controller, Client) {
    let (sender, receiver) = mpsc::sync_channel(8);
    let client = Client {
        sender,
        generation: Arc::new(AtomicU64::new(0)),
        healthy: Arc::new(AtomicBool::new(engine.error.is_none())),
        managed: engine.managed,
    };
    (
        Controller {
            receiver,
            client: client.clone(),
            snapshot: None,
        },
        client,
    )
}

impl Client {
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn health(&self) -> Value {
        json!({"ok":self.healthy.load(Ordering::Acquire),"version":crate::server::VERSION,
            "background_service":self.managed,"runtime":"rust"})
    }

    pub fn call(&self, operation: Operation) -> Result<Response> {
        let (reply, receiver) = mpsc::sync_channel(1);
        let timeout = Duration::from_secs(10);
        self.sender
            .try_send(Command {
                operation,
                expires: Instant::now() + timeout,
                reply,
            })
            .map_err(|_| Error(503, "控制请求繁忙或服务正在停止，请重试".into()))?;
        receiver
            .recv_timeout(timeout)
            .map_err(|_| Error(503, "控制请求超时，请刷新状态后重试".into()))?
    }
}

impl Controller {
    pub fn invalidate(&mut self) {
        self.snapshot = None;
        self.client.generation.fetch_add(1, Ordering::Release);
    }

    pub fn changed(&mut self, engine: &Engine) {
        self.client
            .healthy
            .store(engine.error.is_none(), Ordering::Release);
        self.invalidate();
    }

    // Main checks its control deadline before every command. A stream of HTTP
    // reads therefore cannot starve a due control cycle.
    pub fn serve_one(&mut self, engine: &mut Engine, wait: Duration) {
        let Ok(command) = self.receiver.recv_timeout(wait) else {
            return;
        };
        if Instant::now() >= command.expires {
            let _ = command
                .reply
                .send(Err(Error(503, "控制请求已过期，请重试".into())));
            return;
        }
        let result = match command.operation {
            Operation::State { since, after } => Response::State {
                snapshot: self.snapshot(engine, true),
                since,
                after,
            },
            Operation::View { since, after } => Response::View {
                snapshot: self.snapshot(engine, false),
                since,
                after,
            },
            Operation::Write { path, body } => {
                if path != "/api/editor-heartbeat" {
                    // Do not keep serving old readings if a disk/driver call
                    // blocks while this operation is in progress.
                    self.invalidate();
                }
                let result = engine.request(&path, &body);
                // A failed write may still change recovery state. Heartbeats
                // only extend a discovery lease and do not change API payloads.
                if path != "/api/editor-heartbeat" {
                    self.changed(engine);
                }
                let _ = command.reply.send(result.map(Response::Write));
                return;
            }
        };
        let _ = command.reply.send(Ok(result));
    }

    fn snapshot(&mut self, engine: &Engine, editor: bool) -> Arc<ReadSnapshot> {
        // No readers means no snapshot work. All queries in a control generation
        // share one immutable sample, including the already shared configuration.
        if editor && self.snapshot.as_ref().is_some_and(|s| s.editor.is_none()) {
            self.snapshot = None;
        }
        self.snapshot
            .get_or_insert_with(|| Arc::new(engine.read_snapshot(editor)))
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hardware::Demo, tests::Temp};

    #[test]
    fn expired_command_cannot_change_configuration() {
        let temp = Temp::new();
        let mut engine = Engine::new(
            Box::new(Demo::new()),
            temp.0.join("config.json"),
            true,
            true,
            false,
        )
        .unwrap();
        let before = engine.config.clone();
        let (mut controller, client) = channel(&engine);
        let (reply, receiver) = mpsc::sync_channel(1);
        client
            .sender
            .send(Command {
                operation: Operation::Write {
                    path: "/api/config".into(),
                    body: json!({"version":2,"fans":[]}),
                },
                expires: Instant::now() - Duration::from_secs(1),
                reply,
            })
            .unwrap();
        controller.serve_one(&mut engine, Duration::ZERO);
        assert_eq!(receiver.recv().unwrap().err().unwrap().0, 503);
        assert_eq!(engine.config, before);
        assert_eq!(client.generation(), 0);
    }

    #[test]
    fn health_reads_do_not_queue_work_and_writes_advance_generation() {
        let temp = Temp::new();
        let mut engine = Engine::new(
            Box::new(Demo::new()),
            temp.0.join("config.json"),
            true,
            true,
            false,
        )
        .unwrap();
        let (mut controller, client) = channel(&engine);
        assert_eq!(client.health()["ok"], true);
        assert!(controller.receiver.try_recv().is_err());
        assert!(controller.snapshot.is_none());
        for (path, expected) in [("/api/editor-heartbeat", 0), ("/api/config", 2)] {
            let caller = client.clone();
            let thread = std::thread::spawn(move || {
                caller.call(Operation::Write {
                    path: path.into(),
                    body: json!({"version":2,"fans":[]}),
                })
            });
            controller.serve_one(&mut engine, Duration::from_secs(1));
            assert!(thread.join().unwrap().is_ok());
            assert_eq!(client.generation(), expected);
        }
    }

    #[test]
    fn readers_share_a_generation_and_old_snapshots_survive_changes() {
        let temp = Temp::new();
        let mut engine = Engine::new(
            Box::new(Demo::new()),
            temp.0.join("config.json"),
            true,
            true,
            false,
        )
        .unwrap();
        let (mut controller, _) = channel(&engine);
        for _ in 0..100 {
            engine.tick().unwrap();
        }
        let light = controller.snapshot(&engine, false);
        assert!(light.editor.is_none());
        assert_eq!(light.history.len(), 80);
        assert!(Arc::ptr_eq(&light, &controller.snapshot(&engine, false)));
        let old = controller.snapshot(&engine, true);
        assert!(old.editor.is_some());
        assert!(old.history.len() > 80);
        assert!(!Arc::ptr_eq(&light, &old));
        assert!(Arc::ptr_eq(&old, &controller.snapshot(&engine, false)));
        assert!(Arc::ptr_eq(&old, &controller.snapshot(&engine, true)));
        assert!(Arc::ptr_eq(&old.config, &engine.config));
        let old_reading = old.editor.as_ref().unwrap().hardware.sensors[0]["value"].clone();
        let old_revision = old.revision.clone();
        let mut config = (*engine.config).clone();
        config["fans"][0]["name"] = json!("Renamed <fan>");
        // Readers already encoding the previous generation may finish safely;
        // new requests must see new configuration and missing readings.
        controller.invalidate();
        engine.update(&config, None).unwrap();
        engine.snapshot.sensors[0]["value"] = Value::Null;
        controller.changed(&engine);
        let new = controller.snapshot(&engine, true);
        assert!(!Arc::ptr_eq(&old, &new));
        assert!(!Arc::ptr_eq(&old.config, &new.config));
        assert_eq!(
            old.editor.as_ref().unwrap().hardware.sensors[0]["value"],
            old_reading
        );
        assert!(new.editor.as_ref().unwrap().hardware.sensors[0]["value"].is_null());
        assert_eq!(old.revision, old_revision);
        assert_ne!(new.revision, old_revision);
        let response = Response::State {
            snapshot: new.clone(),
            since: Some(old_revision),
            after: None,
        };
        let encoded: Value = serde_json::from_slice(
            &response
                .encode(Metadata {
                    token: "test-token",
                    hostname: "host",
                })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(encoded["config"]["fans"][0]["name"], "Renamed <fan>");
        assert_eq!(encoded["csrf_token"], "test-token");
        assert_eq!(encoded["discovery"], json!(engine.discovery()));
        let response = Response::View {
            snapshot: new,
            since: None,
            after: None,
        };
        let encoded: Value = serde_json::from_slice(
            &response
                .encode(Metadata {
                    token: "secret",
                    hostname: "host",
                })
                .unwrap(),
        )
        .unwrap();
        assert!(encoded.get("csrf_token").is_none());
        assert!(encoded.get("config").is_none());
        assert_eq!(encoded["hostname"], "host");
        controller.invalidate();
        assert!(controller.snapshot.is_none());
    }
}
