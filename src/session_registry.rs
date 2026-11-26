use crate::{acp::known_stop_reason, state_files::StateFiles};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env, io,
    ops::Deref,
    sync::{Arc, Mutex, MutexGuard},
};

type SessionRecords = BTreeMap<String, Value>;
pub(crate) type Sessions = Arc<SessionRegistry>;
pub(crate) struct SessionRegistry {
    records: Mutex<SessionRecords>,
    database: Option<Mutex<Connection>>,
    _state: Option<StateFiles>,
}
pub(crate) struct SessionGuard<'a> {
    records: MutexGuard<'a, SessionRecords>,
    database: &'a Option<Mutex<Connection>>,
    before: BTreeMap<String, Option<Value>>,
}
impl Deref for SessionGuard<'_> {
    type Target = SessionRecords;
    fn deref(&self) -> &Self::Target {
        &self.records
    }
}
impl SessionGuard<'_> {
    fn remember(&mut self, id: &str) {
        if self.database.is_some() && !self.before.contains_key(id) {
            self.before
                .insert(id.to_owned(), self.records.get(id).map(registry_metadata));
        }
    }
    pub(crate) fn get_mut(&mut self, id: &str) -> Option<&mut Value> {
        self.remember(id);
        self.records.get_mut(id)
    }
    pub(crate) fn insert(&mut self, id: String, record: Value) -> Option<Value> {
        self.remember(&id);
        self.records.insert(id, record)
    }
    pub(crate) fn remove(&mut self, id: &str) -> Option<Value> {
        self.remember(id);
        self.records.remove(id)
    }
}
impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        if self.before.is_empty() {
            return;
        }
        if let Some(database) = self.database {
            if let Err(error) =
                save_registry(&mut database.lock().unwrap(), &self.records, &self.before)
            {
                // Never acknowledge or launch work after losing durable registry state.
                eprintln!("session registry unavailable: {error}");
                std::process::exit(1);
            }
        }
    }
}
impl SessionRegistry {
    pub(crate) fn lock(&self) -> Result<SessionGuard<'_>, ()> {
        self.records
            .lock()
            .map(|records| SessionGuard {
                records,
                database: &self.database,
                before: BTreeMap::new(),
            })
            .map_err(|_| ())
    }
    pub(crate) fn new(persistent: bool) -> io::Result<Self> {
        if !persistent {
            return Ok(Self {
                records: Mutex::new(BTreeMap::new()),
                database: None,
                _state: None,
            });
        }
        let directory = env::var_os("SESSION_SCHEDULER_STATE_DIR")
            .ok_or_else(|| io::Error::other("state directory required"))?;
        let mut state = StateFiles::open(directory.into())?;
        let database = Connection::open_with_flags(
            state.database_path()?,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(io::Error::other)?;
        database
            .execute_batch(
                "PRAGMA synchronous=FULL;
                 CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, metadata TEXT NOT NULL);",
            )
            .map_err(io::Error::other)?;
        let records = load_records(&database).map_err(io::Error::other)?;
        Ok(Self {
            records: Mutex::new(records),
            database: Some(Mutex::new(database)),
            _state: Some(state),
        })
    }
}

fn load_records(
    database: &Connection,
) -> Result<SessionRecords, Box<dyn std::error::Error + Send + Sync>> {
    let mut statement = database.prepare("SELECT id, metadata FROM sessions")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut records = BTreeMap::new();
    for row in rows {
        let (id, metadata) = row?;
        let mut record: Value = serde_json::from_str(&metadata)?;
        let unresolved = record["unresolved_turn_ids"]
            .as_array()
            .is_some_and(|ids| !ids.is_empty());
        let active = !matches!(
            record["status"].as_str(),
            Some("completed" | "failed" | "cancelled" | "idle")
        );
        if active {
            record["status"] = json!("pending_confirmation");
        }
        record["restart_blocked"] = json!(active || unresolved);
        record["history"] = json!([]);
        record["result"] = json!("");
        records.insert(id, record);
    }
    Ok(records)
}
fn registry_metadata(record: &Value) -> Value {
    json!({
        "session_id": record["session_id"], "turn_id": record["turn_id"],
        "workspace": record["workspace"], "agent": record["agent"],
        "acp_session_id": record["acp_session_id"], "status": record["status"],
        "stop_reason": record["stop_reason"].as_str().filter(|reason| known_stop_reason(reason)),
        "archived": record["archived"], "unresolved_turn_ids": record["unresolved_turn_ids"],
        "reconciliation": record["reconciliation"]
    })
}
fn save_registry(
    database: &mut Connection,
    records: &SessionRecords,
    before: &BTreeMap<String, Option<Value>>,
) -> rusqlite::Result<()> {
    let changed: Vec<_> = before
        .iter()
        .filter_map(|(id, old)| {
            let current = records.get(id).map(registry_metadata);
            (current != *old).then_some((id, current))
        })
        .collect();
    if changed.is_empty() {
        return Ok(());
    }
    let transaction = database.transaction()?;
    for (id, metadata) in changed {
        match metadata {
            Some(metadata) => {
                transaction.execute("INSERT INTO sessions (id, metadata) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET metadata = excluded.metadata", rusqlite::params![id, metadata.to_string()])?;
            }
            None => {
                transaction.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
            }
        }
    }
    transaction.commit()
}
