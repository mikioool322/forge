//! Typed IPC messages.

use forge_core::model::{
    AgentInfo, AgentSession, EventRecord, PlanFile, Project, Task, TaskArtifact, TaskId, Worktree,
};
use serde::{Deserialize, Serialize};

/// Bumped whenever the wire format changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// First frame on every connection.
    Hello {
        protocol: u32,
        client: String,
        /// Required for TCP endpoints, ignored on Unix sockets.
        #[serde(default)]
        token: Option<String>,
    },
    Ping,
    /// Full state snapshot (tasks, agents, projects, recent events).
    GetState,
    /// Ask the daemon to push notifications on this connection.
    Subscribe {
        #[serde(default)]
        since_event_id: Option<i64>,
    },
    /// Submit a deterministic task plan.
    SubmitPlan {
        request_id: String,
        plan: PlanFile,
    },
    CancelTask {
        #[serde(default)]
        project: Option<String>,
        task: String,
        #[serde(default)]
        reason: Option<String>,
    },
    RetryTask {
        #[serde(default)]
        project: Option<String>,
        task: String,
    },
    GetTaskDetail {
        #[serde(default)]
        project: Option<String>,
        task: String,
    },
    GetTaskLog {
        #[serde(default)]
        project: Option<String>,
        task: String,
        #[serde(default)]
        lines: Option<usize>,
    },
    GetTaskDiff {
        #[serde(default)]
        project: Option<String>,
        task: String,
        #[serde(default)]
        max_bytes: Option<usize>,
    },
    ListProjects,
    RegisterProject {
        root: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// Graceful daemon shutdown.
    Shutdown {
        #[serde(default)]
        idempotency_key: Option<String>,
        /// Let running tasks finish before shutting down.
        #[serde(default)]
        drain: Option<bool>,
    },
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    NotFound,
    Conflict,
    Unauthorized,
    Unavailable,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Hello {
        protocol: u32,
        daemon_version: String,
        instance_id: String,
        uptime_ms: u64,
    },
    Pong {
        protocol: u32,
        daemon_version: String,
        uptime_ms: u64,
    },
    State(Box<StateSnapshot>),
    Subscribed,
    PlanAccepted {
        plan_id: i64,
        created: bool,
        tasks: Vec<TaskView>,
    },
    Task(Box<TaskView>),
    TaskDetail(Box<TaskDetail>),
    TaskLog {
        text: String,
        truncated: bool,
        path: Option<String>,
    },
    TaskDiff {
        text: String,
        truncated: bool,
    },
    Projects {
        projects: Vec<Project>,
    },
    Events {
        events: Vec<EventRecord>,
    },
    Project {
        project: Project,
        created: bool,
    },
    Ack {
        #[serde(default)]
        note: Option<String>,
    },
    ShuttingDown,
    Error {
        code: ErrorCode,
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Notifications (server push)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "notification", rename_all = "snake_case")]
pub enum Notification {
    TaskChanged(Box<TaskView>),
    /// Struct variant on purpose: serde cannot internally tag a newtype
    /// variant containing a sequence.
    AgentsChanged {
        agents: Vec<AgentInfo>,
    },
    Event(Box<EventRecord>),
    DaemonNote {
        message: String,
    },
    /// Notifications were dropped (slow client): refetch full state.
    Resync,
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum Frame {
    Request { id: u64, request: Request },
    Response { id: u64, response: Response },
    Notify { notification: Notification },
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskView {
    pub task: Task,
    pub deps: Vec<TaskId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDetail {
    pub task: Task,
    pub deps: Vec<TaskId>,
    pub worktree: Option<Worktree>,
    pub artifacts: Vec<TaskArtifact>,
    pub sessions: Vec<AgentSession>,
    pub events: Vec<EventRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub protocol: u32,
    pub instance_id: String,
    pub started_at: i64,
    pub uptime_ms: u64,
    pub endpoint: String,
    pub max_concurrency: u32,
    pub running_tasks: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub daemon: DaemonInfo,
    pub projects: Vec<Project>,
    pub tasks: Vec<TaskView>,
    pub agents: Vec<AgentInfo>,
    /// Recent durable events (tail).
    pub events: Vec<EventRecord>,
}

impl Response {
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Response::Error {
            code,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_request() {
        let frame = Frame::Request {
            id: 7,
            request: Request::Ping,
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: Frame = serde_json::from_str(&json).unwrap();
        match back {
            Frame::Request {
                id,
                request: Request::Ping,
            } => assert_eq!(id, 7),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn frame_roundtrip_notify() {
        let frame = Frame::Notify {
            notification: Notification::DaemonNote {
                message: "hello".into(),
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: Frame = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            Frame::Notify {
                notification: Notification::DaemonNote { .. }
            }
        ));
    }

    #[test]
    fn every_response_variant_serializes() {
        // Internally tagged enums reject newtype variants containing
        // sequences; this test constructs every variant to catch that class
        // of bug at test time instead of on a live connection.
        let responses = vec![
            Response::Hello {
                protocol: PROTOCOL_VERSION,
                daemon_version: "x".into(),
                instance_id: "i".into(),
                uptime_ms: 1,
            },
            Response::Pong {
                protocol: PROTOCOL_VERSION,
                daemon_version: "x".into(),
                uptime_ms: 1,
            },
            Response::State(Box::new(StateSnapshot {
                daemon: DaemonInfo {
                    version: "x".into(),
                    protocol: PROTOCOL_VERSION,
                    instance_id: "i".into(),
                    started_at: 0,
                    uptime_ms: 0,
                    endpoint: "tcp:127.0.0.1:1".into(),
                    max_concurrency: 1,
                    running_tasks: 0,
                },
                projects: vec![],
                tasks: vec![],
                agents: vec![],
                events: vec![],
            })),
            Response::Subscribed,
            Response::PlanAccepted {
                plan_id: 1,
                created: true,
                tasks: vec![],
            },
            Response::Task(Box::new(TaskView {
                task: dummy_task(),
                deps: vec![],
            })),
            Response::TaskDetail(Box::new(TaskDetail {
                task: dummy_task(),
                deps: vec![],
                worktree: None,
                artifacts: vec![],
                sessions: vec![],
                events: vec![],
            })),
            Response::TaskLog {
                text: "log".into(),
                truncated: false,
                path: None,
            },
            Response::TaskDiff {
                text: "diff".into(),
                truncated: false,
            },
            Response::Projects { projects: vec![] },
            Response::Events { events: vec![] },
            Response::Project {
                project: Project {
                    id: 1,
                    name: "demo".into(),
                    root: "/tmp/demo".into(),
                    default_branch: None,
                    created_at: 0,
                },
                created: true,
            },
            Response::Ack { note: None },
            Response::ShuttingDown,
            Response::error(ErrorCode::Internal, "boom"),
        ];
        for response in responses {
            let json = serde_json::to_string(&response)
                .unwrap_or_else(|e| panic!("cannot serialize {response:?}: {e}"));
            let back: Response = serde_json::from_str(&json).unwrap();
            // Round-trip through the full frame as well.
            let frame = Frame::Response {
                id: 1,
                response: back,
            };
            let json = serde_json::to_string(&frame)
                .unwrap_or_else(|e| panic!("cannot serialize frame {frame:?}: {e}"));
            let _: Frame = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn every_notification_variant_serializes() {
        let notifications = vec![
            Notification::TaskChanged(Box::new(TaskView {
                task: dummy_task(),
                deps: vec![],
            })),
            Notification::AgentsChanged { agents: vec![] },
            Notification::Event(Box::new(EventRecord {
                id: 1,
                ts: 0,
                project_id: None,
                task_id: None,
                agent_id: None,
                kind: forge_core::EventKind::DaemonNote,
                severity: forge_core::Severity::Info,
                message: "x".into(),
                data: None,
            })),
            Notification::DaemonNote {
                message: "x".into(),
            },
            Notification::Resync,
        ];
        for notification in notifications {
            let frame = Frame::Notify { notification };
            let json = serde_json::to_string(&frame)
                .unwrap_or_else(|e| panic!("cannot serialize {frame:?}: {e}"));
            let _: Frame = serde_json::from_str(&json).unwrap();
        }
    }

    fn dummy_task() -> Task {
        Task {
            id: 1,
            project_id: 1,
            seq_no: 1,
            title: "t".into(),
            spec: String::new(),
            state: forge_core::TaskState::Created,
            priority: 0,
            required_caps: vec![],
            claims: vec![],
            preferred_agent: None,
            agent_id: None,
            worktree_id: None,
            base_commit: None,
            timeout_secs: None,
            attempts: 0,
            created_at: 0,
            updated_at: 0,
            started_at: None,
            finished_at: None,
            exit_code: None,
            failure_reason: None,
            log_path: None,
            summary: None,
            plan_id: None,
            plan_key: None,
        }
    }

    #[test]
    fn error_response_roundtrip() {
        let frame = Frame::Response {
            id: 1,
            response: Response::error(ErrorCode::NotFound, "missing"),
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: Frame = serde_json::from_str(&json).unwrap();
        match back {
            Frame::Response {
                response: Response::Error { code, message },
                ..
            } => {
                assert_eq!(code, ErrorCode::NotFound);
                assert_eq!(message, "missing");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
