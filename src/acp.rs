use crate::{
    finish_turn, register_cancel, send, session_has_status, turn_state_mut, update, Adapter,
    AgentKind, CancelSlot, CancelWriters, ChildRegistry, Sessions, Turn, Work,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
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

fn visible_update(update: &Value, replay: bool) -> Option<Value> {
    match update["sessionUpdate"].as_str()? {
        "user_message_chunk" if replay => {
            let text = update["content"]["text"].as_str().unwrap_or("");
            Some(json!({"role":"user","text":text}))
        }
        "agent_message_chunk" if update["content"]["type"] == "text" => {
            let text = update["content"]["text"].as_str()?;
            Some(json!({"role":"agent","text":text}))
        }
        "tool_call" | "tool_call_update" => {
            let id = update["toolCallId"].as_str()?;
            let status = update["status"].as_str()?;
            matches!(status, "pending" | "in_progress" | "completed" | "failed")
                .then(|| json!({"type":"tool_call","tool_call_id":id,"status":status}))
        }
        _ => None,
    }
}

fn append_history(history: &mut Vec<Value>, entry: Value, new_user_message: bool) {
    if let Some(role) = entry["role"].as_str() {
        if let Some(last) = history
            .last_mut()
            .filter(|last| last["role"] == role && !new_user_message)
        {
            let old = last["text"].as_str().unwrap();
            let text = entry["text"].as_str().unwrap();
            last["text"] = json!(format!("{old}{text}"));
            return;
        }
    }
    history.push(entry);
}

fn position_history(entries: &[Value]) -> (String, Vec<(usize, &Value)>) {
    let mut text = String::new();
    let positioned = entries
        .iter()
        .map(|entry| {
            if let Some(chunk) = entry["text"].as_str() {
                text.push_str(chunk);
            }
            (text.len(), entry)
        })
        .collect();
    (text, positioned)
}

fn tool_stage(entry: &Value) -> u8 {
    match entry["status"].as_str() {
        Some("pending") => 0,
        Some("in_progress") => 1,
        _ => 2,
    }
}

fn merge_turn(observed: &[Value], replay: &[Value], has_user_boundary: bool) -> Option<Vec<Value>> {
    let (observed_text, observed) = position_history(observed);
    let (replayed_text, replayed) = position_history(replay);
    // A load can omit tool updates and join the text chunks they used to separate.
    if !replayed_text.starts_with(&observed_text) && !observed_text.starts_with(&replayed_text) {
        return None;
    }
    if !has_user_boundary
        && (observed_text.is_empty() || replayed_text.is_empty())
        && !replayed.iter().any(|(_, entry)| {
            entry["tool_call_id"].as_str().is_some_and(|id| {
                observed
                    .iter()
                    .any(|(_, entry)| entry["tool_call_id"] == id)
            })
        })
    {
        return None;
    }

    // Preserve every observation. Replay can supply newer state for a tool ID,
    // but it cannot establish which occurrence of an update was observed live.
    let mut merged: Vec<_> = observed
        .into_iter()
        .map(|(offset, entry)| (offset, entry.clone()))
        .collect();
    let mut final_tools = BTreeMap::new();
    for (index, &(offset, entry)) in replayed.iter().enumerate() {
        if let Some(chunk) = entry["text"].as_str() {
            if offset > observed_text.len() {
                let start = offset - chunk.len();
                let continuation = observed_text.len().saturating_sub(start);
                let mut position = merged.len();
                if continuation > 0 {
                    // Extend the replayed chunk before the tools at its tail.
                    while position > 0 && merged[position - 1].1["type"] == "tool_call" {
                        position -= 1;
                        merged[position].0 = offset;
                    }
                }
                merged.insert(
                    position,
                    (
                        offset,
                        json!({"role":"agent","text":&chunk[continuation..]}),
                    ),
                );
            }
        } else if let (Some(id), Some(status)) =
            (entry["tool_call_id"].as_str(), entry["status"].as_str())
        {
            let (latest, terminal, conflicting) =
                final_tools.entry(id).or_insert((index, None, false));
            *latest = index;
            if matches!(status, "completed" | "failed") {
                *conflicting |= terminal.is_some_and(|previous| previous != status);
                *terminal = Some(status);
            }
        }
    }

    for (index, (offset, entry)) in replayed.into_iter().enumerate() {
        let Some(id) = entry["tool_call_id"].as_str() else {
            continue;
        };
        if !matches!(final_tools.get(id), Some((latest, _, false)) if *latest == index) {
            continue;
        }

        // A repeated status is relevant only to the current state of this ID.
        let last = merged
            .iter()
            .rposition(|(_, observed)| observed["tool_call_id"] == id);
        let after_observations = if let Some(last) = last {
            if tool_stage(&merged[last].1) >= tool_stage(entry) {
                continue;
            }
            last + 1
        } else {
            0
        };
        let index = merged
            .partition_point(|(end, _)| *end <= offset)
            .max(after_observations);
        let offset = index
            .checked_sub(1)
            .map_or(offset, |previous| offset.max(merged[previous].0));
        merged.insert(index, (offset, entry.clone()));
    }
    Some(merged.into_iter().map(|(_, entry)| entry).collect())
}

fn reconcile_history(record: &mut Value, turn_id: &str, replayed: &[Value]) {
    let history = record["history"].as_array().unwrap();
    let observed_turns: Vec<_> = history.chunk_by(|_, next| next["role"] != "user").collect();
    let replayed_turns: Vec<_> = replayed
        .chunk_by(|_, next| next["role"] != "user")
        .collect();
    let unmarked =
        observed_turns.len() == 1 && replayed_turns.len() == 1 && replayed[0]["role"] != "user";
    if !unmarked
        && (observed_turns.len() != replayed_turns.len()
            || observed_turns
                .iter()
                .zip(&replayed_turns)
                .any(|(old, new)| old[0] != new[0]))
    {
        // A replay without matching turn boundaries could describe a different turn.
        return;
    }

    let index = if record["turn_id"] == turn_id {
        let Some(index) = observed_turns.len().checked_sub(1) else {
            return;
        };
        index
    } else {
        let Some(index) = record["previous_turns"][turn_id]["history_turn_index"]
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
        else {
            return;
        };
        index
    };
    let (Some(observed), Some(replay)) = (observed_turns.get(index), replayed_turns.get(index))
    else {
        return;
    };
    let replay = if unmarked { replay } else { &replay[1..] };
    if let Some(merged) = merge_turn(&observed[1..], replay, !unmarked) {
        let start = observed_turns[..index]
            .iter()
            .map(|turn| turn.len())
            .sum::<usize>()
            + 1;
        let end = start + observed.len() - 1;
        let result: String = merged
            .iter()
            .filter(|entry| entry["role"] == "agent")
            .filter_map(|entry| entry["text"].as_str())
            .collect();
        record["history"]
            .as_array_mut()
            .unwrap()
            .splice(start..end, merged);
        turn_state_mut(record, turn_id).unwrap()["result"] = json!(result);
    }
}

pub(super) enum Start {
    New(Turn),
    Reconnect { acp_id: String, turn_id: String },
}

pub(super) struct AcpJob {
    pub sessions: Sessions,
    pub id: String,
    pub workspace: PathBuf,
    pub receiver: mpsc::Receiver<Work>,
    pub workers: Weak<Mutex<std::collections::BTreeMap<String, mpsc::Sender<Work>>>>,
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
            let mut registry = children.lock().unwrap();
            if let Some(children) = registry.children.get_mut(&self.id) {
                children.retain(|child| !Arc::ptr_eq(child, &self.child));
                if children.is_empty() {
                    registry.children.remove(&self.id);
                }
            }
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
        let workspace = self.workspace.canonicalize()?;
        if workspace != self.workspace || !workspace.is_dir() {
            return Err(io::Error::other("workspace changed"));
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
        registry
            .children
            .entry(self.id.clone())
            .or_default()
            .push(child.clone());
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

    fn recover(&self, acp_id: &str, turn_id: &str) -> io::Result<(Connection, Option<String>)> {
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
        let mut history = Vec::new();
        let mut user_message_id = None;
        let evidence = (|| {
            let initialized = connection.initialize()?;
            if initialized["agentCapabilities"]["loadSession"] != true {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "ACP session loading is unsupported",
                ));
            }
            connection.send(json!({"jsonrpc":"2.0","id":2,"method":"session/load",
                "params":{"sessionId":acp_id,"cwd":workspace,"mcpServers":[]}}))?;
            connection
                .response(&json!(2), |params| {
                    if params["sessionId"] != acp_id {
                        return;
                    }
                    let update = &params["update"];
                    if let Some(entry) = visible_update(update, true) {
                        let mut new_user_message = false;
                        if entry["role"] == "user" {
                            let message_id = update["messageId"].as_str();
                            new_user_message =
                                message_id.is_some() && message_id != user_message_id.as_deref();
                            user_message_id = message_id.map(str::to_owned);
                        }
                        append_history(&mut history, entry, new_user_message);
                    }
                })?
                .ok_or_else(|| io::Error::other("ACP session load failed"))
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
        let reason = outcome["stopReason"].as_str().filter(|reason| {
            outcome["sessionId"] == acp_id
                && outcome["promptRequestId"] == turn_id
                && known_stop_reason(reason)
        });
        update(&self.sessions, &self.id, |record| {
            if record["turn_id"] == turn_id || reason.is_some() {
                reconcile_history(record, turn_id, &history);
            }
        });
        Ok((connection, reason.map(str::to_owned)))
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
        if let Some(entry) = visible_update(u, false) {
            update(&self.sessions, &self.id, |record| {
                if entry["role"] == "agent" {
                    let old = record["result"].as_str().unwrap_or("");
                    let text = entry["text"].as_str().unwrap();
                    record["result"] = json!(format!("{old}{text}"));
                }
                append_history(record["history"].as_array_mut().unwrap(), entry, false);
            });
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

    fn finish_reconnect(&self, turn_id: &str, reason: Option<&str>) {
        if let Some(reason) = reason {
            self.finish(turn_id, Some(reason));
        } else {
            update(&self.sessions, &self.id, |record| {
                if let Some(turn) = turn_state_mut(record, turn_id) {
                    turn["status"] = json!("unknown");
                }
            });
        }
    }

    fn require_recovery(&self, turn_id: &str) {
        crate::require_recovery(&self.sessions, self.events.as_deref(), &self.id, turn_id);
    }

    fn retire(&self) {
        if let Some(workers) = self.workers.upgrade() {
            workers.lock().unwrap().remove(&self.id);
        }
        self.cancel_writers.lock().unwrap().remove(&self.id);
    }

    fn next_work(&self, connection: &Connection) -> Option<Work> {
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
                    session_has_status(record, &["running", "cancelling", "recovering"])
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
                Ok((connection, reason)) => {
                    self.finish_reconnect(&turn_id, reason.as_deref());
                    (connection, acp_id, None)
                }
                Err(_) => {
                    self.retire();
                    self.finish_reconnect(&turn_id, None);
                    return;
                }
            },
        };
        while let Some(work) = first
            .take()
            .map(Work::Prompt)
            .or_else(|| self.next_work(&connection))
        {
            let turn = match work {
                Work::Prompt(turn) => turn,
                Work::Reconnect { turn_id } => {
                    let latest = self
                        .sessions
                        .lock()
                        .unwrap()
                        .get(&self.id)
                        .is_some_and(|record| record["turn_id"] == turn_id);
                    let reason = match self.recover(&acp_id, &turn_id) {
                        Ok((loaded, reason)) => {
                            if loaded.alive() && (latest || !connection.alive()) {
                                connection = loaded;
                            }
                            reason
                        }
                        Err(_) => None,
                    };
                    self.finish_reconnect(&turn_id, reason.as_deref());
                    continue;
                }
            };
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
                            Ok((loaded, reason)) => {
                                let confirmed = reason.is_some();
                                if confirmed || loaded.alive() {
                                    recovered = Some((loaded, reason));
                                }
                                if confirmed {
                                    break;
                                }
                            }
                            Err(error) if error.kind() == io::ErrorKind::Unsupported => break,
                            Err(_) => {}
                        }
                    }
                    if let Some((loaded, reason)) = recovered {
                        connection = loaded;
                        if let Some(reason) = reason {
                            self.finish(&turn.id, Some(&reason));
                        } else {
                            self.require_recovery(&turn.id);
                        }
                    } else {
                        self.retire();
                        self.require_recovery(&turn.id);
                        return;
                    }
                }
            }
        }
    }
}
