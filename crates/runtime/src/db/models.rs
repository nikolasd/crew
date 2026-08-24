//! Plain data types returned by the database actor. These carry only
//! already-durable, already-sanitized data -- never raw or classified
//! content.

use crew_protocol::{OperationId, ProjectId, RunId, TaskId, Timestamp, WorkerId};

/// A durable event fetched via [`crate::db::DatabaseHandle::replay_events`],
/// exactly as it was stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedEvent {
    pub sequence: u64,
    pub timestamp: Timestamp,
    pub project_id: ProjectId,
    pub task_id: Option<TaskId>,
    pub worker_id: Option<WorkerId>,
    pub run_id: Option<RunId>,
    /// The sanitized event body, as JSON text (the `event_json` column).
    pub event_json: String,
}

/// An operation's persisted intent, returned by
/// [`crate::db::DatabaseHandle::incomplete_operations`] when it has not yet
/// been acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationIntent {
    pub operation_id: OperationId,
    pub kind: String,
    pub intent_json: String,
    pub requested_at: Timestamp,
}

/// A snapshot of the actor's own connection: proves the configured PRAGMAs
/// and migrated schema actually took effect, rather than merely having been
/// requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostics {
    pub journal_mode: String,
    pub foreign_keys: bool,
    pub busy_timeout: i64,
    pub synchronous: i64,
    pub tables: Vec<String>,
}
