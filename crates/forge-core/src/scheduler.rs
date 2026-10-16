//! Scheduler decision function.
//!
//! [`schedule`] is a pure function: given the current task/agent state it
//! returns the actions the daemon engine should apply. It never performs
//! I/O itself, which makes every scheduling rule directly unit-testable.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{claims_conflict, TaskId, TaskState};

/// Minimal task view needed for scheduling decisions.
#[derive(Debug, Clone)]
pub struct SchedulerTask {
    pub id: TaskId,
    pub seq_no: i64,
    pub priority: i64,
    pub state: TaskState,
    pub required_caps: Vec<String>,
    pub preferred_agent: Option<String>,
    pub claims: Vec<String>,
    /// State of every dependency: `(dep_task_id, dep_state)`.
    pub dep_states: Vec<(TaskId, TaskState)>,
}

/// Agent view needed for scheduling decisions.
#[derive(Debug, Clone)]
pub struct AgentScheduleView {
    pub id: String,
    pub capabilities: Vec<String>,
    pub slots: u32,
    pub running: u32,
    /// False when the agent is disabled or its probe failed.
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduledAction {
    /// `CREATED -> READY` (all dependencies completed).
    Promote { task: TaskId },
    /// `CREATED|READY -> BLOCKED` (a dependency failed or was cancelled).
    Block { task: TaskId, reason: String },
    /// `READY -> LEASED` with an agent assignment.
    Dispatch { task: TaskId, agent: String },
}

pub struct SchedulerInput {
    pub tasks: Vec<SchedulerTask>,
    pub agents: Vec<AgentScheduleView>,
    /// Number of tasks currently in LEASED/RUNNING state.
    pub running_count: usize,
    /// Resource claims held by running tasks: `(task_id, claim)`.
    pub running_claims: Vec<(TaskId, String)>,
    pub max_concurrency: usize,
}

/// Compute the next scheduling actions. Deterministic for equal inputs.
pub fn schedule(input: &SchedulerInput) -> Vec<ScheduledAction> {
    let mut actions: Vec<ScheduledAction> = Vec::new();

    // Stable priority: higher priority first, then lower sequence number.
    let mut order: Vec<&SchedulerTask> = input.tasks.iter().collect();
    order.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(a.seq_no.cmp(&b.seq_no))
            .then(a.id.cmp(&b.id))
    });

    // --- 1. dependency gating ---------------------------------------------
    let mut promoted: BTreeSet<TaskId> = BTreeSet::new();
    for t in &order {
        let dep_failed = t
            .dep_states
            .iter()
            .any(|(_, s)| matches!(s, TaskState::Failed | TaskState::Cancelled));
        match t.state {
            TaskState::Created => {
                if dep_failed {
                    actions.push(ScheduledAction::Block {
                        task: t.id,
                        reason: "dependency failed or cancelled".into(),
                    });
                } else if t.dep_states.iter().all(|(_, s)| *s == TaskState::Completed) {
                    actions.push(ScheduledAction::Promote { task: t.id });
                    promoted.insert(t.id);
                }
            }
            TaskState::Ready if dep_failed => {
                actions.push(ScheduledAction::Block {
                    task: t.id,
                    reason: "dependency failed or cancelled".into(),
                });
            }
            _ => {}
        }
    }

    // --- 2. dispatch --------------------------------------------------------
    let mut free_slots: BTreeMap<String, i64> = BTreeMap::new();
    for a in &input.agents {
        if a.available {
            free_slots.insert(a.id.clone(), i64::from(a.slots) - i64::from(a.running));
        }
    }

    let mut announced_claims: Vec<(TaskId, String)> = input.running_claims.clone();
    let mut capacity_left = input.max_concurrency.saturating_sub(input.running_count);

    for t in &order {
        let ready_now = matches!(t.state, TaskState::Ready) || promoted.contains(&t.id);
        if !ready_now || capacity_left == 0 {
            continue;
        }
        // A promoted task is dispatched in the same pass; a task that was
        // already READY must still have all dependencies completed.
        if t.dep_states.iter().any(|(_, s)| *s != TaskState::Completed) {
            continue;
        }

        let caps: BTreeSet<String> = t
            .required_caps
            .iter()
            .map(|c| c.to_ascii_lowercase())
            .collect();

        let mut candidates: Vec<(&AgentScheduleView, i64)> = input
            .agents
            .iter()
            .filter(|a| a.available)
            .filter(|a| free_slots.get(&a.id).copied().unwrap_or(0) > 0)
            .filter(|a| {
                t.preferred_agent
                    .as_deref()
                    .is_none_or(|p| p == a.id.as_str())
            })
            .filter(|a| {
                caps.iter().all(|c| {
                    a.capabilities
                        .iter()
                        .any(|ac| ac.to_ascii_lowercase() == *c)
                })
            })
            .map(|a| (a, free_slots.get(&a.id).copied().unwrap_or(0)))
            .collect();
        // Most free capacity first, then agent id for determinism.
        candidates.sort_by(|(a, fa), (b, fb)| fb.cmp(fa).then(a.id.cmp(&b.id)));

        let Some((agent, _)) = candidates.first() else {
            continue;
        };

        // Resource claims: never dispatch a task whose claims overlap claims
        // held by a *different* task (running or dispatched in this pass).
        let conflicts = t.claims.iter().any(|c| {
            announced_claims
                .iter()
                .any(|(tid, rc)| *tid != t.id && claims_conflict(c, rc))
        });
        if conflicts {
            continue;
        }

        actions.push(ScheduledAction::Dispatch {
            task: t.id,
            agent: agent.id.clone(),
        });
        for c in &t.claims {
            announced_claims.push((t.id, c.clone()));
        }
        if let Some(s) = free_slots.get_mut(&agent.id) {
            *s -= 1;
        }
        capacity_left -= 1;
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(
        id: TaskId,
        seq: i64,
        state: TaskState,
        deps: Vec<(TaskId, TaskState)>,
    ) -> SchedulerTask {
        SchedulerTask {
            id,
            seq_no: seq,
            priority: 0,
            state,
            required_caps: vec![],
            preferred_agent: None,
            claims: vec![],
            dep_states: deps,
        }
    }

    fn agent(id: &str, caps: &[&str], slots: u32, running: u32) -> AgentScheduleView {
        AgentScheduleView {
            id: id.into(),
            capabilities: caps.iter().map(|c| (*c).to_string()).collect(),
            slots,
            running,
            available: true,
        }
    }

    fn base_input(tasks: Vec<SchedulerTask>, agents: Vec<AgentScheduleView>) -> SchedulerInput {
        SchedulerInput {
            tasks,
            agents,
            running_count: 0,
            running_claims: vec![],
            max_concurrency: 4,
        }
    }

    #[test]
    fn promotes_created_task_when_deps_complete() {
        let input = base_input(
            vec![task(
                1,
                1,
                TaskState::Created,
                vec![(2, TaskState::Completed)],
            )],
            vec![agent("a", &[], 1, 0)],
        );
        let actions = schedule(&input);
        assert!(actions.contains(&ScheduledAction::Promote { task: 1 }));
    }

    #[test]
    fn waits_while_dependency_runs() {
        let input = base_input(
            vec![
                task(1, 1, TaskState::Running, vec![]),
                task(2, 2, TaskState::Created, vec![(1, TaskState::Running)]),
            ],
            vec![agent("a", &[], 2, 1)],
        );
        let actions = schedule(&input);
        assert_eq!(
            actions,
            vec![],
            "dependent must stay CREATED while dep is RUNNING"
        );
    }

    #[test]
    fn blocks_when_dependency_failed() {
        let input = base_input(
            vec![task(1, 1, TaskState::Created, vec![(2, TaskState::Failed)])],
            vec![agent("a", &[], 1, 0)],
        );
        let actions = schedule(&input);
        assert!(matches!(actions[0], ScheduledAction::Block { task: 1, .. }));
    }

    #[test]
    fn dispatches_ready_tasks_in_sequence_order() {
        let input = base_input(
            vec![
                task(2, 2, TaskState::Ready, vec![]),
                task(1, 1, TaskState::Ready, vec![]),
            ],
            vec![agent("a", &[], 2, 0)],
        );
        let actions = schedule(&input);
        let dispatched: Vec<TaskId> = actions
            .iter()
            .filter_map(|a| match a {
                ScheduledAction::Dispatch { task, .. } => Some(*task),
                _ => None,
            })
            .collect();
        assert_eq!(dispatched, vec![1, 2]);
    }

    #[test]
    fn respects_capability_matching() {
        let mut t = task(1, 1, TaskState::Ready, vec![]);
        t.required_caps = vec!["backend".into()];
        let input = base_input(
            vec![t],
            vec![
                agent("frontend-only", &["frontend"], 1, 0),
                agent("full", &["frontend", "backend"], 1, 0),
            ],
        );
        let actions = schedule(&input);
        assert_eq!(
            actions,
            vec![ScheduledAction::Dispatch {
                task: 1,
                agent: "full".into()
            }]
        );
    }

    #[test]
    fn respects_preferred_agent() {
        let mut t = task(1, 1, TaskState::Ready, vec![]);
        t.preferred_agent = Some("b".into());
        let input = base_input(vec![t], vec![agent("a", &[], 1, 0), agent("b", &[], 1, 0)]);
        let actions = schedule(&input);
        assert_eq!(
            actions,
            vec![ScheduledAction::Dispatch {
                task: 1,
                agent: "b".into()
            }]
        );
    }

    #[test]
    fn preferred_agent_unavailable_means_queued() {
        let mut t = task(1, 1, TaskState::Ready, vec![]);
        t.preferred_agent = Some("b".into());
        let input = base_input(vec![t], vec![agent("a", &[], 1, 0)]);
        assert_eq!(schedule(&input), vec![]);
    }

    #[test]
    fn global_concurrency_limit_enforced() {
        let input = SchedulerInput {
            tasks: vec![
                task(1, 1, TaskState::Ready, vec![]),
                task(2, 2, TaskState::Ready, vec![]),
                task(3, 3, TaskState::Ready, vec![]),
            ],
            agents: vec![agent("a", &[], 8, 0)],
            running_count: 1,
            running_claims: vec![],
            max_concurrency: 2,
        };
        let actions = schedule(&input);
        let dispatched = actions
            .iter()
            .filter(|a| matches!(a, ScheduledAction::Dispatch { .. }))
            .count();
        assert_eq!(dispatched, 1, "only one free slot below max_concurrency=2");
    }

    #[test]
    fn agent_slot_limit_enforced() {
        let input = base_input(
            vec![
                task(1, 1, TaskState::Ready, vec![]),
                task(2, 2, TaskState::Ready, vec![]),
            ],
            vec![agent("a", &[], 1, 0)],
        );
        let actions = schedule(&input);
        let dispatched: Vec<TaskId> = actions
            .iter()
            .filter_map(|a| match a {
                ScheduledAction::Dispatch { task, .. } => Some(*task),
                _ => None,
            })
            .collect();
        assert_eq!(dispatched, vec![1]);
    }

    #[test]
    fn conflicting_claims_are_serialized() {
        let mut t1 = task(1, 1, TaskState::Ready, vec![]);
        t1.claims = vec!["file:src/lib.rs".into()];
        let mut t2 = task(2, 2, TaskState::Ready, vec![]);
        t2.claims = vec!["file:src/lib.rs".into()];
        let input = base_input(vec![t1, t2], vec![agent("a", &[], 4, 0)]);
        let actions = schedule(&input);
        let dispatched: Vec<TaskId> = actions
            .iter()
            .filter_map(|a| match a {
                ScheduledAction::Dispatch { task, .. } => Some(*task),
                _ => None,
            })
            .collect();
        assert_eq!(dispatched, vec![1], "second task must wait for the claim");
    }

    #[test]
    fn conflicting_claims_with_running_task_are_serialized() {
        let mut t = task(1, 1, TaskState::Ready, vec![]);
        t.claims = vec!["dir:src".into()];
        let input = SchedulerInput {
            tasks: vec![t],
            agents: vec![agent("a", &[], 2, 0)],
            running_count: 1,
            running_claims: vec![(99, "file:src/lib.rs".into())],
            max_concurrency: 4,
        };
        assert_eq!(schedule(&input), vec![]);
    }

    #[test]
    fn unavailable_agents_are_excluded() {
        let mut a = agent("a", &[], 1, 0);
        a.available = false;
        let input = base_input(vec![task(1, 1, TaskState::Ready, vec![])], vec![a]);
        assert_eq!(schedule(&input), vec![]);
    }

    #[test]
    fn higher_priority_wins() {
        let mut t1 = task(1, 1, TaskState::Ready, vec![]);
        t1.priority = 0;
        let mut t2 = task(2, 2, TaskState::Ready, vec![]);
        t2.priority = 5;
        let input = base_input(vec![t1, t2], vec![agent("a", &[], 1, 0)]);
        let actions = schedule(&input);
        assert_eq!(
            actions,
            vec![ScheduledAction::Dispatch {
                task: 2,
                agent: "a".into()
            }]
        );
    }

    #[test]
    fn deterministic_output() {
        let mut t1 = task(1, 1, TaskState::Created, vec![]);
        t1.priority = 1;
        let mut t2 = task(2, 2, TaskState::Ready, vec![]);
        t2.priority = 1;
        t2.required_caps = vec!["x".into()];
        let input = base_input(
            vec![t2, t1],
            vec![agent("a", &["x"], 2, 0), agent("b", &["x"], 2, 0)],
        );
        let first = schedule(&input);
        let second = schedule(&input);
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }
}
