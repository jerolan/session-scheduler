mod acp;
mod events;
mod http;
mod session_registry;
mod state_files;
use acp::{AcpJob, Start};
use events::TurnEvent;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use session_registry::{SessionRegistry, Sessions};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin},
    sync::{mpsc, Arc, Mutex},
    thread,
};
use uuid::Uuid;

#[derive(Default)]
struct ChildRegistry {
    stopping: bool,
    children: BTreeMap<String, Vec<Arc<Mutex<Child>>>>,
}
type Children = Arc<Mutex<ChildRegistry>>;
#[derive(Clone, Copy)]
enum AgentKind {
    Codex,
    OpenCode,
}
impl AgentKind {
    fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }
}

struct Adapter {
    kind: AgentKind,
    executable: PathBuf,
}
struct PendingCancel {
    acp_id: String,
    writer: Arc<Mutex<ChildStdin>>,
    sent: bool,
}
type CancelSlot = Arc<Mutex<Option<PendingCancel>>>;
type CancelWriters = Arc<Mutex<BTreeMap<String, CancelSlot>>>;
fn schedule_cancel(slot: CancelSlot, sessions: Sessions, id: String, turn_id: String) {
    thread::spawn(move || {
        let writer = {
            let guard = slot.lock().unwrap();
            guard.as_ref().map(|pending| pending.writer.clone())
        };
        let Some(writer) = writer else {
            return;
        };
        // Serialize with subsequent prompts, but do not hold state locks over I/O.
        let mut writer = writer.lock().unwrap();
        let acp_id = {
            let mut guard = slot.lock().unwrap();
            let records = sessions.lock().unwrap();
            let current = records
                .get(&id)
                .is_some_and(|s| s["turn_id"] == turn_id && s["status"] == "cancelling");
            match guard.as_mut() {
                Some(pending) if current && !pending.sent => {
                    pending.sent = true;
                    Some(pending.acp_id.clone())
                }
                _ => None,
            }
        };
        if let Some(acp_id) = acp_id {
            // A failed cancel write does not disprove an already-buffered prompt response.
            // Keep reading until that response or EOF determines the turn's outcome.
            let _ = send(
                &mut *writer,
                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":acp_id}}),
            );
        }
    });
}
fn register_cancel(
    slot: &CancelSlot,
    sessions: &Sessions,
    id: &str,
    turn_id: &str,
    acp_id: &str,
    input: &Arc<Mutex<ChildStdin>>,
) {
    let cancelling = {
        let mut guard = slot.lock().unwrap();
        *guard = Some(PendingCancel {
            acp_id: acp_id.to_owned(),
            writer: input.clone(),
            sent: false,
        });
        sessions
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|s| s["turn_id"] == turn_id && s["status"] == "cancelling")
    };
    if cancelling {
        schedule_cancel(
            slot.clone(),
            sessions.clone(),
            id.to_owned(),
            turn_id.to_owned(),
        );
    }
}
type Workers = Arc<Mutex<BTreeMap<String, mpsc::Sender<Work>>>>;
struct Turn {
    id: String,
    prompt: String,
}

enum Work {
    Prompt(Turn),
    Reconnect { turn_id: String },
}

fn resolve_workspace(args: &Value) -> Option<PathBuf> {
    args["workspace"]
        .as_str()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .and_then(|p| p.canonicalize().ok())
        .filter(|p| p.is_dir() && std::fs::read_dir(p).is_ok())
}

fn result(value: Value, error: bool) -> Value {
    json!({"content":[{"type":"text", "text":value.to_string()}], "isError":error})
}
fn update(sessions: &Sessions, id: &str, f: impl FnOnce(&mut Value)) {
    if let Some(session) = sessions.lock().unwrap().get_mut(id) {
        f(session);
    }
}
fn turn_state_mut<'a>(record: &'a mut Value, turn_id: &str) -> Option<&'a mut Value> {
    if record["turn_id"] == turn_id {
        Some(record)
    } else {
        record.get_mut("previous_turns")?.get_mut(turn_id)
    }
}

fn session_has_status(record: &Value, statuses: &[&str]) -> bool {
    std::iter::once(record)
        .chain(
            record["previous_turns"]
                .as_object()
                .into_iter()
                .flat_map(|turns| turns.values()),
        )
        .any(|turn| {
            turn["status"]
                .as_str()
                .is_some_and(|status| statuses.contains(&status))
        })
}

fn finish_turn(
    sessions: &Sessions,
    events: Option<&events::Events>,
    id: &str,
    turn_id: &str,
    status: &'static str,
    stop_reason: Option<&str>,
    event_reason: &str,
) {
    if events.is_some() {
        update(sessions, id, |record| {
            if let Some(turn) = turn_state_mut(record, turn_id) {
                turn["status"] = json!("completion_pending");
            }
        });
    }
    let publish = {
        let sessions = sessions.clone();
        let id = id.to_owned();
        let turn_id = turn_id.to_owned();
        let stop_reason = json!(stop_reason);
        move || {
            update(&sessions, &id, |record| {
                if let Some(turn) = turn_state_mut(record, &turn_id) {
                    turn["status"] = json!(status);
                    turn["stop_reason"] = stop_reason;
                    if let Some(ids) = record
                        .get_mut("unresolved_turn_ids")
                        .and_then(Value::as_array_mut)
                    {
                        ids.retain(|id| id != &turn_id);
                    }
                }
            });
        }
    };
    if let Some(events) = events {
        if let Err(error) =
            events.queue_turn_event(TurnEvent::Completed, id, turn_id, event_reason, publish)
        {
            eprintln!("could not persist completion of turn {turn_id}: {error}");
        }
    } else {
        publish();
    }
}

fn require_recovery(sessions: &Sessions, events: Option<&events::Events>, id: &str, turn_id: &str) {
    let publish = {
        let (sessions, id, turn_id) = (sessions.clone(), id.to_owned(), turn_id.to_owned());
        move || {
            update(&sessions, &id, |record| {
                if record["turn_id"] == turn_id && record["status"] == "recovering" {
                    record["status"] = json!("unknown");
                }
            })
        }
    };
    if let Some(events) = events {
        if let Err(error) = events.queue_turn_event(
            TurnEvent::RecoveryRequired,
            id,
            turn_id,
            "outcome_unknown",
            publish,
        ) {
            eprintln!("could not persist recovery notice for turn {turn_id}: {error}");
        }
    } else {
        publish();
    }
}

fn send(writer: &mut impl Write, value: Value) -> io::Result<()> {
    writeln!(writer, "{value}")?;
    writer.flush()
}
struct Launch {
    action: Option<Action>,
    sessions: Sessions,
    id: String,
    turn_id: String,
    previous: Option<Value>,
    events: Option<Arc<events::Events>>,
    cancel_writers: CancelWriters,
}
enum Action {
    Start {
        workspace: PathBuf,
        start: Start,
        receiver: mpsc::Receiver<Work>,
        workers: Workers,
        children: Children,
        adapter: Adapter,
    },
    Send(mpsc::Sender<Work>, Work),
}
impl Drop for Launch {
    fn drop(&mut self) {
        if self.action.is_some() {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(record) = sessions.get_mut(&self.id) {
                if record["turn_id"] == self.turn_id
                    || record["previous_turns"][&self.turn_id]["status"] == "recovering"
                {
                    if let Some(previous) = self.previous.take() {
                        *record = previous;
                    } else {
                        sessions.remove(&self.id);
                    }
                    if let Some(Action::Start { workers, .. }) = &self.action {
                        workers.lock().unwrap().remove(&self.id);
                        self.cancel_writers.lock().unwrap().remove(&self.id);
                    }
                }
            }
        }
    }
}
fn launch(mut job: Launch) {
    match job.action.take().unwrap() {
        Action::Start {
            workspace,
            start,
            receiver,
            workers,
            children,
            adapter,
        } => {
            let acp = AcpJob {
                sessions: job.sessions.clone(),
                id: job.id.clone(),
                workspace,
                receiver,
                workers: Arc::downgrade(&workers),
                children: Arc::downgrade(&children),
                adapter,
                events: job.events.clone(),
                cancel_writers: job.cancel_writers.clone(),
            };
            thread::spawn(move || acp.run(start));
        }
        Action::Send(sender, work) => {
            if let Err(error) = sender.send(work) {
                if matches!(error.0, Work::Reconnect { .. }) {
                    update(&job.sessions, &job.id, |record| {
                        if let Some(turn) = turn_state_mut(record, &job.turn_id) {
                            turn["status"] = json!("unknown");
                        }
                    });
                    return;
                }
                finish_turn(
                    &job.sessions,
                    job.events.as_deref(),
                    &job.id,
                    &job.turn_id,
                    "failed",
                    None,
                    "failed",
                );
            }
        }
    }
}
struct Attempt {
    original: PathBuf,
    workspace: PathBuf,
    prompt: String,
    kind: AgentKind,
    executable: PathBuf,
    completed: Option<Value>,
}
#[derive(Clone, Default, Deserialize, Serialize)]
struct WorkspaceDecisions {
    approved: BTreeSet<PathBuf>,
    revoked: BTreeSet<PathBuf>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SavedWorkspaceDecisions {
    Decisions(WorkspaceDecisions),
    Legacy(BTreeSet<PathBuf>),
}

#[derive(Default)]
struct WorkspaceTrust {
    decisions: WorkspaceDecisions,
    attempts: BTreeMap<String, Attempt>,
}

struct State {
    configured: Vec<PathBuf>,
    workspace_trust: Mutex<WorkspaceTrust>,
    trust_store: Option<state_files::StateFiles>,
    codex_command: Option<PathBuf>,
    opencode_command: Option<PathBuf>,
    sessions: Sessions,
    workers: Workers,
    children: Children,
    events: Option<Arc<events::Events>>,
    cancel_writers: CancelWriters,
}
impl Drop for State {
    fn drop(&mut self) {
        let mut registry = self.children.lock().unwrap();
        registry.stopping = true;
        for child in registry.children.values().flatten() {
            let _ = child.lock().unwrap().kill();
        }
    }
}
impl State {
    fn new(events: Option<Arc<events::Events>>) -> io::Result<Self> {
        let configured: Vec<PathBuf> = env::var_os("SESSION_SCHEDULER_WORKSPACES")
            .map(|v| {
                env::split_paths(&v)
                    .filter_map(|p| p.canonicalize().ok())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        let mut decisions = WorkspaceDecisions::default();
        let mut trust_store = None;
        if events.is_some() {
            let loaded = (|| -> io::Result<_> {
                let dir = env::var_os("SESSION_SCHEDULER_STATE_DIR")
                    .ok_or_else(|| io::Error::other("state directory required"))?;
                let store = state_files::StateFiles::open(PathBuf::from(dir))?;
                let decisions = match store.read(state_files::StateFile::Workspaces)? {
                    Some(file) => match serde_json::from_reader(file).map_err(io::Error::other)? {
                        SavedWorkspaceDecisions::Decisions(decisions) => decisions,
                        SavedWorkspaceDecisions::Legacy(approved) => WorkspaceDecisions {
                            approved,
                            ..WorkspaceDecisions::default()
                        },
                    },
                    None => WorkspaceDecisions::default(),
                };
                Ok((store, decisions))
            })();
            match loaded {
                Ok((store, saved)) => {
                    trust_store = Some(store);
                    decisions = saved;
                }
                Err(error) => eprintln!("workspace trust unavailable: {error}"),
            }
        }
        let executable =
            |name| env::var_os(name).and_then(|p| PathBuf::from(p).canonicalize().ok());
        let sessions = Arc::new(SessionRegistry::new(events.is_some())?);
        if let Some(events) = &events {
            for (id, turn_id, reason) in events.persisted_completions() {
                let mut records = sessions.lock().unwrap();
                if let Some(record) = records.get_mut(&id) {
                    if let Some(ids) = record["unresolved_turn_ids"].as_array_mut() {
                        ids.retain(|id| id != &turn_id);
                    }
                    if record["turn_id"] == turn_id && record["status"] == "pending_confirmation" {
                        record["status"] = json!(match reason.as_str() {
                            "end_turn" => "completed",
                            "cancelled" => "cancelled",
                            _ => "failed",
                        });
                        record["stop_reason"] = if reason == "failed" {
                            Value::Null
                        } else {
                            json!(reason)
                        };
                    }
                    record["restart_blocked"] = json!(
                        record["status"] == "pending_confirmation"
                            || record["unresolved_turn_ids"]
                                .as_array()
                                .is_some_and(|ids| !ids.is_empty())
                    );
                }
            }
        }
        Ok(Self {
            configured,
            workspace_trust: Mutex::new(WorkspaceTrust {
                decisions,
                ..WorkspaceTrust::default()
            }),
            trust_store,
            codex_command: executable("SESSION_SCHEDULER_ACP_COMMAND"),
            opencode_command: executable("SESSION_SCHEDULER_OPENCODE_COMMAND"),
            sessions,
            workers: Arc::new(Mutex::new(BTreeMap::new())),
            children: Arc::new(Mutex::new(ChildRegistry::default())),
            cancel_writers: Arc::new(Mutex::new(BTreeMap::new())),
            events,
        })
    }
    fn is_workspace_trusted(&self, decisions: &WorkspaceDecisions, workspace: &Path) -> bool {
        (self.events.is_none() || self.trust_store.is_some())
            && !decisions.revoked.contains(workspace)
            && (decisions.approved.contains(workspace)
                || self.configured.iter().any(|path| path == workspace))
    }

    fn session_workspace(
        &self,
        decisions: &WorkspaceDecisions,
        record: &Value,
    ) -> Result<PathBuf, Value> {
        let workspace = record["workspace"]
            .as_str()
            .map(Path::new)
            .filter(|path| path.canonicalize().is_ok_and(|p| p == *path && p.is_dir()))
            .ok_or_else(|| result(json!("Workspace is not allowed"), true))?;
        if !self.is_workspace_trusted(decisions, workspace) {
            return Err(result(
                json!({"status":"workspace_not_trusted", "workspace":workspace}),
                true,
            ));
        }
        Ok(workspace.to_owned())
    }

    fn save_workspace_decisions(&self, decisions: &WorkspaceDecisions) -> Result<(), Value> {
        let store = self
            .trust_store
            .as_ref()
            .ok_or_else(|| result(json!("Workspace trust store unavailable"), true))?;
        store
            .replace(state_files::StateFile::Workspaces, |file| {
                serde_json::to_writer(file, decisions).map_err(io::Error::other)
            })
            .map_err(|_| result(json!("Could not save workspace trust"), true))
    }

    fn import_session(&self, args: &Value) -> Value {
        let Some(agent @ ("codex" | "opencode")) = args["agent"].as_str() else {
            return result(json!("Unknown agent"), true);
        };
        let Some(native_id) = args["native_session_id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.trim() == *id && !id.chars().any(char::is_control))
        else {
            return result(json!("A native session ID is required"), true);
        };
        let Some(workspace) = resolve_workspace(args) else {
            return result(json!({"status":"invalid_workspace"}), true);
        };
        let trust = self.workspace_trust.lock().unwrap();
        if !self.is_workspace_trusted(&trust.decisions, &workspace) {
            return result(
                json!({"status":"workspace_not_trusted", "workspace":workspace}),
                true,
            );
        }

        let workspace = json!(workspace);
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(record) = sessions.values().find(|record| {
            record["agent"] == agent
                && record["acp_session_id"] == native_id
                && record["workspace"] == workspace
        }) {
            return result(
                json!({"session_id":record["session_id"], "status":record["status"]}),
                false,
            );
        }

        // The supplied reference alone gives us no evidence of an observed turn.
        let id = Uuid::new_v4().to_string();
        sessions.insert(
            id.clone(),
            json!({
                "session_id":id, "turn_id":null, "workspace":workspace, "agent":agent,
                "status":"pending_confirmation", "restart_blocked":true,
                "acp_session_id":native_id, "stop_reason":null, "archived":false,
                "history":[], "result":""
            }),
        );
        drop(sessions);
        result(
            json!({"session_id":id, "status":"pending_confirmation"}),
            false,
        )
    }

    fn delegate(&self, args: &Value) -> (Value, Option<Launch>) {
        let Some(workspace) = resolve_workspace(args) else {
            return (result(json!({"status":"invalid_workspace"}), true), None);
        };
        let (kind, executable) = match args.get("agent") {
            None => (AgentKind::Codex, &self.codex_command),
            Some(agent) if agent == "codex" => (AgentKind::Codex, &self.codex_command),
            Some(agent) if agent == "opencode" => (AgentKind::OpenCode, &self.opencode_command),
            Some(_) => return (result(json!("Unknown agent"), true), None),
        };
        let (Some(executable), Some(prompt)) = (
            executable.clone(),
            args["prompt"].as_str().filter(|p| !p.is_empty()),
        ) else {
            return (result(json!("Adapter or prompt unavailable"), true), None);
        };

        // Serialize admission with revocation until the session or turn is reserved.
        let mut trust = self.workspace_trust.lock().unwrap();
        if !self.is_workspace_trusted(&trust.decisions, &workspace) {
            if self.events.is_none() {
                return (result(json!("Workspace is not allowed"), true), None);
            }
            if self.trust_store.is_none() {
                return (
                    result(json!("Workspace trust store unavailable"), true),
                    None,
                );
            }
            let attempt_id = Uuid::new_v4().to_string();
            trust.attempts.insert(
                attempt_id.clone(),
                Attempt {
                    original: PathBuf::from(args["workspace"].as_str().unwrap()),
                    workspace: workspace.clone(),
                    prompt: prompt.to_owned(),
                    kind,
                    executable,
                    completed: None,
                },
            );
            return (
                result(
                    json!({"status":"approval_required", "workspace":workspace, "attempt_id":attempt_id}),
                    false,
                ),
                None,
            );
        }

        self.start_delegate(workspace, kind, executable, prompt.to_owned())
    }
    fn start_delegate(
        &self,
        workspace: PathBuf,
        kind: AgentKind,
        executable: PathBuf,
        prompt: String,
    ) -> (Value, Option<Launch>) {
        let sid = Uuid::new_v4().to_string();
        let tid = Uuid::new_v4().to_string();
        let record = json!({
            "session_id":sid, "turn_id":tid, "workspace":workspace, "agent":kind.name(),
            "status":"running", "result":"", "history":[{"role":"user","text":prompt}],
            "acp_session_id":null, "stop_reason":null, "archived":false
        });
        self.cancel_writers
            .lock()
            .unwrap()
            .insert(sid.clone(), Arc::new(Mutex::new(None)));
        self.sessions.lock().unwrap().insert(sid.clone(), record);
        let (sender, receiver) = mpsc::channel();
        self.workers.lock().unwrap().insert(sid.clone(), sender);
        let job = Launch {
            action: Some(Action::Start {
                workspace,
                start: Start::New(Turn {
                    id: tid.clone(),
                    prompt: prompt.to_owned(),
                }),
                receiver,
                workers: self.workers.clone(),
                children: self.children.clone(),
                adapter: Adapter { kind, executable },
            }),
            sessions: self.sessions.clone(),
            id: sid.clone(),
            turn_id: tid.clone(),
            previous: None,
            events: self.events.clone(),
            cancel_writers: self.cancel_writers.clone(),
        };
        (
            result(json!({"session_id":sid,"turn_id":tid}), false),
            Some(job),
        )
    }
    fn approve_workspace(&self, args: &Value) -> Value {
        let mut trust = self.workspace_trust.lock().unwrap();
        let WorkspaceTrust {
            decisions,
            attempts,
        } = &mut *trust;
        let Some(attempt) = args["attempt_id"]
            .as_str()
            .and_then(|id| attempts.get_mut(id))
        else {
            return result(json!("Attempt not found"), true);
        };
        if let Some(response) = &attempt.completed {
            return response.clone();
        }
        if !attempt
            .original
            .canonicalize()
            .ok()
            .is_some_and(|p| p == attempt.workspace && p.is_dir() && std::fs::read_dir(p).is_ok())
        {
            return result(json!({"status":"invalid_workspace"}), true);
        }
        let mut updated = decisions.clone();
        updated.revoked.remove(&attempt.workspace);
        updated.approved.insert(attempt.workspace.clone());
        if let Err(error) = self.save_workspace_decisions(&updated) {
            return error;
        }
        *decisions = updated;
        let (response, job) = self.start_delegate(
            attempt.workspace.clone(),
            attempt.kind,
            attempt.executable.clone(),
            attempt.prompt.clone(),
        );
        if response["isError"] == false {
            attempt.completed = Some(response.clone());
            if let Some(job) = job {
                launch(job);
            }
        }
        response
    }
    fn revoke_workspace(&self, args: &Value) -> Value {
        let Some(workspace) = args["workspace"]
            .as_str()
            .map(Path::new)
            .filter(|path| path.is_absolute())
            .and_then(|path| path.canonicalize().ok())
            .filter(|path| path.is_dir())
        else {
            return result(json!({"status":"invalid_workspace"}), true);
        };
        let mut trust = self.workspace_trust.lock().unwrap();
        let mut updated = trust.decisions.clone();
        updated.approved.remove(&workspace);
        if updated.revoked.insert(workspace.clone()) {
            if let Err(error) = self.save_workspace_decisions(&updated) {
                return error;
            }
            trust.decisions = updated;
        }
        trust
            .attempts
            .retain(|_, attempt| attempt.workspace != workspace || attempt.completed.is_some());
        result(json!({"status":"revoked", "workspace":workspace}), false)
    }

    fn reconnect_session(&self, args: &Value) -> (Value, Option<Launch>) {
        let Some(id) = args["session_id"].as_str() else {
            return (result(json!("Session required"), true), None);
        };
        let trust = self.workspace_trust.lock().unwrap();
        let mut sessions = self.sessions.lock().unwrap();
        let Some(record) = sessions.get_mut(id) else {
            return (result(json!("Session not found"), true), None);
        };
        let turn_id = match args.get("turn_id") {
            None => record["turn_id"].as_str(),
            Some(id) => id.as_str(),
        };
        let Some(turn_id) = turn_id.map(str::to_owned) else {
            return (result(json!("Invalid turn ID"), true), None);
        };
        if record["restart_blocked"] == true
            || session_has_status(
                record,
                &["running", "cancelling", "completion_pending", "recovering"],
            )
            || record["archived"] == true
            || !turn_state_mut(record, &turn_id).is_some_and(|turn| turn["status"] == "unknown")
        {
            return (
                result(json!("Session does not require recovery"), true),
                None,
            );
        }
        let workspace = match self.session_workspace(&trust.decisions, record) {
            Ok(workspace) => workspace,
            Err(error) => return (error, None),
        };
        let (kind, executable) = match record["agent"].as_str() {
            Some("codex") => (AgentKind::Codex, &self.codex_command),
            Some("opencode") => (AgentKind::OpenCode, &self.opencode_command),
            _ => return (result(json!("Unknown agent"), true), None),
        };
        let (Some(executable), Some(acp_id)) =
            (executable.clone(), record["acp_session_id"].as_str())
        else {
            return (result(json!("Session is not available"), true), None);
        };
        let acp_id = acp_id.to_owned();
        let previous = record.clone();
        turn_state_mut(record, &turn_id).unwrap()["status"] = json!("recovering");
        let mut workers = self.workers.lock().unwrap();
        let action = if let Some(sender) = workers.get(id) {
            Action::Send(
                sender.clone(),
                Work::Reconnect {
                    turn_id: turn_id.clone(),
                },
            )
        } else {
            let (sender, receiver) = mpsc::channel();
            workers.insert(id.to_owned(), sender);
            self.cancel_writers
                .lock()
                .unwrap()
                .insert(id.to_owned(), Arc::new(Mutex::new(None)));
            Action::Start {
                start: Start::Reconnect {
                    acp_id,
                    turn_id: turn_id.clone(),
                },
                workspace,
                receiver,
                workers: self.workers.clone(),
                children: self.children.clone(),
                adapter: Adapter { kind, executable },
            }
        };
        let job = Launch {
            action: Some(action),
            sessions: self.sessions.clone(),
            id: id.to_owned(),
            turn_id: turn_id.clone(),
            previous: Some(previous),
            events: self.events.clone(),
            cancel_writers: self.cancel_writers.clone(),
        };
        (
            result(
                json!({"session_id":id,"turn_id":turn_id,"status":"recovering"}),
                false,
            ),
            Some(job),
        )
    }
    fn follow_up(&self, args: &Value) -> (Value, Option<Launch>) {
        let (Some(id), Some(prompt)) = (
            args["session_id"].as_str(),
            args["prompt"].as_str().filter(|p| !p.is_empty()),
        ) else {
            return (result(json!("Session and prompt required"), true), None);
        };
        let trust = self.workspace_trust.lock().unwrap();
        let mut sessions = self.sessions.lock().unwrap();
        let Some(record) = sessions.get_mut(id) else {
            return (result(json!("Session not found"), true), None);
        };
        if record["restart_blocked"] == true
            || session_has_status(
                record,
                &["running", "cancelling", "completion_pending", "recovering"],
            )
            || record["archived"] == true
        {
            return (result(json!("Session is not available"), true), None);
        }
        if let Err(error) = self.session_workspace(&trust.decisions, record) {
            return (error, None);
        }
        let Some(sender) = self.workers.lock().unwrap().get(id).cloned() else {
            return (result(json!("Session is not available"), true), None);
        };
        let usable = self
            .children
            .lock()
            .unwrap()
            .children
            .get(id)
            .and_then(|children| children.last())
            .map(|child| matches!(child.lock().unwrap().try_wait(), Ok(None)))
            .unwrap_or(false);
        if !usable {
            return (result(json!("Session is not available"), true), None);
        }
        let previous = record.clone();
        let tid = Uuid::new_v4().to_string();
        if record["status"] == "unknown" {
            let unresolved = record["turn_id"].as_str().unwrap().to_owned();
            let history_turn_index = record["history"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["role"] == "user")
                .count()
                - 1;
            record["previous_turns"][&unresolved] = json!({
                "status": "unknown", "stop_reason": null, "result": record["result"],
                "history_turn_index": history_turn_index,
            });
            if record["unresolved_turn_ids"].is_null() {
                record["unresolved_turn_ids"] = json!([]);
            }
            record["unresolved_turn_ids"]
                .as_array_mut()
                .unwrap()
                .push(json!(unresolved));
        }
        record["turn_id"] = json!(tid);
        record["status"] = json!("running");
        record["result"] = json!("");
        record["stop_reason"] = Value::Null;
        record["history"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","text":prompt}));
        let job = Launch {
            action: Some(Action::Send(
                sender,
                Work::Prompt(Turn {
                    id: tid.clone(),
                    prompt: prompt.to_owned(),
                }),
            )),
            sessions: self.sessions.clone(),
            id: id.to_owned(),
            turn_id: tid.clone(),
            previous: Some(previous),
            events: self.events.clone(),
            cancel_writers: self.cancel_writers.clone(),
        };
        (
            result(json!({"session_id":id,"turn_id":tid}), false),
            Some(job),
        )
    }
    fn cancel_turn(&self, args: &Value) -> Value {
        let (Some(id), Some(turn_id)) = (args["session_id"].as_str(), args["turn_id"].as_str())
        else {
            return result(json!("Session and turn required"), true);
        };
        let mut records = self.sessions.lock().unwrap();
        let Some(record) = records.get_mut(id) else {
            return result(json!("Session not found"), true);
        };
        if record["turn_id"] != turn_id
            || record["status"] != "running"
            || record["archived"] == true
        {
            return result(json!("Turn is not available for cancellation"), true);
        }
        record["status"] = json!("cancelling");
        drop(records);
        if let Some(slot) = self.cancel_writers.lock().unwrap().get(id).cloned() {
            schedule_cancel(
                slot,
                self.sessions.clone(),
                id.to_owned(),
                turn_id.to_owned(),
            );
        }
        result(
            json!({"session_id":id,"turn_id":turn_id,"status":"cancelling"}),
            false,
        )
    }
    fn archive_session(&self, args: &Value) -> Value {
        let Some(id) = args["session_id"].as_str() else {
            return result(json!("Session required"), true);
        };
        let mut records = self.sessions.lock().unwrap();
        let Some(record) = records.get_mut(id) else {
            return result(json!("Session not found"), true);
        };
        if !matches!(
            record["status"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) || record["archived"] == true
            || record["unresolved_turn_ids"]
                .as_array()
                .is_some_and(|ids| !ids.is_empty())
        {
            return result(json!("Session cannot be archived"), true);
        }
        record["archived"] = json!(true);
        self.workers.lock().unwrap().remove(id);
        self.cancel_writers.lock().unwrap().remove(id);
        result(json!({"session_id":id,"archived":true}), false)
    }
    fn dispatch(&self, request: Value) -> (Option<Value>, Option<Launch>) {
        let Some(id) = request.get("id") else {
            return (None, None);
        };
        let method = request["method"].as_str().unwrap_or("");
        if let Some(events) = &self.events {
            if let Some(response) = events.dispatch(method, &request["params"]) {
                return (
                    Some(match response {
                        Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
                        Err(error) => json!({"jsonrpc":"2.0","id":id,"error":error}),
                    }),
                    None,
                );
            }
        }
        let sessions = &self.sessions;
        let mut launch = None;
        let response = match method {
            "initialize" => {
                let version = if request["params"]["protocolVersion"] == "2025-03-26" {
                    "2025-03-26"
                } else {
                    "2025-11-25"
                };
                let capabilities = if self.events.is_some() {
                    json!({"tools":{},"events":{}})
                } else {
                    json!({"tools":{}})
                };
                json!({"protocolVersion":version,"capabilities":capabilities,"serverInfo":{"name":"session-scheduler","version":"0.1.0"}})
            }
            "ping" => json!({}),
            "tools/list" => {
                let mut catalog = json!({"tools":[{
                    "name":"delegate",
                    "description":"Delegate work to Codex or OpenCode. If approval_required, show the exact workspace to the user and ask for one explicit yes in this chat. Only after yes call approve_workspace with attempt_id; do nothing on denial or no response.",
                    "inputSchema":{
                        "type":"object",
                        "properties":{
                            "workspace":{"type":"string"},
                            "prompt":{"type":"string"},
                            "agent":{"type":"string","enum":["codex","opencode"],"default":"codex"}
                        },
                        "required":["workspace","prompt"]
                    }
                },{"name":"approve_workspace","description":"After the user explicitly approves the exact workspace in this chat, approve and continue the retained attempt. Retry with the same attempt_id if needed.","inputSchema":{"type":"object","properties":{"attempt_id":{"type":"string"}},"required":["attempt_id"]}},{"name":"revoke_workspace","description":"When the user asks to revoke trust, revoke it for the effective workspace. Existing history remains readable and active turns continue.","inputSchema":{"type":"object","properties":{"workspace":{"type":"string","description":"Absolute path to the workspace"}},"required":["workspace"]}},{"name":"follow_up","description":"Continue an existing session","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"prompt":{"type":"string"}},"required":["session_id","prompt"]}},{"name":"list_sessions","description":"List sessions","inputSchema":{"type":"object","properties":{}}},{"name":"inspect_session","description":"Inspect a session","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"]}}, {"name":"cancel_turn","description":"Request cancellation of a specific turn","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"turn_id":{"type":"string"}},"required":["session_id","turn_id"]}}, {"name":"reconnect_session","description":"Recover an indeterminate turn in the existing session (defaults to the latest turn)","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"turn_id":{"type":"string"}},"required":["session_id"]}}, {"name":"archive_session","description":"Archive a session with a confirmed outcome","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"]}}]});
                if self.events.is_some() {
                    catalog["tools"].as_array_mut().unwrap().push(json!({
                        "name":"import_session",
                        "description":"Register an explicitly supplied Codex or OpenCode native session ID in a trusted workspace without launching the agent. A new reference remains pending confirmation.",
                        "inputSchema":{
                            "type":"object",
                            "properties":{
                                "agent":{"type":"string","enum":["codex","opencode"]},
                                "native_session_id":{"type":"string"},
                                "workspace":{"type":"string","description":"Absolute path to the workspace"}
                            },
                            "required":["agent","native_session_id","workspace"]
                        }
                    }));
                }
                if self.events.is_none() {
                    catalog["tools"][0]["description"] =
                        json!("Delegate work to Codex or OpenCode in a configured workspace");
                    catalog["tools"].as_array_mut().unwrap().retain(|tool| {
                        !matches!(
                            tool["name"].as_str(),
                            Some("approve_workspace" | "revoke_workspace")
                        )
                    });
                }
                catalog
            }
            "tools/call" => {
                let args = &request["params"]["arguments"];
                match request["params"]["name"].as_str().unwrap_or("") {
                    "list_sessions" => result(
                        json!(sessions
                            .lock()
                            .unwrap()
                            .values()
                            .filter(|s| s["archived"] != true)
                            .map(|s| json!({"session_id":s["session_id"],"turn_id":s["turn_id"],"workspace":s["workspace"],"status":s["status"]}))
                            .collect::<Vec<_>>()),
                        false,
                    ),
                    "inspect_session" => match args["session_id"]
                        .as_str()
                        .and_then(|id| sessions.lock().unwrap().get(id).cloned())
                    {
                        Some(s) => result(s, false),
                        None => result(json!("Session not found"), true),
                    },
                    "approve_workspace" if self.events.is_some() => self.approve_workspace(args),
                    "revoke_workspace" if self.events.is_some() => self.revoke_workspace(args),
                    "import_session" if self.events.is_some() => self.import_session(args),
                    "cancel_turn" => self.cancel_turn(args),
                    "archive_session" => self.archive_session(args),
                    "reconnect_session" => {
                        let (response, job) = self.reconnect_session(args);
                        launch = job;
                        response
                    }
                    "follow_up" => {
                        let (response, job) = self.follow_up(args);
                        launch = job;
                        response
                    }
                    "delegate" => {
                        let (response, job) = self.delegate(args);
                        launch = job;
                        response
                    }
                    _ => result(json!("Unknown tool"), true),
                }
            }
            _ => {
                return (
                    Some(
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}}),
                    ),
                    None,
                );
            }
        };
        (
            Some(json!({"jsonrpc":"2.0","id":id,"result":response})),
            launch,
        )
    }
}
fn main() {
    if env::var_os("SESSION_SCHEDULER_HTTP_PORT").is_some() {
        if let Err(error) = http::serve() {
            eprintln!("HTTP server: {error}");
            std::process::exit(1);
        }
        return;
    }
    let state = State::new(None).expect("in-memory scheduler state");
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (response, job) = state.dispatch(request);
        if let Some(response) = response {
            if send(&mut io::stdout().lock(), response).is_err() {
                break;
            }
            if let Some(job) = job {
                launch(job);
            }
        }
    }
}
