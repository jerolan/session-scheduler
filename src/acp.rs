use crate::{
    finish_turn, register_cancel, send, update, Adapter, AgentKind, CancelSlot, CancelWriters,
    ChildRegistry, Sessions, Turn,
};
use serde_json::{json, Value};
use std::{
    env,
    io::{self, BufRead, BufReader},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{mpsc, Arc, Mutex, Weak},
    thread,
    time::{Duration, Instant},
};

const RECOVERY_ATTEMPTS: usize = 3;
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(2);

fn known_stop_reason(reason: &str) -> bool {
    matches!(
        reason,
        "end_turn" | "cancelled" | "refusal" | "max_tokens" | "max_turn_requests" | "failed"
    )
}

pub(super) enum Start {
    New(Turn),
    Reconnect { acp_id: String, turn_id: String },
}

pub(super) struct AcpJob {
    pub sessions: Sessions,
    pub id: String,
    pub workspace: PathBuf,
    pub receiver: mpsc::Receiver<Turn>,
    pub workers: Weak<Mutex<std::collections::BTreeMap<String, mpsc::Sender<Turn>>>>,
    pub children: Weak<Mutex<ChildRegistry>>,
    pub adapter: Adapter,
    pub events: Option<Arc<crate::events::Events>>,
    pub cancel_writers: CancelWriters,
}

struct Connection {
    child: Arc<Mutex<Child>>,
    children: Weak<Mutex<ChildRegistry>>,
    id: String,
    input: Arc<Mutex<ChildStdin>>,
    output: mpsc::Receiver<io::Result<String>>,
    deadline: Option<Instant>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Do not hold the child lock while taking the registry lock (see State::drop).
        {
            let mut child = self.child.lock().unwrap();
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(children) = self.children.upgrade() {
            children.lock().unwrap().children.remove(&self.id);
        }
    }
}

impl Connection {
    fn send(&self, value: Value) -> io::Result<()> {
        send(&mut *self.input.lock().unwrap(), value)
    }

    fn response(
        &mut self,
        request_id: &Value,
        mut on_update: impl FnMut(&Value),
    ) -> io::Result<Option<Value>> {
        loop {
            let line = match self.deadline {
                Some(deadline) => self
                    .output
                    .recv_timeout(deadline.checked_duration_since(Instant::now()).ok_or_else(
                        || io::Error::new(io::ErrorKind::TimedOut, "ACP reload timed out"),
                    )?)
                    .map_err(|error| match error {
                        mpsc::RecvTimeoutError::Timeout => {
                            io::Error::new(io::ErrorKind::TimedOut, "ACP reload timed out")
                        }
                        mpsc::RecvTimeoutError::Disconnected => {
                            io::Error::other("ACP connection closed")
                        }
                    })?,
                None => self
                    .output
                    .recv()
                    .map_err(|_| io::Error::other("ACP connection closed"))?,
            }?;
            let message: Value = serde_json::from_str(&line).map_err(io::Error::other)?;
            if message["jsonrpc"] != "2.0" {
                continue;
            }
            if let Some(method) = message["method"].as_str() {
                if method == "session/update" {
                    on_update(&message["params"]);
                } else if let Some(id) = message.get("id") {
                    let reply = if method == "session/request_permission" {
                        json!({"jsonrpc":"2.0","id":id,"result":{"outcome":{"outcome":"cancelled"}}})
                    } else {
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not supported"}})
                    };
                    self.send(reply)?;
                }
            } else if message["id"] == *request_id {
                return match (message.get("result"), message.get("error")) {
                    (Some(result), None) if result.is_object() => Ok(Some(result.clone())),
                    (None, Some(error))
                        if error["code"].is_i64() && error["message"].is_string() =>
                    {
                        Ok(None)
                    }
                    _ => Err(io::Error::other("invalid ACP response")),
                };
            }
        }
    }

    fn setup(&mut self, id: u64, method: &str, params: Value) -> io::Result<Value> {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        self.response(&json!(id), |_| {})?
            .ok_or_else(|| io::Error::other("ACP request failed"))
    }

    fn initialize(&mut self) -> io::Result<Value> {
        self.setup(1, "initialize", json!({"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"session-scheduler","version":"0.1.0"}}))
    }

    fn alive(&self) -> bool {
        matches!(self.child.lock().unwrap().try_wait(), Ok(None))
    }
}

impl AcpJob {
    fn connect(&self) -> io::Result<Connection> {
        let mut command = Command::new(&self.adapter.executable);
        match self.adapter.kind {
            AgentKind::Codex => command.env("INITIAL_AGENT_MODE", "read-only"),
            AgentKind::OpenCode => command.arg("acp"),
        };
        for (key, _) in env::vars_os() {
            // Windows environment keys are case-insensitive.
            let name = key.to_string_lossy().to_ascii_uppercase();
            if name.starts_with("SESSION_SCHEDULER_")
                || name.starts_with("CONTROL_PLANE_")
                || name == "OPENAI_ADMIN_KEY"
            {
                command.env_remove(key);
            }
        }
        let registry = self
            .children
            .upgrade()
            .ok_or_else(|| io::Error::other("scheduler stopped"))?;
        let mut registry = registry.lock().unwrap();
        if registry.stopping {
            return Err(io::Error::other("scheduler stopped"));
        }
        let mut child = command
            .current_dir(&self.workspace)
            .env_remove("CODEX_CONFIG")
            .env_remove("APP_SERVER_LOGS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let input = Arc::new(Mutex::new(child.stdin.take().unwrap()));
        let reader = BufReader::new(child.stdout.take().unwrap());
        let (lines, output) = mpsc::sync_channel(1);
        thread::spawn(move || {
            for line in reader.lines() {
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        let child = Arc::new(Mutex::new(child));
        registry.children.insert(self.id.clone(), child.clone());
        Ok(Connection {
            child,
            children: self.children.clone(),
            id: self.id.clone(),
            input,
            output,
            deadline: None,
        })
    }

    fn open_session(&self) -> io::Result<(Connection, String)> {
        let mut connection = self.connect()?;
        connection.initialize()?;
        let session = connection.setup(
            2,
            "session/new",
            json!({"cwd":self.workspace,"mcpServers":[]}),
        )?;
        let id = session["sessionId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| io::Error::other("missing ACP session"))?
            .to_owned();
        update(&self.sessions, &self.id, |s| {
            s["acp_session_id"] = json!(id)
        });
        Ok((connection, id))
    }

    fn recover(&self, acp_id: &str, turn_id: &str) -> io::Result<Option<(Connection, String)>> {
        let workspace = self.workspace.canonicalize()?;
        if workspace != self.workspace || !workspace.is_dir() {
            return Err(io::Error::other("workspace changed"));
        }
        let mut connection = self.connect()?;
        let (done, deadline) = mpsc::channel();
        let child = connection.child.clone();
        let expires = Instant::now() + RECOVERY_TIMEOUT;
        connection.deadline = Some(expires);
        // Terminate the adapter even if the recovery handshake blocks on a write.
        let watchdog = thread::spawn(move || {
            if deadline
                .recv_timeout(expires.saturating_duration_since(Instant::now()))
                .is_err()
            {
                let _ = child.lock().unwrap().kill();
            }
        });
        let evidence = (|| {
            let initialized = connection.initialize()?;
            if initialized["agentCapabilities"]["loadSession"] != true {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "ACP session loading is unsupported",
                ));
            }
            // Load replays the history. Keep the scheduler's observed history unchanged.
            // Replayed messages carry no proof about the interrupted prompt's outcome.
            connection.setup(
                2,
                "session/load",
                json!({"sessionId":acp_id,"cwd":workspace,"mcpServers":[]}),
            )
        })();
        let _ = done.send(());
        let _ = watchdog.join();
        connection.deadline = None;
        if Instant::now() >= expires {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "ACP reload timed out",
            ));
        }
        let loaded = evidence?;
        let outcome = &loaded["_meta"]["session-scheduler/turnOutcome"];
        if outcome["sessionId"] == acp_id && outcome["promptRequestId"] == turn_id {
            if let Some(reason) = outcome["stopReason"]
                .as_str()
                .filter(|reason| known_stop_reason(reason))
            {
                return Ok(Some((connection, reason.to_owned())));
            }
        }
        Ok(None)
    }

    fn prompt(
        &self,
        connection: &mut Connection,
        acp_id: &str,
        turn: &Turn,
        cancel: &CancelSlot,
    ) -> io::Result<Option<String>> {
        // The same unique ID identifies the turn in MCP and the original ACP request.
        connection.send(json!({"jsonrpc":"2.0","id":turn.id,"method":"session/prompt","params":{"sessionId":acp_id,"prompt":[{"type":"text","text":turn.prompt}]}}))?;
        register_cancel(
            cancel,
            &self.sessions,
            &self.id,
            &turn.id,
            acp_id,
            &connection.input,
        );
        let response = connection.response(&json!(turn.id), |params| {
            if params["sessionId"] == acp_id {
                self.record_update(&params["update"]);
            }
        })?;
        response
            .map(|response| {
                response["stopReason"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| io::Error::other("missing stop reason"))
            })
            .transpose()
    }

    fn record_update(&self, u: &Value) {
        if u["sessionUpdate"] == "agent_message_chunk" && u["content"]["type"] == "text" {
            if let Some(text) = u["content"]["text"].as_str() {
                update(&self.sessions, &self.id, |s| {
                    let old = s["result"].as_str().unwrap_or("");
                    s["result"] = json!(format!("{old}{text}"));
                    let history = s["history"].as_array_mut().unwrap();
                    if let Some(last) = history.last_mut().filter(|entry| entry["role"] == "agent")
                    {
                        let old = last["text"].as_str().unwrap_or("");
                        last["text"] = json!(format!("{old}{text}"));
                    } else {
                        history.push(json!({"role":"agent","text":text}));
                    }
                });
            }
        } else if matches!(
            u["sessionUpdate"].as_str(),
            Some("tool_call" | "tool_call_update")
        ) {
            if let (Some(tool_id), Some(status)) = (u["toolCallId"].as_str(), u["status"].as_str())
            {
                if matches!(status, "pending" | "in_progress" | "completed" | "failed") {
                    update(&self.sessions, &self.id, |s| {
                        s["history"].as_array_mut().unwrap().push(
                            json!({"type":"tool_call","tool_call_id":tool_id,"status":status}),
                        );
                    });
                }
            }
        }
    }

    fn finish(&self, turn_id: &str, reason: Option<&str>) {
        let status = match reason {
            Some("end_turn") => "completed",
            Some("cancelled") => "cancelled",
            _ => "failed",
        };
        let event_reason = reason
            .filter(|reason| known_stop_reason(reason))
            .unwrap_or("failed");
        finish_turn(
            &self.sessions,
            self.events.as_deref(),
            &self.id,
            turn_id,
            status,
            reason,
            event_reason,
        );
    }

    fn retire(&self) {
        if let Some(workers) = self.workers.upgrade() {
            workers.lock().unwrap().remove(&self.id);
        }
        self.cancel_writers.lock().unwrap().remove(&self.id);
    }

    fn next_turn(&self, connection: &Connection) -> Option<Turn> {
        loop {
            match self.receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(turn) => return Some(turn),
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if !connection.alive() {
                let records = self.sessions.lock().unwrap();
                // A follow-up may already have reserved the turn and be about to queue it.
                if !records.get(&self.id).is_some_and(|record| {
                    matches!(record["status"].as_str(), Some("running" | "cancelling"))
                }) {
                    self.retire();
                    return None;
                }
            }
        }
    }

    pub(super) fn run(self, start: Start) {
        let cancel = self
            .cancel_writers
            .lock()
            .unwrap()
            .get(&self.id)
            .unwrap()
            .clone();
        let (mut connection, acp_id, mut first) = match start {
            Start::New(turn) => match self.open_session() {
                Ok((connection, acp_id)) => (connection, acp_id, Some(turn)),
                Err(_) => {
                    self.retire();
                    self.finish(&turn.id, None);
                    return;
                }
            },
            Start::Reconnect { acp_id, turn_id } => match self.recover(&acp_id, &turn_id) {
                Ok(Some((connection, reason))) => {
                    self.finish(&turn_id, Some(&reason));
                    (connection, acp_id, None)
                }
                _ => {
                    self.retire();
                    update(&self.sessions, &self.id, |record| {
                        record["status"] = json!("unknown")
                    });
                    return;
                }
            },
        };
        while let Some(turn) = first.take().or_else(|| self.next_turn(&connection)) {
            if !connection.alive() {
                drop(connection);
                self.retire();
                self.finish(&turn.id, None);
                return;
            }
            let outcome = self.prompt(&mut connection, &acp_id, &turn, &cancel);
            *cancel.lock().unwrap() = None;
            match outcome {
                Ok(Some(reason)) => self.finish(&turn.id, Some(&reason)),
                Ok(None) => {
                    drop(connection);
                    self.retire();
                    self.finish(&turn.id, None);
                    return;
                }
                Err(_) => {
                    drop(connection);
                    update(&self.sessions, &self.id, |record| {
                        record["status"] = json!("recovering")
                    });
                    let mut recovered = None;
                    for attempt in 0..RECOVERY_ATTEMPTS {
                        if attempt != 0 {
                            thread::sleep(Duration::from_millis(100));
                        }
                        match self.recover(&acp_id, &turn.id) {
                            Ok(Some(loaded)) => {
                                recovered = Some(loaded);
                                break;
                            }
                            Err(error) if error.kind() == io::ErrorKind::Unsupported => break,
                            _ => {}
                        }
                    }
                    let Some((loaded, reason)) = recovered else {
                        self.retire();
                        crate::require_recovery(
                            &self.sessions,
                            self.events.as_deref(),
                            &self.id,
                            &turn.id,
                        );
                        return;
                    };
                    connection = loaded;
                    self.finish(&turn.id, Some(&reason));
                }
            }
        }
        drop(connection);
        self.retire();
    }
}
