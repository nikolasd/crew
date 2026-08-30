//! Read-only projection queries, built as [`DomainClosure`]s so they run on
//! the database actor thread alongside every mutating command.

use crew_protocol::{ApprovalId, MessageId, ProjectId, RunId, TaskId, WorkerId};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

use crate::db::DomainClosure;
use crate::domain::DomainError;

pub fn task_get_op(task_id: TaskId) -> DomainClosure {
    Box::new(move |conn| {
        conn.query_row(
            "SELECT task_id, project_id, owner_client_instance_id, revision, created_at, updated_at
             FROM tasks WHERE task_id = ?1",
            [task_id.to_string()],
            |row| {
                Ok(json!({
                    "taskId": row.get::<_, String>(0)?,
                    "projectId": row.get::<_, String>(1)?,
                    "ownerClientInstanceId": row.get::<_, String>(2)?,
                    "revision": row.get::<_, i64>(3)?,
                    "createdAt": row.get::<_, String>(4)?,
                    "updatedAt": row.get::<_, String>(5)?,
                }))
            },
        )
        .optional()
        .map_err(DomainError::Sqlite)?
        .ok_or(DomainError::NotFound {
            kind: "task",
            id: task_id.to_string(),
        })
    })
}

/// Reads a run's current lifecycle state, for the adapter layer's
/// evidence-driven transitions (`crate::adapter::run_lifecycle`) and the
/// coordination broker's settled-run gate.
pub fn run_state_op(run_id: RunId) -> DomainClosure {
    Box::new(move |conn| {
        conn.query_row(
            "SELECT state FROM runs WHERE run_id = ?1",
            [run_id.to_string()],
            |row| Ok(json!({ "state": row.get::<_, String>(0)? })),
        )
        .optional()
        .map_err(DomainError::Sqlite)?
        .ok_or(DomainError::NotFound {
            kind: "run",
            id: run_id.to_string(),
        })
    })
}

pub fn worker_get_op(worker_id: WorkerId) -> DomainClosure {
    Box::new(move |conn| {
        conn.query_row(
            "SELECT w.worker_id, w.project_id, w.parent_worker_id, w.created_at,
                    p.id, p.fingerprint, p.adapter, p.model, p.permission_envelope
             FROM workers w JOIN worker_profiles p ON w.profile_id = p.id
             WHERE w.worker_id = ?1",
            [worker_id.to_string()],
            |row| {
                Ok(json!({
                    "workerId": row.get::<_, String>(0)?,
                    "projectId": row.get::<_, String>(1)?,
                    "parentWorkerId": row.get::<_, Option<String>>(2)?,
                    "createdAt": row.get::<_, String>(3)?,
                    "profileRef": {
                        "id": row.get::<_, String>(4)?,
                        "fingerprint": row.get::<_, String>(5)?,
                        "adapter": row.get::<_, String>(6)?,
                        "model": row.get::<_, String>(7)?,
                        "permissionEnvelope": serde_json::from_str::<Value>(&row.get::<_, String>(8)?).unwrap_or(Value::Null),
                    }
                }))
            },
        )
        .optional()
        .map_err(DomainError::Sqlite)?
        .ok_or(DomainError::NotFound {
            kind: "worker",
            id: worker_id.to_string(),
        })
    })
}

pub fn worker_list_op(project_id: ProjectId) -> DomainClosure {
    Box::new(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT w.worker_id, w.parent_worker_id, w.created_at,
                    p.id, p.fingerprint, p.adapter, p.model
             FROM workers w JOIN worker_profiles p ON w.profile_id = p.id
             WHERE w.project_id = ?1 ORDER BY w.created_at",
        )?;
        let rows = stmt
            .query_map([project_id.to_string()], |row| {
                Ok(json!({
                    "workerId": row.get::<_, String>(0)?,
                    "parentWorkerId": row.get::<_, Option<String>>(1)?,
                    "createdAt": row.get::<_, String>(2)?,
                    "profileRef": {
                        "id": row.get::<_, String>(3)?,
                        "fingerprint": row.get::<_, String>(4)?,
                        "adapter": row.get::<_, String>(5)?,
                        "model": row.get::<_, String>(6)?,
                    }
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({ "workers": rows }))
    })
}

pub fn run_get_op(run_id: RunId) -> DomainClosure {
    Box::new(move |conn| {
        conn.query_row(
            "SELECT run_id, task_id, worker_id, state,
                    flags_degraded_control, flags_needs_reconciliation, flags_protocol_unhealthy,
                    flags_policy_quarantined, flags_workspace_dirty, flags_children_active,
                    vendor_session_id, created_at, started_at, completed_at, policy_fingerprint,
                    flags_turn_settled
             FROM runs WHERE run_id = ?1",
            [run_id.to_string()],
            row_to_run_json,
        )
        .optional()
        .map_err(DomainError::Sqlite)?
        .ok_or(DomainError::NotFound {
            kind: "run",
            id: run_id.to_string(),
        })
    })
}

pub fn run_list_op(task_id: Option<TaskId>, project_id: ProjectId) -> DomainClosure {
    Box::new(move |conn| {
        let rows = if let Some(task_id) = task_id {
            let mut stmt = conn.prepare(
                "SELECT run_id, task_id, worker_id, state,
                        flags_degraded_control, flags_needs_reconciliation, flags_protocol_unhealthy,
                        flags_policy_quarantined, flags_workspace_dirty, flags_children_active,
                        vendor_session_id, created_at, started_at, completed_at,
                        policy_fingerprint, flags_turn_settled
                 FROM runs WHERE task_id = ?1 ORDER BY created_at",
            )?;
            stmt.query_map([task_id.to_string()], row_to_run_json)?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            let mut stmt = conn.prepare(
                "SELECT r.run_id, r.task_id, r.worker_id, r.state,
                        r.flags_degraded_control, r.flags_needs_reconciliation, r.flags_protocol_unhealthy,
                        r.flags_policy_quarantined, r.flags_workspace_dirty, r.flags_children_active,
                        r.vendor_session_id, r.created_at, r.started_at, r.completed_at,
                        r.policy_fingerprint, r.flags_turn_settled
                 FROM runs r JOIN tasks t ON r.task_id = t.task_id
                 WHERE t.project_id = ?1 ORDER BY r.created_at",
            )?;
            stmt.query_map([project_id.to_string()], row_to_run_json)?
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(json!({ "runs": rows }))
    })
}

/// Lists every recorded policy violation for `project_id`, newest first,
/// with its decision state -- the discovery surface for "which violation
/// still holds this run's quarantine" (R80). Project-wide like the other
/// read ops (`run_list_op`, `approval_list_op`); optionally narrowed to
/// one run.
pub fn policy_violation_list_op(run_id: Option<RunId>, project_id: ProjectId) -> DomainClosure {
    Box::new(move |conn| {
        let base = "SELECT v.violation_id, v.run_id, v.task_id, v.worker_id,
                           v.vendor_child_id, v.vendor_parent_ref, v.action, v.created_at,
                           v.resolved_at, v.resolution, v.resolved_by
                    FROM policy_violations v
                    JOIN tasks t ON v.task_id = t.task_id
                    WHERE t.project_id = ?1";
        let row_to_json = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Value> {
            Ok(json!({
                "violationId": row.get::<_, String>(0)?,
                "runId": row.get::<_, String>(1)?,
                "taskId": row.get::<_, String>(2)?,
                "workerId": row.get::<_, String>(3)?,
                "vendorChildId": row.get::<_, Option<String>>(4)?,
                "vendorParentRef": row.get::<_, Option<String>>(5)?,
                "action": row.get::<_, String>(6)?,
                "createdAt": row.get::<_, String>(7)?,
                "resolvedAt": row.get::<_, Option<String>>(8)?,
                "resolution": row.get::<_, Option<String>>(9)?,
                "resolvedBy": row.get::<_, Option<String>>(10)?,
            }))
        };
        let rows = if let Some(run_id) = run_id {
            let sql = format!("{base} AND v.run_id = ?2 ORDER BY v.created_at DESC");
            let mut stmt = conn.prepare(&sql)?;
            stmt.query_map(
                rusqlite::params![project_id.to_string(), run_id.to_string()],
                row_to_json,
            )?
            .collect::<Result<Vec<_>, _>>()?
        } else {
            let sql = format!("{base} ORDER BY v.created_at DESC");
            let mut stmt = conn.prepare(&sql)?;
            stmt.query_map([project_id.to_string()], row_to_json)?
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(json!({ "violations": rows }))
    })
}

fn row_to_run_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "runId": row.get::<_, String>(0)?,
        "taskId": row.get::<_, String>(1)?,
        "workerId": row.get::<_, String>(2)?,
        "state": row.get::<_, String>(3)?,
        "flags": {
            "degradedControl": row.get::<_, i64>(4)? != 0,
            "needsReconciliation": row.get::<_, i64>(5)? != 0,
            "protocolUnhealthy": row.get::<_, i64>(6)? != 0,
            "policyQuarantined": row.get::<_, i64>(7)? != 0,
            "workspaceDirty": row.get::<_, i64>(8)? != 0,
            "childrenActive": row.get::<_, i64>(9)? != 0,
            // Appended at index 15 rather than inserted beside the other
            // flags: every index below is positional, and shifting them to
            // keep the flags adjacent would be a silent, wide-blast-radius
            // edit for cosmetic grouping.
            "turnSettled": row.get::<_, i64>(15)? != 0,
        },
        "vendorSessionId": row.get::<_, Option<String>>(10)?,
        "createdAt": row.get::<_, String>(11)?,
        "startedAt": row.get::<_, Option<String>>(12)?,
        "completedAt": row.get::<_, Option<String>>(13)?,
        // The immutable snapshot of the merged policy this run was
        // authorized under, so a later violation is auditable against the
        // exact merge that permitted it. `None` for runs created without a
        // merged startup config (tests and embeddings).
        "policyFingerprint": row.get::<_, Option<String>>(14)?,
    }))
}

pub fn message_list_op(run_id: RunId) -> DomainClosure {
    Box::new(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT message_id, run_id, sender_worker_id, recipient_worker_id, task_id, kind,
                    payload, delivery_state, created_at, sent_at, acknowledged_at, reply_to
             FROM messages WHERE run_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map([run_id.to_string()], |row| {
                Ok(json!({
                    "messageId": row.get::<_, String>(0)?,
                    "runId": row.get::<_, String>(1)?,
                    "senderWorkerId": row.get::<_, String>(2)?,
                    "recipientWorkerId": row.get::<_, Option<String>>(3)?,
                    "taskId": row.get::<_, String>(4)?,
                    "kind": row.get::<_, String>(5)?,
                    "payload": row.get::<_, String>(6)?,
                    "deliveryState": row.get::<_, String>(7)?,
                    "createdAt": row.get::<_, String>(8)?,
                    "sentAt": row.get::<_, Option<String>>(9)?,
                    "acknowledgedAt": row.get::<_, Option<String>>(10)?,
                    "replyTo": row.get::<_, Option<String>>(11)?,
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({ "messages": rows }))
    })
}

pub fn approval_list_op(run_id: Option<RunId>) -> DomainClosure {
    Box::new(move |conn| {
        let rows = if let Some(run_id) = run_id {
            let mut stmt = conn.prepare(
                "SELECT approval_id, run_id, task_id, action, arguments, human_required,
                        policy_reason, created_at, decided_at, decision, decided_by, reason
                 FROM approvals WHERE run_id = ?1 ORDER BY created_at",
            )?;
            stmt.query_map([run_id.to_string()], row_to_approval_json)?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            let mut stmt = conn.prepare(
                "SELECT approval_id, run_id, task_id, action, arguments, human_required,
                        policy_reason, created_at, decided_at, decision, decided_by, reason
                 FROM approvals WHERE decision IS NULL ORDER BY created_at",
            )?;
            stmt.query_map([], row_to_approval_json)?
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(json!({ "approvals": rows }))
    })
}

fn row_to_approval_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "approvalId": row.get::<_, String>(0)?,
        "runId": row.get::<_, String>(1)?,
        "taskId": row.get::<_, String>(2)?,
        "action": row.get::<_, String>(3)?,
        "arguments": serde_json::from_str::<Value>(&row.get::<_, String>(4)?).unwrap_or(Value::Null),
        "humanRequired": row.get::<_, i64>(5)? != 0,
        "policyReason": row.get::<_, String>(6)?,
        "createdAt": row.get::<_, String>(7)?,
        "decidedAt": row.get::<_, Option<String>>(8)?,
        "decision": row.get::<_, Option<String>>(9)?,
        "decidedBy": row.get::<_, Option<String>>(10)?,
        "reason": row.get::<_, Option<String>>(11)?,
    }))
}

/// Lists every run's turn budget for `project_id` (WP19 rows, snapshotted
/// at `run/submit`). Project-scoped through the run's task like
/// [`run_list_op`] -- the `budgets` table itself carries no project column.
pub fn budget_list_op(project_id: ProjectId) -> DomainClosure {
    Box::new(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT b.run_id, b.task_id, b.turns_used, b.turn_limit
             FROM budgets b
             JOIN runs r ON b.run_id = r.run_id
             JOIN tasks t ON r.task_id = t.task_id
             WHERE t.project_id = ?1 ORDER BY r.created_at",
        )?;
        let rows = stmt
            .query_map([project_id.to_string()], |row| {
                Ok(json!({
                    "runId": row.get::<_, String>(0)?,
                    "taskId": row.get::<_, String>(1)?,
                    "turnsUsed": row.get::<_, i64>(2)?,
                    "turnLimit": row.get::<_, i64>(3)?,
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({ "budgets": rows }))
    })
}

/// Lists every open (undecided) escalation for `project_id`, oldest first
/// -- what the dashboard's Escalations section renders as pending. A
/// resolved escalation has a non-null `decided_at` and is excluded.
pub fn pending_escalation_list_op(project_id: ProjectId) -> DomainClosure {
    Box::new(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT e.escalation_id, e.run_id, e.kind, e.question, e.created_at
             FROM escalations e
             JOIN runs r ON e.run_id = r.run_id
             JOIN tasks t ON r.task_id = t.task_id
             WHERE t.project_id = ?1 AND e.decided_at IS NULL ORDER BY e.created_at",
        )?;
        let rows = stmt
            .query_map([project_id.to_string()], |row| {
                Ok(json!({
                    "escalationId": row.get::<_, String>(0)?,
                    "runId": row.get::<_, String>(1)?,
                    "kind": row.get::<_, String>(2)?,
                    "question": row.get::<_, Option<String>>(3)?,
                    "createdAt": row.get::<_, String>(4)?,
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({ "pendingEscalations": rows }))
    })
}

/// Suppresses unused-import warnings for ids referenced only in signatures
/// across the various `Option<T>` positions.
#[allow(unused_imports)]
use ApprovalId as _ApprovalId;
#[allow(unused_imports)]
use MessageId as _MessageId;
pub fn owned_run_ids_op(
    owner_instance_id: String,
    task_id: Option<TaskId>,
    project_id: ProjectId,
) -> DomainClosure {
    Box::new(move |conn| {
        let sql = if task_id.is_some() {
            "SELECT r.run_id FROM runs r JOIN tasks t ON r.task_id = t.task_id WHERE t.project_id = ?1 AND t.owner_client_instance_id = ?2 AND t.task_id = ?3"
        } else {
            "SELECT r.run_id FROM runs r JOIN tasks t ON r.task_id = t.task_id WHERE t.project_id = ?1 AND t.owner_client_instance_id = ?2"
        };
        let mut stmt = conn.prepare(sql)?;
        let ids: Vec<String> = if let Some(task_id) = task_id {
            stmt.query_map(
                rusqlite::params![
                    project_id.to_string(),
                    owner_instance_id,
                    task_id.to_string()
                ],
                |row| row.get(0),
            )?
            .filter_map(|r| r.ok())
            .collect()
        } else {
            stmt.query_map(
                rusqlite::params![project_id.to_string(), owner_instance_id],
                |row| row.get(0),
            )?
            .filter_map(|r| r.ok())
            .collect()
        };
        Ok(json!(ids))
    })
}

/// Confirms `expected_instance_id` currently owns the task that owns
/// `run_id`, for `workspace/acquire`'s ownership arbitration (R77).
///
/// A workspace lease lives in [`crate::workspace::LeaseService`]'s own
/// database file, not the runs database this query reads, so this
/// round trip cannot commit atomically with the lease acquisition it
/// guards the way an owner check inside a runs-DB write can (see
/// `crate::domain::DomainRepository::submit_run`'s doc comment for that
/// pattern). `OrchestrationService::workspace_acquire` calls this as
/// close to `LeaseService::acquire` as possible -- immediately before it,
/// with no other I/O between the two -- to bound, not eliminate, the
/// window: a `reconcile/omp` rebind that commits inside that single gap
/// is not observed, and a lease already allocated when this call runs
/// again for a second attempt is unaffected either way. Making this
/// atomic with the lease acquisition it guards would require the two
/// database files to share a transaction, which they do not.
///
/// # Errors
/// Returns [`DomainError::NotFound`] if `run_id` or its task does not
/// exist, or [`DomainError::NotOwner`] if `expected_instance_id` does not
/// own the run's task.
pub fn run_owner_op(run_id: RunId, expected_instance_id: String) -> DomainClosure {
    Box::new(move |conn| {
        let task_id: String = conn
            .query_row(
                "SELECT task_id FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(DomainError::Sqlite)?
            .ok_or(DomainError::NotFound {
                kind: "run",
                id: run_id.to_string(),
            })?;
        let owner: Option<String> = conn
            .query_row(
                "SELECT owner_client_instance_id FROM tasks WHERE task_id = ?1",
                [task_id.clone()],
                |row| row.get(0),
            )
            .optional()
            .map_err(DomainError::Sqlite)?;
        match owner {
            Some(owner) if owner == expected_instance_id => Ok(json!({})),
            Some(_) => Err(DomainError::NotOwner {
                task_id,
                instance_id: expected_instance_id,
            }),
            None => Err(DomainError::NotFound {
                kind: "task",
                id: task_id,
            }),
        }
    })
}

/// [`run_owner_op`] plus a policy-quarantine check, in one closure: lease
/// owner and quarantine flag come from a single consistent snapshot, so
/// there is no check-to-check window between the ownership gate and the
/// quarantine gate (R78). The quarantine read deliberately follows the
/// owner check, so a non-owner cannot probe quarantine state.
///
/// # Errors
/// Everything [`run_owner_op`] returns, plus
/// [`DomainError::PolicyQuarantined`] when the flag is set.
pub fn run_owner_not_quarantined_op(run_id: RunId, expected_instance_id: String) -> DomainClosure {
    Box::new(move |conn| {
        let (task_id, quarantined): (String, i64) = conn
            .query_row(
                "SELECT task_id, flags_policy_quarantined FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(DomainError::Sqlite)?
            .ok_or(DomainError::NotFound {
                kind: "run",
                id: run_id.to_string(),
            })?;
        let owner: Option<String> = conn
            .query_row(
                "SELECT owner_client_instance_id FROM tasks WHERE task_id = ?1",
                [task_id.clone()],
                |row| row.get(0),
            )
            .optional()
            .map_err(DomainError::Sqlite)?;
        match owner {
            Some(owner) if owner == expected_instance_id => {}
            Some(_) => {
                return Err(DomainError::NotOwner {
                    task_id,
                    instance_id: expected_instance_id,
                });
            }
            None => {
                return Err(DomainError::NotFound {
                    kind: "task",
                    id: task_id,
                });
            }
        }
        if quarantined != 0 {
            return Err(DomainError::PolicyQuarantined {
                run_id: run_id.to_string(),
            });
        }
        Ok(json!({}))
    })
}

/// Every journaled event for one run, oldest first -- the per-run
/// transcript the dashboard serves at `/api/run/<id>/events`.
///
/// Reads the **journal**, never a vendor's own transcript file on disk:
/// journaled content has crossed the `Classified` redaction boundary and
/// had secrets stripped (ADR-0006), while the vendor's file has not.
/// Serving the file would look like a shortcut and would route around that
/// boundary.
///
/// Uses `idx_events_run_seq` (`events(run_id, sequence)`), so this is an
/// index scan over one run rather than a walk of the whole journal.
/// `limit` bounds the response: a long-running worker's transcript is
/// unbounded, and an HTTP response should not be.
pub fn run_events_op(run_id: RunId, limit: u32) -> DomainClosure {
    Box::new(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT sequence, timestamp, event_json FROM events
              WHERE run_id = ?1
              ORDER BY sequence
              LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![run_id.to_string(), limit], |row| {
                let sequence: i64 = row.get(0)?;
                let timestamp: String = row.get(1)?;
                let event_json: String = row.get(2)?;
                Ok(json!({
                    "sequence": sequence,
                    "timestamp": timestamp,
                    // Parsed so the response is real JSON rather than a
                    // string containing JSON; an unparseable row is
                    // surfaced as null rather than failing the whole read.
                    "event": serde_json::from_str::<Value>(&event_json).ok(),
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({ "runId": run_id.to_string(), "events": rows }))
    })
}

/// Reads a terminal run's journal residue for `run/result`: the final
/// visible message text and the folded usage totals. The usage fold is
/// adapter-dependent -- Claude journals per-invocation deltas (sum);
/// every other reporting adapter journals cumulative totals (last wins).
/// Returns `{"resultText": ..., "usage": ...}`; the caller merges the
/// run-row fields it already holds.
pub fn run_result_events_op(run_id: RunId) -> DomainClosure {
    Box::new(move |conn| {
        let adapter: Option<String> = conn
            .query_row(
                "SELECT p.adapter
                   FROM runs r
                   JOIN workers w ON r.worker_id = w.worker_id
                   JOIN worker_profiles p ON w.profile_id = p.id
                  WHERE r.run_id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(DomainError::Sqlite)?;
        let sum_deltas = adapter.as_deref() == Some("claude");

        let mut stmt = conn
            .prepare(
                "SELECT event_json FROM events
                  WHERE run_id = ?1
                    AND (event_json LIKE '%adapterMessageEvent%'
                         OR event_json LIKE '%adapterUsageEvent%'
                         OR event_json LIKE '%adapterTurnEvent%')
                  ORDER BY sequence",
            )
            .map_err(DomainError::Sqlite)?;
        let rows = stmt
            .query_map([run_id.to_string()], |row| row.get::<_, String>(0))
            .map_err(DomainError::Sqlite)?;

        let mut final_text: Option<String> = None;
        let mut chunk_text: Option<String> = None;
        let mut usage: Option<(u64, u64, Option<f64>)> = None;
        // ADR-0027's fold boundary: the residue is read up to and
        // including the FIRST turn boundary. The vendor process stays
        // alive after its turn, so a later turn would otherwise keep
        // appending to this same run and silently rewrite an answer the
        // leader has already read. A leader wanting a later turn asks for
        // it explicitly.
        let mut turn_ended = false;

        for row in rows {
            let raw = row.map_err(DomainError::Sqlite)?;
            let Ok(event) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            // Re-verify the type tag: the LIKE prefilter alone would
            // false-positive on message text that mentions these strings
            // (same two-step as policy/evaluate.rs:154-196).
            match event.get("type").and_then(Value::as_str) {
                Some("adapterMessageEvent") => {
                    let payload = &event["payload"];
                    let Some(text) = payload.get("text").and_then(Value::as_str) else {
                        continue; // fully-redacted fragment, journaled as null
                    };
                    match payload.get("kind").and_then(Value::as_str) {
                        Some("adapterMessageFinal") => final_text = Some(text.to_string()),
                        Some("adapterMessageChunk") => chunk_text = Some(text.to_string()),
                        _ => {}
                    }
                }
                Some("adapterUsageEvent") => {
                    let payload = &event["payload"];
                    let input = payload
                        .get("inputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let output = payload
                        .get("outputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let cost = payload.get("costUsd").and_then(Value::as_f64);
                    usage = Some(match (sum_deltas, usage) {
                        (true, Some((i, o, c))) => (
                            i + input,
                            o + output,
                            match (c, cost) {
                                (Some(a), Some(b)) => Some(a + b),
                                (a, b) => a.or(b),
                            },
                        ),
                        _ => (input, output, cost),
                    });
                }
                Some("adapterTurnEvent") => {
                    turn_ended = true;
                    break;
                }
                _ => {}
            }
        }

        Ok(json!({
            "turnEnded": turn_ended,
            "resultText": final_text.or(chunk_text),
            "usage": usage.map(|(input, output, cost)| json!({
                "inputTokens": input,
                "outputTokens": output,
                "costUsd": cost,
            })),
        }))
    })
}
