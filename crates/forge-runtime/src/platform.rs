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
        let filter = format!("PID eq {pid}");
        match std::process::Command::new("tasklist")
            .args(["/FI", &filter, "/NH"])
            .output()
        {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                text.split_whitespace()
                    .any(|tok| tok.parse::<u32>() == Ok(pid))
            }
            Err(_) => false,
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
