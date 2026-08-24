//! Run lifecycle transition enforcement.
//!
//! The runtime is the sole authority for run-state transitions. Every edge
//! is validated against the canonical lifecycle relation in
//! [`crew_protocol::RunState`] before an event is appended; an illegal
//! edge produces [`TransitionError::Illegal`] and appends nothing.

use crew_protocol::RunState;

/// An error raised when a requested run-state transition is not allowed by
/// the canonical lifecycle relation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    /// The `from -> to` edge is not a legal lifecycle transition.
    #[error("illegal run transition for {run_id}: {from} -> {to}")]
    Illegal {
        run_id: String,
        from: String,
        to: String,
    },
}

/// Validates a run-state transition. Returns `Ok(())` when `from -> to` is a
/// legal edge, or [`TransitionError::Illegal`] otherwise.
///
/// # Errors
/// Returns [`TransitionError::Illegal`] if the edge is not permitted by the
/// canonical lifecycle relation (including self-transitions and any edge out
/// of a terminal state).
pub fn check_transition(
    run_id: &str,
    from: &RunState,
    to: &RunState,
) -> Result<(), TransitionError> {
    if from.can_transition_to(to) {
        Ok(())
    } else {
        Err(TransitionError::Illegal {
            run_id: run_id.to_string(),
            from: from.to_string(),
            to: to.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_edge_is_accepted() {
        let from = RunState::try_from("queued").unwrap();
        let to = RunState::try_from("starting").unwrap();
        assert!(check_transition("r1", &from, &to).is_ok());
    }

    #[test]
    fn illegal_edge_is_rejected_with_detail() {
        let from = RunState::try_from("working").unwrap();
        let to = RunState::try_from("queued").unwrap();
        let err = check_transition("r1", &from, &to).unwrap_err();
        assert_eq!(
            err,
            TransitionError::Illegal {
                run_id: "r1".to_string(),
                from: "working".to_string(),
                to: "queued".to_string(),
            }
        );
    }

    #[test]
    fn terminal_state_rejects_all_edges() {
        let from = RunState::try_from("succeeded").unwrap();
        for target in ["working", "failed", "cancelled", "lost"] {
            let to = RunState::try_from(target).unwrap();
            assert!(check_transition("r1", &from, &to).is_err());
        }
    }
}
