//! The database actor: one `std::thread` owns the `rusqlite::Connection`
//! for the lifetime of the process, communicating over a bounded async
//! command channel. Every write command commits its transaction before the
//! actor sends its response, so every public write method here returns only
//! after commit.

use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;

use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use batman_protocol::{OperationId, ProjectId, RunId, TaskId, Timestamp, WorkerId};

use crate::security::redaction::{PersistableEvent, SanitizedJson};

use super::migrations::open_and_migrate;
use super::models::{Diagnostics, OperationIntent, ReplayedEvent};

/// The bound on the actor's command channel. Callers backpressure against
/// this rather than the channel growing without limit.
const COMMAND_CHANNEL_CAPACITY: usize = 32;

/// A boxed, one-shot domain operation dispatched to the actor thread. Takes
/// the owned connection and returns a JSON value describing the committed
/// result (or a [`crate::domain::DomainError`]).
pub type DomainClosure = Box<
    dyn FnOnce(&mut Connection) -> Result<serde_json::Value, crate::domain::DomainError>
        + Send
        + 'static,
>;

/// Errors returned by the database actor.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// A `rusqlite` (SQLite) error.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A migration failed to apply.
    #[error("migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),

    /// The database file could not be created or secured privately.
    #[error(transparent)]
    Security(#[from] crate::security::SecurityError),

    /// A timestamp read back from the database was not valid RFC 3339.
    #[error("invalid timestamp stored in database: {0}")]
    InvalidTimestamp(String),

    /// An identifier read back from the database was not a valid UUID.
    #[error("invalid identifier stored in database: {0}")]
    InvalidId(String),

    /// `acknowledge_operation` was called for an operation id that has no
    /// recorded intent.
    #[error("no operation intent found for operation {0}")]
    UnknownOperation(String),
    /// The actor thread could not be spawned.
    #[error("failed to spawn database actor thread: {0}")]
    ThreadSpawn(#[from] std::io::Error),
    #[error("pruning events failed: {0}")]
    PruneFailed(String),
    /// The actor thread is no longer running (e.g. it has already shut
    /// down), so the command could not be delivered or answered.
    #[error("database actor is not running")]
    ActorUnavailable,
}

/// A command sent to the actor thread, carrying a `oneshot` reply channel.
enum Command {
    AppendEvent {
        event: PersistableEvent,
        respond: oneshot::Sender<Result<u64, DbError>>,
    },
    ReplayEvents {
        after_sequence: u64,
        respond: oneshot::Sender<Result<Vec<ReplayedEvent>, DbError>>,
    },
    MaxSequence {
        respond: oneshot::Sender<Result<Option<u64>, DbError>>,
    },
    RecordOperationIntent {
        operation_id: OperationId,
        kind: String,
        intent_json: SanitizedJson,
        requested_at: Timestamp,
        respond: oneshot::Sender<Result<(), DbError>>,
    },
    AcknowledgeOperation {
        operation_id: OperationId,
        acknowledgement_json: SanitizedJson,
        respond: oneshot::Sender<Result<(), DbError>>,
    },
    IncompleteOperations {
        respond: oneshot::Sender<Result<Vec<OperationIntent>, DbError>>,
    },
    Diagnostics {
        respond: oneshot::Sender<Result<Diagnostics, DbError>>,
    },
    /// Runs an arbitrary domain operation against the owned connection on
    /// the actor thread. The closure receives `&mut Connection` so it can
    /// open its own transaction (append event + update projection) and
    /// returns a JSON value describing the committed result.
    DomainOp {
        op: DomainClosure,
        respond: oneshot::Sender<Result<serde_json::Value, crate::domain::DomainError>>,
    },
    Shutdown {
        respond: oneshot::Sender<Result<(), DbError>>,
    },
}

/// A handle to the running database actor. Cheap to hold and safe to share
/// behind an [`std::sync::Arc`]: every method sends a command over a bounded
/// channel and awaits the actor's reply, and [`DatabaseHandle::shutdown`] takes
/// `&self` so the clean drain-and-join runs even while other clones of the
/// handle are still live (it does not require unique `Arc` ownership).
pub struct DatabaseHandle {
    sender: mpsc::Sender<Command>,
    /// The actor's OS thread, taken exactly once by whichever call to
    /// [`DatabaseHandle::shutdown`] joins it. Behind a `Mutex<Option<..>>` so
    /// the join can happen through a shared `&self` rather than requiring the
    /// handle to be uniquely owned.
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl DatabaseHandle {
    /// Spawns the single owner thread for the database at `path`, opening
    /// it (creating it privately, mode `0600`, if missing), configuring its
    /// PRAGMAs, and migrating it to the latest schema before returning.
    ///
    /// # Errors
    /// Returns [`DbError`] if the actor thread cannot be spawned, or if
    /// opening/configuring/migrating the database fails.
    pub async fn start(path: impl Into<PathBuf>) -> Result<Self, DbError> {
        let path = path.into();
        let (sender, receiver) = mpsc::channel::<Command>(COMMAND_CHANNEL_CAPACITY);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), DbError>>();

        let worker = thread::Builder::new()
            .name("crew-db-actor".to_string())
            .spawn(move || run_actor(path, receiver, ready_tx))
            .map_err(DbError::ThreadSpawn)?;

        ready_rx.await.map_err(|_| DbError::ActorUnavailable)??;

        Ok(Self {
            sender,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// Appends a sanitized event to the journal and returns its assigned
    /// sequence number. Returns only after the insert transaction commits.
    ///
    /// # Errors
    /// Returns [`DbError`] if the actor is unavailable or the write fails.
    pub async fn append_event(&self, event: PersistableEvent) -> Result<u64, DbError> {
        let (respond, rx) = oneshot::channel();
        self.send(Command::AppendEvent { event, respond }).await?;
        rx.await.map_err(|_| DbError::ActorUnavailable)?
    }

    /// Returns every event with `sequence > after_sequence`, in ascending
    /// sequence order.
    ///
    /// # Errors
    /// Returns [`DbError`] if the actor is unavailable or the query fails.
    pub async fn replay_events(&self, after_sequence: u64) -> Result<Vec<ReplayedEvent>, DbError> {
        let (respond, rx) = oneshot::channel();
        self.send(Command::ReplayEvents {
            after_sequence,
            respond,
        })
        .await?;
        rx.await.map_err(|_| DbError::ActorUnavailable)?
    }

    /// Returns the highest event sequence currently in the journal, or `None`
    /// if the journal holds no events. A single indexed `MAX(sequence)` read --
    /// used at `initialize` to compute the next sequence a client should
    /// expect without loading the entire event log into memory.
    ///
    /// # Errors
    /// Returns [`DbError`] if the actor is unavailable or the query fails.
    pub async fn max_sequence(&self) -> Result<Option<u64>, DbError> {
        let (respond, rx) = oneshot::channel();
        self.send(Command::MaxSequence { respond }).await?;
        rx.await.map_err(|_| DbError::ActorUnavailable)?
    }

    /// Persists an operation's intent before its side effect runs. Returns
    /// only after the insert transaction commits.
    ///
    /// `intent_json` must be a [`SanitizedJson`] -- obtainable only via
    /// [`crate::security::redaction::Redactor::sanitize_json`] -- so
    /// unsanitized content cannot reach this durable table.
    ///
    /// # Errors
    /// Returns [`DbError`] if the actor is unavailable or the write fails.
    pub async fn record_operation_intent(
        &self,
        operation_id: OperationId,
        kind: impl Into<String>,
        intent_json: SanitizedJson,
        requested_at: Timestamp,
    ) -> Result<(), DbError> {
        let (respond, rx) = oneshot::channel();
        self.send(Command::RecordOperationIntent {
            operation_id,
            kind: kind.into(),
            intent_json,
            requested_at,
            respond,
        })
        .await?;
        rx.await.map_err(|_| DbError::ActorUnavailable)?
    }

    /// Records an operation's acknowledgement. Returns only after the
    /// update transaction commits.
    ///
    /// `acknowledgement` must be a [`SanitizedJson`] -- obtainable only via
    /// [`crate::security::redaction::Redactor::sanitize_json`] -- so
    /// unsanitized content cannot reach this durable table.
    ///
    /// # Errors
    /// Returns [`DbError::UnknownOperation`] if no intent was recorded for
    /// `operation_id`, or another [`DbError`] if the actor is unavailable or
    /// the write fails.
    pub async fn acknowledge_operation(
        &self,
        operation_id: OperationId,
        acknowledgement: SanitizedJson,
    ) -> Result<(), DbError> {
        let (respond, rx) = oneshot::channel();
        self.send(Command::AcknowledgeOperation {
            operation_id,
            acknowledgement_json: acknowledgement,
            respond,
        })
        .await?;
        rx.await.map_err(|_| DbError::ActorUnavailable)?
    }

    /// Returns every recorded operation intent that has not yet been
    /// acknowledged -- the recovery set to reconcile after a restart.
    ///
    /// # Errors
    /// Returns [`DbError`] if the actor is unavailable or the query fails.
    pub async fn incomplete_operations(&self) -> Result<Vec<OperationIntent>, DbError> {
        let (respond, rx) = oneshot::channel();
        self.send(Command::IncompleteOperations { respond }).await?;
        rx.await.map_err(|_| DbError::ActorUnavailable)?
    }

    /// Returns a snapshot of the actor's own connection PRAGMAs and schema,
    /// proving they took effect rather than merely having been requested.
    ///
    /// # Errors
    /// Returns [`DbError`] if the actor is unavailable or the query fails.
    pub async fn diagnostics(&self) -> Result<Diagnostics, DbError> {
        let (respond, rx) = oneshot::channel();
        self.send(Command::Diagnostics { respond }).await?;
        rx.await.map_err(|_| DbError::ActorUnavailable)?
    }

    /// Signals the actor to drain outstanding commands, commit, and stop, then
    /// joins its OS thread so the connection is closed (and WAL checkpointed)
    /// before returning. Takes `&self`, so a runtime holding this handle behind
    /// an [`std::sync::Arc`] -- shared with live connection tasks -- can still
    /// drive a clean shutdown without needing to reclaim unique ownership
    /// first. The thread is joined at most once; later calls are no-ops for the
    /// join and simply report the actor as unavailable.
    ///
    /// The blocking `JoinHandle::join` runs on a [`tokio::task::spawn_blocking`]
    /// thread so it never blocks the async runtime's worker.
    ///
    /// # Errors
    /// Returns [`DbError`] if the actor was already unavailable.
    pub async fn shutdown(&self) -> Result<(), DbError> {
        let (respond, rx) = oneshot::channel();
        let sent = self.sender.send(Command::Shutdown { respond }).await;

        // Never short-circuit past the join below: a dropped ack channel
        // means the actor died abnormally (panic), which is precisely when
        // the join must still run -- it reaps the thread and surfaces the
        // panic instead of leaking the JoinHandle (R66). Mirrors the
        // `sent.is_err()` branch, which already falls through.
        let result = if sent.is_ok() {
            match rx.await {
                Ok(inner) => inner,
                Err(_) => Err(DbError::ActorUnavailable),
            }
        } else {
            Err(DbError::ActorUnavailable)
        };

        let worker = self
            .worker
            .lock()
            .expect("db actor worker mutex is never poisoned")
            .take();
        if let Some(worker) = worker {
            match tokio::task::spawn_blocking(move || worker.join()).await {
                Ok(Err(panic)) => {
                    tracing::error!(?panic, "database actor thread panicked before shutdown");
                }
                Err(join_err) => {
                    tracing::warn!(error = %join_err, "joining the database actor thread was itself cancelled");
                }
                Ok(Ok(())) => {}
            }
        }

        result
    }

    /// Runs a boxed domain operation against the owned connection on the
    /// actor thread. The closure opens its own transaction (append event +
    /// update projection) and commits before this returns.
    ///
    /// # Errors
    /// Returns [`crate::domain::DomainError::ActorUnavailable`] if the actor
    /// is not running, or whatever error the closure itself returns.
    pub async fn run_domain_op(
        &self,
        op: DomainClosure,
    ) -> Result<serde_json::Value, crate::domain::DomainError> {
        let (respond, rx) = oneshot::channel();
        self.sender
            .send(Command::DomainOp { op, respond })
            .await
            .map_err(|_| crate::domain::DomainError::ActorUnavailable)?;
        rx.await
            .map_err(|_| crate::domain::DomainError::ActorUnavailable)?
    }

    async fn send(&self, command: Command) -> Result<(), DbError> {
        self.sender
            .send(command)
            .await
            .map_err(|_| DbError::ActorUnavailable)
    }
}

/// The actor's body: opens (and migrates) the database, signals readiness,
/// then services commands from the bounded channel until it is told to
/// shut down or the channel closes. Runs on its own `std::thread`, so
/// blocking on the channel is the correct way to wait for work.
fn run_actor(
    path: PathBuf,
    mut receiver: mpsc::Receiver<Command>,
    ready: oneshot::Sender<Result<(), DbError>>,
) {
    let mut conn = match open_and_migrate(&path) {
        Ok(conn) => {
            let _ = ready.send(Ok(()));
            conn
        }
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };

    while let Some(command) = receiver.blocking_recv() {
        match command {
            Command::AppendEvent { event, respond } => {
                let _ = respond.send(tx_append_event(&mut conn, &event));
            }
            Command::ReplayEvents {
                after_sequence,
                respond,
            } => {
                let _ = respond.send(tx_replay_events(&conn, after_sequence));
            }
            Command::MaxSequence { respond } => {
                let _ = respond.send(tx_max_sequence(&conn));
            }
            Command::RecordOperationIntent {
                operation_id,
                kind,
                intent_json,
                requested_at,
                respond,
            } => {
                let _ = respond.send(tx_record_operation_intent(
                    &mut conn,
                    &operation_id,
                    &kind,
                    intent_json.as_str(),
                    &requested_at,
                ));
            }
            Command::AcknowledgeOperation {
                operation_id,
                acknowledgement_json,
                respond,
            } => {
                let acknowledged_at = Timestamp::now();
                let _ = respond.send(tx_acknowledge_operation(
                    &mut conn,
                    &operation_id,
                    acknowledgement_json.as_str(),
                    &acknowledged_at,
                ));
            }
            Command::IncompleteOperations { respond } => {
                let _ = respond.send(tx_incomplete_operations(&conn));
            }
            Command::Diagnostics { respond } => {
                let _ = respond.send(tx_diagnostics(&conn));
            }
            Command::DomainOp { op, respond } => {
                let _ = respond.send(op(&mut conn));
            }
            Command::Shutdown { respond } => {
                let _ = respond.send(Ok(()));
                break;
            }
        }
    }
}

/// Inserts a sanitized event and returns its assigned sequence number
/// (the SQLite rowid, since `sequence INTEGER PRIMARY KEY` is a rowid
/// alias). Wrapped in an explicit transaction so future projection updates
/// can be added to the same commit.
fn tx_append_event(conn: &mut Connection, event: &PersistableEvent) -> Result<u64, DbError> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO events (timestamp, project_id, run_id, event_json) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            event.timestamp().as_str(),
            event.project_id().to_string(),
            event.run_id().map(std::string::ToString::to_string),
            event.event_json(),
        ],
    )?;
    let sequence = tx.last_insert_rowid();
    tx.commit()?;
    // Metadata only -- the sequence number -- never event text/payload
    // fields, which may have crossed the redaction boundary but must still
    // never be duplicated into the log.
    tracing::debug!(sequence, "event appended");
    Ok(sequence as u64)
}

struct RawEventRow {
    sequence: i64,
    timestamp: String,
    project_id: String,
    run_id: Option<String>,
    task_id: Option<String>,
    worker_id: Option<String>,
    event_json: String,
}

fn tx_replay_events(conn: &Connection, after_sequence: u64) -> Result<Vec<ReplayedEvent>, DbError> {
    let mut statement = conn.prepare(
        "SELECT sequence, timestamp, project_id, run_id, task_id, worker_id, event_json \
         FROM events WHERE sequence > ?1 ORDER BY sequence ASC",
    )?;
    let after_sequence = i64::try_from(after_sequence).unwrap_or(i64::MAX);
    let rows = statement.query_map(rusqlite::params![after_sequence], |row| {
        Ok(RawEventRow {
            sequence: row.get(0)?,
            timestamp: row.get(1)?,
            project_id: row.get(2)?,
            run_id: row.get(3)?,
            task_id: row.get(4)?,
            worker_id: row.get(5)?,
            event_json: row.get(6)?,
        })
    })?;

    let mut events = Vec::new();
    for row in rows {
        events.push(parse_event_row(row?)?);
    }
    Ok(events)
}

/// Reads `MAX(sequence)` from the events table. Returns `None` on an empty
/// journal (SQLite yields a single `NULL` row for `MAX` over no rows).
fn tx_max_sequence(conn: &Connection) -> Result<Option<u64>, DbError> {
    let max: Option<i64> =
        conn.query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))?;
    Ok(max.map(|value| value as u64))
}

fn parse_event_row(row: RawEventRow) -> Result<ReplayedEvent, DbError> {
    Ok(ReplayedEvent {
        sequence: row.sequence as u64,
        timestamp: Timestamp::parse(&row.timestamp)
            .map_err(|err| DbError::InvalidTimestamp(err.to_string()))?,
        project_id: ProjectId::parse(&row.project_id)
            .map_err(|err| DbError::InvalidId(err.to_string()))?,
        task_id: row
            .task_id
            .map(|value| TaskId::parse(&value))
            .transpose()
            .map_err(|err| DbError::InvalidId(err.to_string()))?,
        worker_id: row
            .worker_id
            .map(|value| WorkerId::parse(&value))
            .transpose()
            .map_err(|err| DbError::InvalidId(err.to_string()))?,
        run_id: row
            .run_id
            .map(|value| RunId::parse(&value))
            .transpose()
            .map_err(|err| DbError::InvalidId(err.to_string()))?,
        event_json: row.event_json,
    })
}

fn tx_record_operation_intent(
    conn: &mut Connection,
    operation_id: &OperationId,
    kind: &str,
    intent_json: &str,
    requested_at: &Timestamp,
) -> Result<(), DbError> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO operations (operation_id, kind, intent_json, requested_at) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            operation_id.to_string(),
            kind,
            intent_json,
            requested_at.as_str(),
        ],
    )?;
    tx.commit()?;
    Ok(())
}

fn tx_acknowledge_operation(
    conn: &mut Connection,
    operation_id: &OperationId,
    acknowledgement_json: &str,
    acknowledged_at: &Timestamp,
) -> Result<(), DbError> {
    let tx = conn.transaction()?;
    let updated = tx.execute(
        "UPDATE operations SET acknowledged_at = ?1, acknowledgement_json = ?2 \
         WHERE operation_id = ?3",
        rusqlite::params![
            acknowledged_at.as_str(),
            acknowledgement_json,
            operation_id.to_string(),
        ],
    )?;
    if updated == 0 {
        return Err(DbError::UnknownOperation(operation_id.to_string()));
    }
    tx.commit()?;
    Ok(())
}

struct RawOperationRow {
    operation_id: String,
    kind: String,
    intent_json: String,
    requested_at: String,
}

fn tx_incomplete_operations(conn: &Connection) -> Result<Vec<OperationIntent>, DbError> {
    let mut statement = conn.prepare(
        "SELECT operation_id, kind, intent_json, requested_at FROM operations \
         WHERE acknowledged_at IS NULL ORDER BY requested_at ASC",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(RawOperationRow {
            operation_id: row.get(0)?,
            kind: row.get(1)?,
            intent_json: row.get(2)?,
            requested_at: row.get(3)?,
        })
    })?;

    let mut intents = Vec::new();
    for row in rows {
        intents.push(parse_operation_row(row?)?);
    }
    Ok(intents)
}

fn parse_operation_row(row: RawOperationRow) -> Result<OperationIntent, DbError> {
    Ok(OperationIntent {
        operation_id: OperationId::parse(&row.operation_id)
            .map_err(|err| DbError::InvalidId(err.to_string()))?,
        kind: row.kind,
        intent_json: row.intent_json,
        requested_at: Timestamp::parse(&row.requested_at)
            .map_err(|err| DbError::InvalidTimestamp(err.to_string()))?,
    })
}

fn tx_diagnostics(conn: &Connection) -> Result<Diagnostics, DbError> {
    let journal_mode: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    let foreign_keys: i64 = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    let busy_timeout: i64 = conn.pragma_query_value(None, "busy_timeout", |row| row.get(0))?;
    let synchronous: i64 = conn.pragma_query_value(None, "synchronous", |row| row.get(0))?;

    let mut statement =
        conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Diagnostics {
        journal_mode,
        foreign_keys: foreign_keys != 0,
        busy_timeout,
        synchronous,
        tables,
    })
}
