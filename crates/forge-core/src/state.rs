//! The durable task state machine.
//!
//! Legal transitions are defined explicitly and exhaustively; every state
//! change in the system must go through [`is_legal_transition`]. The store
//! layer enforces transitions atomically with
//! `UPDATE tasks SET state=:to WHERE id=:id AND state=:from`.

use crate::model::TaskState;

/// `(from, to)` pairs that are allowed.
pub const LEGAL_TRANSITIONS: &[(TaskState, TaskState)] = {
    use TaskState::*;
    &[
        // Creation and dependency gating.
        (Created, Ready),
        (Created, Blocked),
        (Created, Cancelled),
        // Dispatch.
        (Ready, Leased),
        (Ready, Blocked),
        (Ready, Cancelled),
        // Lease lifecycle (build worktree, acquire resources).
        (Leased, Running),
        (Leased, Ready),
        (Leased, Failed),
        (Leased, Cancelled),
        (Leased, Interrupted),
        // Execution.
        (Running, ReviewPending),
        (Running, Failed),
        (Running, Cancelled),
        (Running, Interrupted),
        // Contract verification (sprint 1: artifact capture only).
        (ReviewPending, Completed),
        (ReviewPending, Failed),
        (ReviewPending, Cancelled),
        (ReviewPending, Interrupted),
        // Unblocking and retries.
        (Blocked, Ready),
        (Blocked, Cancelled),
        (Failed, Ready),
        (Failed, Cancelled),
        (Interrupted, Ready),
        (Interrupted, Cancelled),
    ]
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("illegal task state transition {from} -> {to}")]
pub struct TransitionError {
    pub from: TaskState,
    pub to: TaskState,
}

/// Returns true if the transition `from -> to` is legal.
pub fn is_legal_transition(from: TaskState, to: TaskState) -> bool {
    LEGAL_TRANSITIONS.contains(&(from, to))
}

/// Returns `Ok(())` when the transition is legal, otherwise a
/// [`TransitionError`].
pub fn check_transition(from: TaskState, to: TaskState) -> Result<(), TransitionError> {
    if is_legal_transition(from, to) {
        Ok(())
    } else {
        Err(TransitionError { from, to })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskState::*;

    #[test]
    fn legal_chain() {
        let chain = [
            (Created, Ready),
            (Ready, Leased),
            (Leased, Running),
            (Running, ReviewPending),
            (ReviewPending, Completed),
        ];
        for (from, to) in chain {
            assert!(is_legal_transition(from, to), "{from} -> {to}");
        }
    }

    #[test]
    fn illegal_transitions_rejected() {
        let bad = [
            (Completed, Running),
            (Cancelled, Ready),
            (Created, Running),
            (Running, Ready),
            (Ready, Completed),
            (Blocked, Running),
            (Leased, Completed),
        ];
        for (from, to) in bad {
            assert!(
                !is_legal_transition(from, to),
                "{from} -> {to} must be illegal"
            );
            assert!(check_transition(from, to).is_err());
        }
    }

    #[test]
    fn retry_paths_exist() {
        assert!(is_legal_transition(Failed, Ready));
        assert!(is_legal_transition(Interrupted, Ready));
        assert!(is_legal_transition(Blocked, Ready));
    }

    #[test]
    fn terminal_states_have_no_outgoing_running_paths() {
        for to in TaskState::ALL {
            assert!(!is_legal_transition(Completed, to));
            assert!(!is_legal_transition(Cancelled, to));
        }
    }

    #[test]
    fn transition_error_message_is_stable() {
        let e = check_transition(Completed, Running).unwrap_err();
        assert_eq!(
            e.to_string(),
            "illegal task state transition COMPLETED -> RUNNING"
        );
    }
}
