//! Small shared helpers for the CLI: time formatting, log tails, sizes.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};

/// `HH:MM:SS` in UTC.
pub fn format_hms(ms: i64) -> String {
    let dt: DateTime<Utc> = Utc.timestamp_millis_opt(ms).single().unwrap_or_default();
    dt.format("%H:%M:%S").to_string()
}

/// `YYYY-MM-DD HH:MM:SS` in UTC.
pub fn format_datetime(ms: i64) -> String {
    let dt: DateTime<Utc> = Utc.timestamp_millis_opt(ms).single().unwrap_or_default();
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Compact duration since a timestamp: `12s`, `3m05s`, `2h10m`.
pub fn format_ago(ms: i64, now: i64) -> String {
    let delta = (now - ms).max(0) / 1000;
    if delta < 60 {
        format!("{delta}s")
    } else if delta < 3600 {
        format!("{}m{:02}s", delta / 60, delta % 60)
    } else {
        format!("{}h{:02}m", delta / 3600, (delta % 3600) / 60)
    }
}

/// Compact byte size.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Read at most `lines` trailing lines from a file, reading at most
/// `max_bytes` from the end. Returns `(text, truncated)`.
pub fn read_log_tail(
    path: &Path,
    lines: usize,
    max_bytes: usize,
) -> std::io::Result<(String, bool)> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let read_from = len.saturating_sub(max_bytes as u64);
    file.seek(SeekFrom::Start(read_from))?;
    let mut buf = Vec::with_capacity((len - read_from) as usize);
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let mut all_lines: Vec<&str> = text.lines().collect();
    let truncated_at_start = read_from > 0;
    let dropped = all_lines.len().saturating_sub(lines);
    let truncated = truncated_at_start || dropped > 0;
    if dropped > 0 {
        all_lines.drain(..dropped);
    }
    Ok((all_lines.join("\n"), truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(format_hms(0), "00:00:00");
        assert_eq!(format_ago(0, 12_500), "12s");
        assert_eq!(format_ago(0, 65_000), "1m05s");
        assert_eq!(format_ago(0, 7_300_000), "2h01m");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
    }

    #[test]
    fn log_tail_reads_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        let content: String = (0..100).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, content).unwrap();
        let (text, truncated) = read_log_tail(&path, 10, 1024 * 1024).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0], "line 90");
        assert!(truncated);

        let (text, truncated) = read_log_tail(&path, 200, 1024 * 1024).unwrap();
        assert_eq!(text.lines().count(), 100);
        assert!(!truncated);

        let (text, truncated) = read_log_tail(&path, 100, 32).unwrap();
        assert!(truncated);
        assert!(!text.is_empty());
    }
}
