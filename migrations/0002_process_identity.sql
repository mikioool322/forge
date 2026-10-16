-- 0002_process_identity: durable attempt identity and process ownership.
--
-- Sprint 01.5A (audit findings A01/A02/A03/A05):
--   * every agent session row is one *attempt* of a task: `attempt_no` plus a
--     per-attempt `run_token` make a retry a different attempt, so events,
--     timeouts or cancellations of an old attempt can never be applied to a new
--     one,
--   * the process identity is recorded next to the PID: the OS start marker and
--     an image fingerprint. Recovery must prove ownership with both before it
--     may terminate anything; a PID alone is never identity,
--   * `owner_instance` records which daemon instance owns the attempt,
--   * `tree_state` records how the process tree ended (EMPTY/TERMINATED/
--     SURVIVED/UNVERIFIED), so a task is never reported as fully closed while
--     descendants may still be running.
--
-- Every column is nullable and additive: existing rows keep working, nothing is
-- rewritten, and a database created by version 1 migrates in place.

ALTER TABLE agent_sessions ADD COLUMN attempt_no INTEGER;
ALTER TABLE agent_sessions ADD COLUMN run_token TEXT;
ALTER TABLE agent_sessions ADD COLUMN pid_start_marker INTEGER;
ALTER TABLE agent_sessions ADD COLUMN pid_fingerprint TEXT;
ALTER TABLE agent_sessions ADD COLUMN owner_instance TEXT;
ALTER TABLE agent_sessions ADD COLUMN tree_state TEXT;

-- Recovery looks up unsettled attempts per task; the index keeps that bounded.
CREATE INDEX IF NOT EXISTS idx_sessions_attempt ON agent_sessions(task_id, attempt_no);
