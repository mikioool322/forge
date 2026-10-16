//! Store integration tests: migrations, repositories, leases, events and
//! concurrency behaviour of the database worker.

use forge_core::model::{EventKind, SessionState, Severity, TaskState, WorktreeState};
use forge_core::AgentSpec;
use forge_store::repo::{self, NewEvent, NewTask};
use forge_store::{probe_database, DirectDb, Store, StoreError};
use rusqlite::Connection;

fn new_task(project_id: i64, title: &str, state: TaskState) -> NewTask {
    NewTask {
        project_id,
        plan_id: None,
        plan_key: None,
        title: title.into(),
        spec: format!("spec for {title}"),
        state,
        priority: 0,
        required_caps: vec!["code".into()],
        claims: vec![],
        preferred_agent: None,
        timeout_secs: Some(60),
    }
}

#[tokio::test]
async fn migrations_apply_once_and_report_version() {
    let store = Store::open_in_memory().unwrap();
    let version = store
        .call(|conn| forge_store::migrations::current_version(conn))
        .await
        .unwrap();
    assert_eq!(version, forge_store::migrations::latest_version());

    // Running migrations again is a no-op.
    store
        .call(|conn| {
            forge_store::migrations::run(conn)?;
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn file_database_uses_wal_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");

    {
        let store = Store::open(&db_path).unwrap();
        let mode = store
            .call(|conn| {
                let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
                Ok(mode)
            })
            .await
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");

        let (project, created) = store
            .call(|conn| repo::register_project(conn, "demo", "C:/tmp/demo", Some("main")))
            .await
            .unwrap();
        assert!(created);
        store
            .call(move |conn| {
                repo::create_task(conn, new_task(project.id, "first", TaskState::Created))?;
                Ok(())
            })
            .await
            .unwrap();
    } // store dropped -> worker exits, WAL checkpointed

    // Reopen and verify the data is durable.
    let store = Store::open(&db_path).unwrap();
    let tasks = store
        .call(|conn| {
            let p = repo::get_project_by_name(conn, "demo")?.expect("project");
            repo::list_tasks(conn, p.id)
        })
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "first");

    // A second reader (like `forge doctor`) can open the DB read-only.
    let probe = probe_database(&db_path);
    assert!(probe.exists);
    assert_eq!(probe.integrity, "ok");
    assert_eq!(
        probe.version,
        Some(forge_store::migrations::latest_version())
    );
}

#[tokio::test]
async fn rejects_newer_schema() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    {
        let _store = Store::open(&db_path).unwrap();
    }
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "user_version", 999).unwrap();
    }
    let err = Store::open(&db_path).unwrap_err();
    assert!(matches!(err, StoreError::SchemaNewer { .. }), "{err:?}");
}

#[tokio::test]
async fn project_registration_is_idempotent_and_guards_names() {
    let store = Store::open_in_memory().unwrap();
    let (p1, created1) = store
        .call(|conn| repo::register_project(conn, "demo", "C:/tmp/demo", None))
        .await
        .unwrap();
    let (p2, created2) = store
        .call(|conn| repo::register_project(conn, "demo", "C:/tmp/demo", None))
        .await
        .unwrap();
    assert!(created1);
    assert!(!created2);
    assert_eq!(p1.id, p2.id);

    // Same name, different root -> rejected.
    let err = store
        .call(|conn| repo::register_project(conn, "demo", "C:/tmp/other", None))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound { .. }), "{err:?}");

    let projects = store.call(|conn| repo::list_projects(conn)).await.unwrap();
    assert_eq!(projects.len(), 1);
}

#[tokio::test]
async fn task_numbers_are_sequential_and_references_resolve() {
    let store = Store::open_in_memory().unwrap();
    let (project, _) = store
        .call(|conn| repo::register_project(conn, "demo", "C:/tmp/demo", None))
        .await
        .unwrap();
    let pid = project.id;
    let tasks = store
        .call(move |conn| {
            let a = repo::create_task(conn, new_task(pid, "a", TaskState::Created))?;
            let b = repo::create_task(conn, new_task(pid, "b", TaskState::Created))?;
            let c = repo::create_task(conn, new_task(pid, "c", TaskState::Created))?;
            repo::add_dependency(conn, c.id, a.id)?;
            repo::add_dependency(conn, c.id, b.id)?;
            Ok((a, b, c))
        })
        .await
        .unwrap();
    assert_eq!(tasks.0.seq_no, 1);
    assert_eq!(tasks.1.seq_no, 2);
    assert_eq!(tasks.2.seq_no, 3);
    assert_eq!(tasks.0.display_id(), "T-001");

    let deps = store
        .call(move |conn| repo::deps_of(conn, tasks.2.id))
        .await
        .unwrap();
    assert_eq!(deps, vec![tasks.0.id, tasks.1.id]);

    let found = store
        .call(move |conn| repo::find_task(conn, pid, "T-003"))
        .await
        .unwrap()
        .expect("resolvable reference");
    assert_eq!(found.title, "c");
    let found = store
        .call(move |conn| repo::find_task(conn, pid, "1"))
        .await
        .unwrap()
        .expect("resolvable numeric reference");
    assert_eq!(found.title, "a");
    let missing = store
        .call(move |conn| repo::find_task(conn, pid, "T-999"))
        .await
        .unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn state_transitions_are_atomic_and_legal_only() {
    let store = Store::open_in_memory().unwrap();
    let (project, _) = store
        .call(|conn| repo::register_project(conn, "demo", "C:/tmp/demo", None))
        .await
        .unwrap();
    let pid = project.id;
    let task = store
        .call(move |conn| repo::create_task(conn, new_task(pid, "t", TaskState::Created)))
        .await
        .unwrap();
    let id = task.id;

    store
        .call(move |conn| repo::set_task_state(conn, id, TaskState::Created, TaskState::Ready))
        .await
        .unwrap();
    store
        .call(move |conn| repo::set_task_state(conn, id, TaskState::Ready, TaskState::Leased))
        .await
        .unwrap();

    // Wrong "from" state -> conflict, not a silent corruption.
    let err = store
        .call(move |conn| repo::set_task_state(conn, id, TaskState::Ready, TaskState::Leased))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::StateConflict { .. }), "{err:?}");

    // Illegal transition -> rejected before touching the database.
    let err = store
        .call(move |conn| repo::set_task_state(conn, id, TaskState::Leased, TaskState::Completed))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::IllegalTransition(_)), "{err:?}");

    let t = store
        .call(move |conn| repo::get_task(conn, id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(t.state, TaskState::Leased);
}

#[tokio::test]
async fn leases_conflict_and_expire() {
    let store = Store::open_in_memory().unwrap();
    let (project, _) = store
        .call(|conn| repo::register_project(conn, "demo", "C:/tmp/demo", None))
        .await
        .unwrap();
    let pid = project.id;
    let (t1, t2) = store
        .call(move |conn| {
            let a = repo::create_task(conn, new_task(pid, "a", TaskState::Ready))?;
            let b = repo::create_task(conn, new_task(pid, "b", TaskState::Ready))?;
            Ok((a, b))
        })
        .await
        .unwrap();

    let acquired = store
        .call(move |conn| repo::acquire_lease(conn, "file:src/lib.rs", t1.id, None, 60_000))
        .await
        .unwrap();
    assert!(acquired);

    // Same resource, different task -> conflict.
    let acquired = store
        .call(move |conn| repo::acquire_lease(conn, "file:src/lib.rs", t2.id, None, 60_000))
        .await
        .unwrap();
    assert!(!acquired);

    // Same task re-acquiring -> allowed (idempotent refresh).
    let acquired = store
        .call(move |conn| repo::acquire_lease(conn, "file:src/lib.rs", t1.id, None, 60_000))
        .await
        .unwrap();
    assert!(acquired);

    // Simulate expiry by rewriting expires_at into the past.
    store
        .call(|conn| {
            conn.execute(
                "UPDATE resource_leases SET expires_at = 1 WHERE resource = 'file:src/lib.rs'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let acquired = store
        .call(move |conn| repo::acquire_lease(conn, "file:src/lib.rs", t2.id, None, 60_000))
        .await
        .unwrap();
    assert!(acquired, "expired lease must be stealable");

    let released = store
        .call(move |conn| repo::release_leases_for_task(conn, t2.id))
        .await
        .unwrap();
    assert_eq!(released, vec!["file:src/lib.rs".to_string()]);

    let leases = store.call(|conn| repo::list_leases(conn)).await.unwrap();
    assert!(leases.is_empty());
}

#[tokio::test]
async fn events_journal_is_queryable() {
    let store = Store::open_in_memory().unwrap();
    let (project, _) = store
        .call(|conn| repo::register_project(conn, "demo", "C:/tmp/demo", None))
        .await
        .unwrap();
    let pid = project.id;
    let task = store
        .call(move |conn| repo::create_task(conn, new_task(pid, "t", TaskState::Created)))
        .await
        .unwrap();

    let ids: Vec<i64> = store
        .call({
            let tid = task.id;
            move |conn| {
                let mut ids = vec![];
                for i in 0..5 {
                    ids.push(
                        repo::insert_event(
                            conn,
                            &NewEvent {
                                project_id: Some(pid),
                                task_id: Some(tid),
                                agent_id: None,
                                kind: EventKind::DaemonNote,
                                severity: Severity::Info,
                                message: format!("event {i}"),
                                data: Some(serde_json::json!({ "i": i })),
                            },
                        )?
                        .id,
                    );
                }
                Ok(ids)
            }
        })
        .await
        .unwrap();
    assert!(ids.windows(2).all(|w| w[0] < w[1]));

    let recent = store
        .call({
            let tid = task.id;
            move |conn| repo::recent_events(conn, 3, None, Some(tid))
        })
        .await
        .unwrap();
    assert_eq!(recent.len(), 3);
    assert_eq!(recent.last().unwrap().message, "event 4");
    assert!(recent.windows(2).all(|w| w[0].id < w[1].id));

    let since = store
        .call({
            let tid = task.id;
            move |conn| repo::recent_events(conn, 10, Some(ids[2]), Some(tid))
        })
        .await
        .unwrap();
    assert_eq!(since.len(), 2);
    assert_eq!(since[0].message, "event 3");

    let none_for_other_task = store
        .call(|conn| repo::recent_events(conn, 10, None, Some(9999)))
        .await
        .unwrap();
    assert!(none_for_other_task.is_empty());
}

#[tokio::test]
async fn worktrees_are_unique_per_path_and_branch() {
    let store = Store::open_in_memory().unwrap();
    let (project, _) = store
        .call(|conn| repo::register_project(conn, "demo", "C:/tmp/demo", None))
        .await
        .unwrap();
    let pid = project.id;
    let (t1, t2) = store
        .call(move |conn| {
            let a = repo::create_task(conn, new_task(pid, "a", TaskState::Ready))?;
            let b = repo::create_task(conn, new_task(pid, "b", TaskState::Ready))?;
            Ok((a, b))
        })
        .await
        .unwrap();

    let wt = store
        .call(move |conn| {
            repo::insert_worktree(
                conn,
                pid,
                t1.id,
                std::path::Path::new("C:/tmp/wt/t-001"),
                "forge/task-001",
                Some("abc123"),
                WorktreeState::Active,
            )
        })
        .await
        .unwrap();
    assert_eq!(wt.state, WorktreeState::Active);

    let dup = store
        .call({
            let path = wt.path.clone();
            move |conn| {
                repo::insert_worktree(
                    conn,
                    pid,
                    t2.id,
                    &path,
                    "forge/task-002",
                    None,
                    WorktreeState::Active,
                )
            }
        })
        .await;
    assert!(dup.is_err(), "duplicate worktree path must fail");

    let found = store
        .call(move |conn| repo::get_worktree_by_task(conn, t1.id))
        .await
        .unwrap()
        .expect("worktree");
    assert_eq!(found.branch, "forge/task-001");

    store
        .call({
            let id = wt.id;
            move |conn| repo::set_worktree_state(conn, id, WorktreeState::Retained)
        })
        .await
        .unwrap();
    let active = store
        .call(|conn| repo::list_active_worktrees(conn))
        .await
        .unwrap();
    assert!(active.is_empty());
}

#[tokio::test]
async fn sessions_track_process_lifecycle() {
    let store = Store::open_in_memory().unwrap();
    let (project, _) = store
        .call(|conn| repo::register_project(conn, "demo", "C:/tmp/demo", None))
        .await
        .unwrap();
    let pid = project.id;
    let task = store
        .call(move |conn| repo::create_task(conn, new_task(pid, "t", TaskState::Ready)))
        .await
        .unwrap();

    let session_id = store
        .call({
            let tid = task.id;
            move |conn| {
                repo::open_session(
                    conn,
                    &repo::NewSession {
                        agent_id: "codex-a",
                        task_id: tid,
                        worktree_id: None,
                        owner_instance: "instance-1",
                        attempt_no: 0,
                        run_token: "token-1",
                    },
                )
            }
        })
        .await
        .unwrap();
    store
        .call(move |conn| {
            repo::set_session_running(
                conn,
                session_id,
                &repo::SessionProcessIdentity {
                    pid: 4242,
                    start_marker: Some(99),
                    fingerprint: "image|0".into(),
                },
            )
        })
        .await
        .unwrap();

    let open = store.call(|conn| repo::open_sessions(conn)).await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].pid, Some(4242));
    assert_eq!(open[0].state, SessionState::Running);
    assert_eq!(open[0].attempt_no, Some(0));
    assert_eq!(open[0].run_token.as_deref(), Some("token-1"));
    assert_eq!(open[0].owner_instance.as_deref(), Some("instance-1"));
    assert_eq!(open[0].pid_start_marker, Some(99));
    assert_eq!(open[0].pid_fingerprint.as_deref(), Some("image|0"));

    store
        .call(move |conn| {
            repo::close_session(
                conn,
                session_id,
                SessionState::Exited,
                Some(0),
                Some(forge_core::model::SessionTreeState::Empty),
            )
        })
        .await
        .unwrap();
    let open = store.call(|conn| repo::open_sessions(conn)).await.unwrap();
    assert!(open.is_empty());
    let all = store
        .call(move |conn| repo::sessions_for_task(conn, task.id))
        .await
        .unwrap();
    assert_eq!(
        all[0].tree_state,
        Some(forge_core::model::SessionTreeState::Empty),
        "the tree verdict is durable"
    );
}

#[tokio::test]
async fn artifacts_and_plan_idempotency() {
    let store = Store::open_in_memory().unwrap();
    let (project, _) = store
        .call(|conn| repo::register_project(conn, "demo", "C:/tmp/demo", None))
        .await
        .unwrap();
    let pid = project.id;
    let task = store
        .call(move |conn| repo::create_task(conn, new_task(pid, "t", TaskState::Completed)))
        .await
        .unwrap();

    let artifact = store
        .call({
            let tid = task.id;
            move |conn| {
                repo::insert_artifact(conn, tid, "git_diff_stat", None, Some("1 file changed"))
            }
        })
        .await
        .unwrap();
    assert_eq!(artifact.kind, "git_diff_stat");

    let artifacts = store
        .call({
            let tid = task.id;
            move |conn| repo::list_artifacts(conn, tid)
        })
        .await
        .unwrap();
    assert_eq!(artifacts.len(), 1);

    let (id1, created1) = store
        .call(move |conn| repo::create_plan(conn, pid, "req-1"))
        .await
        .unwrap();
    let (id2, created2) = store
        .call(move |conn| repo::create_plan(conn, pid, "req-1"))
        .await
        .unwrap();
    assert!(created1);
    assert!(!created2);
    assert_eq!(id1, id2);
}

#[tokio::test]
async fn agent_rows_roundtrip() {
    let store = Store::open_in_memory().unwrap();
    let spec = AgentSpec {
        id: "codex-a".into(),
        kind: forge_core::AgentKind::Codex,
        command: "codex".into(),
        extra_args: vec![],
        capabilities: vec!["code".into(), "tests".into()],
        slots: 2,
        env: Default::default(),
        timeout_secs: None,
        enabled: true,
        stdin_prompt: false,
    };
    store
        .call({
            let spec = spec.clone();
            move |conn| repo::upsert_agent(conn, &spec)
        })
        .await
        .unwrap();
    store
        .call(|conn| repo::set_agent_probe(conn, "codex-a", Some("0.5.0"), "ok", None))
        .await
        .unwrap();

    let agents = store.call(|conn| repo::list_agents(conn)).await.unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].kind, forge_core::AgentKind::Codex);
    assert_eq!(agents[0].slots, 2);
    assert_eq!(agents[0].version.as_deref(), Some("0.5.0"));
    assert_eq!(agents[0].probe_status, "ok");

    // Config update keeps probe results.
    let mut updated = spec.clone();
    updated.slots = 3;
    store
        .call(move |conn| repo::upsert_agent(conn, &updated))
        .await
        .unwrap();
    let agents = store.call(|conn| repo::list_agents(conn)).await.unwrap();
    assert_eq!(agents[0].slots, 3);
    assert_eq!(agents[0].version.as_deref(), Some("0.5.0"));
}

#[tokio::test]
async fn worker_supports_concurrent_async_callers() {
    let store = Store::open_in_memory().unwrap();
    let (project, _) = store
        .call(|conn| repo::register_project(conn, "demo", "C:/tmp/demo", None))
        .await
        .unwrap();
    let pid = project.id;

    let store = std::sync::Arc::new(store);
    let mut handles = vec![];
    for i in 0..16 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            let title = format!("task-{i}");
            store
                .call(move |conn| {
                    repo::create_task(conn, new_task(pid, &title, TaskState::Created))
                })
                .await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }

    let tasks = store
        .call(move |conn| repo::list_tasks(conn, pid))
        .await
        .unwrap();
    assert_eq!(tasks.len(), 16);
    // Sequence numbers must be unique and dense.
    let seqs: Vec<i64> = tasks.iter().map(|t| t.seq_no).collect();
    assert_eq!(seqs, (1..=16).collect::<Vec<_>>());
}

#[tokio::test]
async fn dependency_edges_are_ordered_and_cascaded() {
    let store = Store::open_in_memory().unwrap();
    let (project, _) = store
        .call(|conn| repo::register_project(conn, "demo", "C:/tmp/demo", None))
        .await
        .unwrap();
    let pid = project.id;
    let (a, b, c) = store
        .call(move |conn| {
            let a = repo::create_task(conn, new_task(pid, "a", TaskState::Created))?;
            let b = repo::create_task(conn, new_task(pid, "b", TaskState::Created))?;
            let c = repo::create_task(conn, new_task(pid, "c", TaskState::Created))?;
            repo::add_dependency(conn, b.id, a.id)?;
            repo::add_dependency(conn, c.id, b.id)?;
            Ok((a, b, c))
        })
        .await
        .unwrap();

    let edges = store
        .call(move |conn| repo::all_dependencies(conn, pid))
        .await
        .unwrap();
    assert_eq!(edges, vec![(b.id, a.id), (c.id, b.id)]);

    let dependents = store
        .call(move |conn| repo::dependents_of(conn, a.id))
        .await
        .unwrap();
    assert_eq!(dependents, vec![b.id]);

    // Deleting a task cascades its dependency edges (FK ON DELETE CASCADE).
    store
        .call(move |conn| {
            conn.execute("DELETE FROM tasks WHERE id = ?1", rusqlite::params![b.id])?;
            Ok(())
        })
        .await
        .unwrap();
    let edges = store
        .call(move |conn| repo::all_dependencies(conn, pid))
        .await
        .unwrap();
    assert!(edges.is_empty());
}

#[tokio::test]
async fn direct_db_readonly_can_read_while_store_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let store = Store::open(&db_path).unwrap();
    store
        .call(|conn| {
            repo::register_project(conn, "demo", "C:/tmp/demo", None)?;
            Ok(())
        })
        .await
        .unwrap();

    // Read-only peer connection (WAL: readers never block the writer).
    let db = DirectDb::open_readonly(&db_path).unwrap();
    let projects = repo::list_projects(db.get()).unwrap();
    assert_eq!(projects.len(), 1);
}
