//! Platform-specific process helpers: executable resolution, process
//! groups, tree termination and liveness checks.
//!
//! On Unix, children are spawned as process-group leaders (`setpgid`) and
//! terminated with `killpg`. On Windows, children are created with
//! `CREATE_NEW_PROCESS_GROUP` and terminated with `taskkill /T`.

use std::io;
use std::path::{Path, PathBuf};

/// Candidate file names to try when resolving a bare command name on PATH.
///
/// On Windows an extensionless file (e.g. the shell script that npm installs
/// next to `opencode.cmd`) is not executable, so extension candidates come
/// first and the bare name is only tried when it already contains an
/// extension.
pub fn executable_candidates(name: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        if name.contains('.') {
            vec![name.to_string()]
        } else {
            vec![
                format!("{name}.exe"),
                format!("{name}.cmd"),
                format!("{name}.bat"),
                format!("{name}.com"),
            ]
        }
    }
    #[cfg(not(windows))]
    {
        vec![name.to_string()]
    }
}

/// npm installs `.cmd` shims (e.g. `opencode.cmd`) that exec the package's
/// real binary. Resolving the shim to the underlying executable avoids
/// `cmd.exe` entirely, which is both safer (no cmd parsing of arguments)
/// and more robust (no quoting hazards) for arbitrary prompt text.
#[cfg(windows)]
pub fn resolve_npm_shim(shim: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(shim).ok()?;
    let dir = shim.parent()?;
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(start) = trimmed.find("\"%dp0%\\") else {
            continue;
        };
        let rest = &trimmed[start + 1..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        let candidate = rest[..end].replace("%dp0%", &dir.to_string_lossy());
        let candidate = PathBuf::from(candidate);
        let is_exe = candidate
            .extension()
            .map(|e| e.eq_ignore_ascii_case("exe"))
            .unwrap_or(false);
        if is_exe && candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(not(windows))]
pub fn resolve_npm_shim(_shim: &Path) -> Option<PathBuf> {
    None
}

/// Resolve an agent command to an executable path.
///
/// * `@self` resolves to the currently running `forge` binary,
/// * paths containing a separator are used as-is,
/// * bare names are searched on `PATH` (on Windows also trying
///   `.exe`, `.cmd`, `.bat`, `.com` — never `.ps1`, which would require
///   PowerShell and execution-policy changes),
/// * `.cmd`/`.bat` npm shims are resolved to the real executable they wrap
///   when possible (see [`resolve_npm_shim`]).
pub fn resolve_executable(name: &str) -> Option<PathBuf> {
    if name == "@self" {
        return std::env::current_exe().ok();
    }
    let looks_like_path = name.contains('/') || name.contains('\\');
    let path = Path::new(name);
    if looks_like_path || path.is_absolute() {
        return if path.is_file() {
            Some(resolve_npm_shim(path).unwrap_or_else(|| path.to_path_buf()))
        } else {
            None
        };
    }
    let path_var = std::env::var_os("PATH")?;
    let candidates = executable_candidates(name);
    for dir in std::env::split_paths(&path_var) {
        for candidate in &candidates {
            let full = dir.join(candidate);
            if full.is_file() {
                return Some(resolve_npm_shim(&full).unwrap_or(full));
            }
        }
    }
    None
}

/// On Windows, `.cmd`/`.bat` files cannot be executed directly by
/// `CreateProcess`; they are launched through `cmd.exe /C`.
#[cfg(windows)]
pub fn maybe_wrap_script(program: PathBuf, args: Vec<String>) -> (PathBuf, Vec<String>) {
    let is_script = program
        .extension()
        .map(|e| {
            let e = e.to_string_lossy().to_ascii_lowercase();
            e == "cmd" || e == "bat"
        })
        .unwrap_or(false);
    if is_script {
        let mut new_args = vec!["/C".to_string(), program.to_string_lossy().to_string()];
        new_args.extend(args);
        (PathBuf::from("cmd.exe"), new_args)
    } else {
        (program, args)
    }
}

#[cfg(not(windows))]
pub fn maybe_wrap_script(program: PathBuf, args: Vec<String>) -> (PathBuf, Vec<String>) {
    (program, args)
}

// ---------------------------------------------------------------------------
// Process identity (finding A01)
// ---------------------------------------------------------------------------

/// What the OS reports about one process.
///
/// * `start_marker` is an opaque OS start stamp (process creation time on
///   Windows, `/proc` start ticks on Linux). It is only compared for equality,
///   never interpreted as a clock.
/// * `fingerprint` is a stable description of the process image (executable
///   path plus parent PID on Windows; command line on Linux). A start stamp is
///   **coarse** — a parent and its child can share one — so the stamp alone is
///   never treated as identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsProcess {
    pub pid: u32,
    pub start_marker: Option<u64>,
    pub fingerprint: String,
}

/// The durable identity of a process Forge owns.
///
/// A PID alone is never identity: the OS reuses PIDs, so every ownership
/// decision (kill, escalation, recovery) must compare the start marker **and**
/// the image fingerprint recorded when the process was spawned with what the OS
/// reports now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Start marker recorded at spawn time. `None` means "not recorded"; the
    /// identity then never matches (fail closed).
    pub start_marker: Option<u64>,
    /// Image fingerprint recorded at spawn time. Empty means "not recorded";
    /// the identity then never matches (fail closed).
    pub fingerprint: String,
    /// Attempt token that owns the process. Audit trail only: it cannot be read
    /// from another process, so it is never used as an authorization check.
    pub run_token: String,
}

impl ProcessIdentity {
    pub fn new(
        pid: u32,
        start_marker: Option<u64>,
        fingerprint: impl Into<String>,
        run_token: impl Into<String>,
    ) -> Self {
        Self {
            pid,
            start_marker,
            fingerprint: fingerprint.into(),
            run_token: run_token.into(),
        }
    }

    /// True when this record still describes the very same OS process.
    ///
    /// Fail closed: a missing recorded marker, a missing current marker, a
    /// missing fingerprint, a dead process or a process we may not inspect all
    /// answer `false`, which means "not provably ours" — never "kill it anyway".
    pub fn matches_os(&self) -> bool {
        if self.pid == 0 || self.fingerprint.is_empty() {
            return false;
        }
        let Some(current) = process_snapshot(self.pid) else {
            return false;
        };
        if current.fingerprint.is_empty() || current.fingerprint != self.fingerprint {
            return false;
        }
        match (self.start_marker, current.start_marker) {
            (Some(recorded), Some(now)) => recorded == now,
            _ => false,
        }
    }
}

/// OS-level snapshot of `pid`. `None` means "not running, or not inspectable by
/// this process" — both are treated as "cannot prove ownership".
pub fn process_snapshot(pid: u32) -> Option<OsProcess> {
    if pid == 0 {
        return None;
    }
    #[cfg(windows)]
    {
        windows_process_snapshot(pid)
    }
    #[cfg(all(unix, target_os = "linux"))]
    {
        linux_process_snapshot(pid)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // No portable start marker on this platform: report liveness only and
        // let identity checks fail closed.
        if process_alive(pid) {
            Some(OsProcess {
                pid,
                start_marker: None,
            })
        } else {
            None
        }
    }
}

#[cfg(windows)]
fn windows_process_snapshot(pid: u32) -> Option<OsProcess> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
        PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: OpenProcess returns a handle or null; it is closed on every path.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        // A terminated process keeps its PID (and creation time) alive as long as
        // somebody holds a handle to it. Only a process that is still running may
        // be treated as "alive" or as "the process we own".
        let mut exit_code: u32 = 0;
        let running = GetExitCodeProcess(handle, &mut exit_code) != 0 && exit_code == STILL_ACTIVE as u32;
        if !running {
            CloseHandle(handle);
            return None;
        }
        let mut creation = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exit = creation;
        let mut kernel = creation;
        let mut user = creation;
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        if ok == 0 {
            CloseHandle(handle);
            return None;
        }
        // The executable image is part of the identity: a recycled PID pointing
        // at a different program can never match.
        let mut image_buf = vec![0u16; 1024];
        let mut image_len = image_buf.len() as u32;
        let image_ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            image_buf.as_mut_ptr(),
            &mut image_len,
        );
        CloseHandle(handle);
        let image = if image_ok != 0 && image_len > 0 {
            let end = image_buf
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(image_len as usize);
            String::from_utf16_lossy(&image_buf[..end]).to_ascii_lowercase()
        } else {
            String::new()
        };
        let parent = windows_parent_pid(pid).unwrap_or(0);
        Some(OsProcess {
            pid,
            start_marker: Some(filetime_to_marker(creation)),
            fingerprint: format!("{image}|{parent}"),
        })
    }
}

/// Parent PID from a Toolhelp snapshot; `None` when the snapshot fails.
#[cfg(windows)]
fn windows_parent_pid(pid: u32) -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: the snapshot handle is closed on every path; the entry struct is
    // initialized with its documented size before the first call.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut parent = None;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == pid {
                    parent = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        parent
    }
}

/// Windows `FILETIME` (100 ns ticks since 1601-01-01) as one opaque 64-bit
/// marker. Only equality matters, so no calendar conversion is performed.
#[cfg(windows)]
fn filetime_to_marker(time: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64
}

#[cfg(all(unix, target_os = "linux"))]
fn linux_process_snapshot(pid: u32) -> Option<OsProcess> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 2 may contain spaces; the starttime is field 22 counted from the
    // last ')', which is where the process name ends.
    let after_name = stat.rsplit_once(')')?.1;
    let starttime = after_name.split_whitespace().nth(19)?;
    // The command line is part of the identity: the start tick is coarse, so a
    // recycled PID pointing at a different program can never match.
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| {
            bytes
                .split(|b| *b == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).to_string())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    Some(OsProcess {
        pid,
        start_marker: starttime.parse::<u64>().ok(),
        fingerprint: cmdline,
    })
}

// ---------------------------------------------------------------------------
// Process tree ownership (finding A03)
// ---------------------------------------------------------------------------

/// Ownership handle for one execution tree.
///
/// * **Windows**: a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The
///   child is assigned immediately after `CreateProcess`; terminating the job
///   reaches every descendant even after the direct parent exited, and a daemon
///   that dies closes the handle, which kills the whole tree.
/// * **Unix**: the child's own process group (`setpgid` at spawn); `killpg`
///   reaches every member.
///
/// The guard is created before the spawn and attached right after it, so the
/// window in which a process is outside Forge's ownership is limited to process
/// startup — and the agent cannot do real work in that window, because its
/// prompt is only written after the durable attempt record is committed.
pub struct TreeGuard {
    inner: TreeInner,
}

#[cfg(windows)]
struct TreeInner {
    job: windows_sys::Win32::Foundation::HANDLE,
    pid: std::sync::atomic::AtomicU32,
    /// True while the job still kills its members when the handle closes.
    kill_on_close: std::sync::atomic::AtomicBool,
}

#[cfg(unix)]
struct TreeInner {
    pid: std::sync::atomic::AtomicU32,
}

// The handle is a plain pointer but every Win32 call on it is thread safe.
unsafe impl Send for TreeGuard {}
unsafe impl Sync for TreeGuard {}

impl TreeGuard {
    /// Create the OS container for one run.
    pub fn create() -> Self {
        #[cfg(windows)]
        {
            use std::sync::atomic::{AtomicBool, AtomicU32};
            use windows_sys::Win32::System::JobObjects::{
                CreateJobObjectW, SetInformationJobObject, JobObjectExtendedLimitInformation,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            // SAFETY: the job is created with default security attributes and no
            // name; the handle is owned by this guard and closed in Drop.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                tracing::warn!("CreateJobObjectW failed; falling back to PID-scoped tree kill");
                return Self {
                    inner: TreeInner {
                        job,
                        pid: AtomicU32::new(0),
                        kill_on_close: AtomicBool::new(false),
                    },
                };
            }
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `info` is a fully initialized struct of the documented size.
            let ok = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const std::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            let kill_on_close = ok != 0;
            if !kill_on_close {
                tracing::warn!(
                    error = std::io::Error::last_os_error().to_string(),
                    "cannot set KILL_ON_JOB_CLOSE; the tree is terminated per run only"
                );
            }
            return Self {
                inner: TreeInner {
                    job,
                    pid: AtomicU32::new(0),
                    kill_on_close: AtomicBool::new(kill_on_close),
                },
            };
        }
        #[cfg(unix)]
        {
            Self {
                inner: TreeInner {
                    pid: std::sync::atomic::AtomicU32::new(0),
                },
            }
        }
    }

    /// Attach the freshly spawned child to this tree.
    pub fn attach(&self, child: &tokio::process::Child) -> Result<(), String> {
        let pid = child.id().unwrap_or(0);
        self.inner
            .pid
            .store(pid, std::sync::atomic::Ordering::SeqCst);
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
            if self.inner.job.is_null() {
                return Err("no job object".into());
            }
            let Some(raw) = child.raw_handle() else {
                return Err("child has no OS handle".into());
            };
            // SAFETY: both handles are valid for the duration of the call.
            let ok = unsafe { AssignProcessToJobObject(self.inner.job, raw as _) };
            if ok == 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        #[cfg(unix)]
        {
            let _ = child;
        }
        Ok(())
    }

    pub fn pid(&self) -> u32 {
        self.inner.pid.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Number of live processes in the tree, when the platform can report it.
    pub fn live_count(&self) -> Option<usize> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::{
                QueryInformationJobObject, JobObjectBasicProcessIdList,
                JOBOBJECT_BASIC_PROCESS_ID_LIST,
            };
            if self.inner.job.is_null() {
                return None;
            }
            // Room for a generous process list; the query returns the real count.
            const MAX_PIDS: usize = 256;
            let size = std::mem::size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>()
                + MAX_PIDS * std::mem::size_of::<usize>();
            let mut buf = vec![0u8; size];
            // SAFETY: the buffer is at least as large as the struct we cast to.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.inner.job,
                    JobObjectBasicProcessIdList,
                    buf.as_mut_ptr() as *mut std::ffi::c_void,
                    size as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return None;
            }
            // SAFETY: the call filled the buffer with the documented struct.
            let list = unsafe { &*(buf.as_ptr() as *const JOBOBJECT_BASIC_PROCESS_ID_LIST) };
            Some(list.NumberOfAssignedProcesses as usize)
        }
        #[cfg(unix)]
        {
            None
        }
    }

    /// True when the OS confirms that no process of this tree is alive.
    /// Fail closed: "unknown" is reported as `false` (not empty).
    pub fn is_empty(&self) -> bool {
        let pid = self.pid();
        if pid == 0 {
            return true;
        }
        #[cfg(windows)]
        {
            matches!(self.live_count(), Some(0))
        }
        #[cfg(unix)]
        {
            // kill(-pgid, 0) == 0 means the group still has members.
            // SAFETY: signal 0 performs the permission and existence check only.
            let rc = unsafe { libc::kill(-(pid as i32), 0) };
            if rc == 0 {
                return false;
            }
            std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        }
    }

    /// Terminate the whole tree. `graceful` asks for a polite signal first
    /// (SIGTERM on Unix); Windows has no graceful console kill, so the job is
    /// terminated either way — the escalation policy stays the same.
    pub fn terminate(&self, graceful: bool) -> std::io::Result<()> {
        let pid = self.pid();
        if pid == 0 {
            return Ok(());
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            let _ = graceful;
            if !self.inner.job.is_null() {
                // SAFETY: the job handle is valid and owned by this guard.
                let ok = unsafe { TerminateJobObject(self.inner.job, 1) };
                if ok != 0 {
                    return Ok(());
                }
            }
            // Fallback: identity is checked by the caller, so a PID-scoped
            // taskkill cannot hit an unrelated process.
            kill_tree(pid, true)
        }
        #[cfg(unix)]
        {
            let sig = if graceful { libc::SIGTERM } else { libc::SIGKILL };
            // SAFETY: negative pid targets the whole process group.
            unsafe {
                libc::kill(-(pid as i32), sig);
            }
            Ok(())
        }
    }

    /// Release the tree without killing it (policy `report`): the processes keep
    /// running and Forge stops claiming ownership.
    pub fn detach(&self) {
        #[cfg(windows)]
        {
            use std::sync::atomic::Ordering;
            use windows_sys::Win32::System::JobObjects::{
                SetInformationJobObject, JobObjectExtendedLimitInformation,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            };
            if self.inner.job.is_null() {
                return;
            }
            if self.inner.kill_on_close.swap(false, Ordering::SeqCst) {
                let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                // SAFETY: clearing the limit flags is safe with a zeroed struct.
                unsafe {
                    SetInformationJobObject(
                        self.inner.job,
                        JobObjectExtendedLimitInformation,
                        &info as *const _ as *const std::ffi::c_void,
                        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    );
                }
            }
        }
        #[cfg(unix)]
        {
            // A process group is not an owned resource on Unix: nothing to release.
        }
    }

    /// PIDs currently inside the tree (diagnostics; Windows only).
    pub fn pids(&self) -> Vec<u32> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::{
                QueryInformationJobObject, JobObjectBasicProcessIdList,
                JOBOBJECT_BASIC_PROCESS_ID_LIST,
            };
            if self.inner.job.is_null() {
                return vec![];
            }
            const MAX_PIDS: usize = 256;
            let size = std::mem::size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>()
                + MAX_PIDS * std::mem::size_of::<usize>();
            let mut buf = vec![0u8; size];
            // SAFETY: the buffer is at least as large as the struct we cast to.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.inner.job,
                    JobObjectBasicProcessIdList,
                    buf.as_mut_ptr() as *mut std::ffi::c_void,
                    size as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return vec![];
            }
            // SAFETY: the call filled the buffer with the documented struct.
            let list = unsafe { &*(buf.as_ptr() as *const JOBOBJECT_BASIC_PROCESS_ID_LIST) };
            let count = (list.NumberOfProcessIdsInList as usize).min(MAX_PIDS);
            let mut out = vec![];
            for i in 0..count {
                // SAFETY: `count` was clamped to the allocated capacity.
                let pid = unsafe { *list.ProcessIdList.as_ptr().add(i) } as u32;
                out.push(pid);
            }
            out
        }
        #[cfg(unix)]
        {
            vec![]
        }
    }
}

#[cfg(windows)]
impl Drop for TreeGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        if self.inner.job.is_null() {
            return;
        }
        // With KILL_ON_JOB_CLOSE still set, closing the last handle kills every
        // process still inside the job: a daemon that dies cannot leak agents.
        // SAFETY: the handle is owned by this guard and closed exactly once.
        unsafe {
            CloseHandle(self.inner.job);
        }
    }
}

/// Configure a Tokio command for detached process-group semantics.
pub fn configure_command(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let _ = cmd;
}

/// Kill a process tree. `force` escalates to SIGKILL / `taskkill /F`.
///
/// Windows note: console processes cannot be terminated gracefully by
/// `taskkill` without `/F` (documented OS behaviour), so the Windows
/// implementation always terminates forcefully; `force` only affects Unix
/// (SIGTERM vs SIGKILL).
pub fn kill_tree(pid: u32, force: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
        // Negative pid targets the whole process group.
        unsafe {
            libc::kill(-(pid as i32), sig);
        }
        if force {
            unsafe {
                libc::kill(pid as i32, sig);
            }
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = force;
        let args = vec![
            "/PID".to_string(),
            pid.to_string(),
            "/T".to_string(),
            "/F".to_string(),
        ];
        // taskkill with a nonexistent PID returns non-zero; treat that as ok.
        let _ = std::process::Command::new("taskkill")
            .args(&args)
            .output()?;
        Ok(())
    }
}

/// Best-effort liveness check.
///
/// Liveness is **not** ownership: callers deciding whether to terminate a
/// process must use [`ProcessIdentity::matches_os`] instead.
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: the handle is closed on every path and never used after.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            // A terminated process keeps a PID as long as a handle to it is open;
            // only a process still running counts as alive.
            let mut exit_code: u32 = 0;
            let running =
                GetExitCodeProcess(handle, &mut exit_code) != 0 && exit_code == STILL_ACTIVE as u32;
            CloseHandle(handle);
            running
        }
    }
}

/// Set owner-only permissions on a directory or file (no-op on Windows,
/// where the user profile ACL already restricts access).
pub fn set_owner_only_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::symlink_metadata(path)?;
        let mode = if meta.is_dir() { 0o700 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn resolves_absolute_path() {
        let exe = std::env::current_exe().unwrap();
        let resolved = resolve_executable(&exe.to_string_lossy()).unwrap();
        assert_eq!(resolved, exe);
    }

    #[test]
    fn missing_binary_is_none() {
        assert!(resolve_executable("definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn candidate_order_prefers_executables() {
        let candidates = executable_candidates("opencode");
        #[cfg(windows)]
        {
            assert_eq!(
                candidates,
                vec![
                    "opencode.exe".to_string(),
                    "opencode.cmd".to_string(),
                    "opencode.bat".to_string(),
                    "opencode.com".to_string(),
                ]
            );
            // Names that already carry an extension are used verbatim.
            assert_eq!(
                executable_candidates("foo.bat"),
                vec!["foo.bat".to_string()]
            );
        }
        #[cfg(not(windows))]
        {
            assert_eq!(candidates, vec!["opencode".to_string()]);
        }
    }

    #[test]
    fn current_process_is_alive() {
        assert!(process_alive(std::process::id()));
    }

    #[test]
    fn bogus_pid_is_dead() {
        // 4 billion-ish pid is not in use on any supported platform.
        assert!(!process_alive(4_000_000_000u32));
    }

    /// Documents the Windows batch-file hazard: cmd.exe re-parses the
    /// command line, so metacharacters in arguments are NOT preserved.
    /// Forge therefore resolves npm shims to real executables
    /// (`resolve_npm_shim`) and never sends prompts through a batch file's
    /// argument list.
    #[cfg(windows)]
    #[test]
    fn cmd_metacharacters_do_not_survive_batch_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("echoargs.cmd");
        std::fs::write(&script, "@echo off\r\n(echo %~1)>\"%~dp0out.txt\"\r\n").unwrap();
        let nasty = "alpha & beta".to_string();
        let _ = crate::adapter::run_capture(
            &script,
            std::slice::from_ref(&nasty),
            Duration::from_secs(15),
        )
        .expect("run batch script");
        let written = std::fs::read_to_string(dir.path().join("out.txt")).unwrap_or_default();
        assert_ne!(
            written.trim(),
            nasty,
            "cmd.exe unexpectedly preserved metacharacters; batch-file handling can be simplified"
        );
    }

    #[cfg(windows)]
    #[test]
    fn npm_shim_resolves_to_real_executable() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules").join("acme").join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let real = bin_dir.join("acme.exe");
        std::fs::write(&real, b"fake").unwrap();
        let shim = dir.path().join("acme.cmd");
        std::fs::write(
            &shim,
            "@ECHO off\r\nGOTO start\r\n:start\r\n\"%dp0%\\node_modules\\acme\\bin\\acme.exe\"   %*\r\n",
        )
        .unwrap();
        let resolved = resolve_npm_shim(&shim).expect("shim resolves");
        assert_eq!(resolved, real);
        // resolve_executable uses the same rule for explicit paths.
        assert_eq!(resolve_executable(&shim.to_string_lossy()).unwrap(), real);
    }
}
