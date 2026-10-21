//! One owner for hardware/control state. HTTP workers exchange bounded commands;
//! they never hold a control lock or serialize JSON on the control thread.
use crate::{common::*, engine::Engine};
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
    View,
    Write {
        path: String,
        body: Value,
    },
}

struct Command {
    operation: Operation,
    expires: Instant,
    reply: SyncSender<Result<Value>>,
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

    pub fn call(&self, operation: Operation) -> Result<Value> {
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
    pub fn invalidate(&self) {
        self.client.generation.fetch_add(1, Ordering::Release);
    }

    pub fn changed(&self, engine: &Engine) {
        self.client
            .healthy
            .store(engine.error.is_none(), Ordering::Release);
        self.invalidate();
    }

    // Main checks its control deadline before every command. A stream of HTTP
    // reads therefore cannot starve a due control cycle.
    pub fn serve_one(&self, engine: &mut Engine, wait: Duration) {
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
            Operation::State { since, after } => engine.state(since.as_deref(), after),
            Operation::View => engine.view_state(),
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
                let _ = command.reply.send(result);
                return;
            }
        };
        let _ = command.reply.send(Ok(result));
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
        let (controller, client) = channel(&engine);
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
        assert_eq!(receiver.recv().unwrap().unwrap_err().0, 503);
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
        let (controller, client) = channel(&engine);
        assert_eq!(client.health()["ok"], true);
        assert!(controller.receiver.try_recv().is_err());
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
}
