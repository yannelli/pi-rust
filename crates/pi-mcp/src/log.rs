//! `mcp.log`: log messages servers send with `notifications/message`. Several pi processes may write
//! to the file, so each message is one append. It rotates to `mcp.log.1` past 5 MB.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` for milliseconds since the epoch.
pub fn iso_timestamp(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rest = ms.rem_euclid(86_400_000);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (hours, minutes, seconds, millis) = (rest / 3_600_000, rest / 60_000 % 60, rest / 1000 % 60, rest % 1000);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z")
}

fn format_data(data: Option<&Value>) -> String {
    match data {
        None => "undefined".into(),
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
    }
}

/// One `notifications/message` from `server` as a log line; continuation lines are indented.
pub fn format_mcp_log_message(server: &str, params: Option<&Value>, now_ms: i64) -> String {
    let message = params.filter(|params| params.is_object());
    let data = match message {
        Some(message) => message.get("data"),
        None => params,
    };
    let field = |key: &str| message.and_then(|message| message.get(key)).and_then(Value::as_str);
    let level = field("level").unwrap_or("info");
    let logger =
        field("logger").filter(|logger| !logger.is_empty()).map(|logger| format!(" {logger}:")).unwrap_or_default();
    let text = format_data(data).replace("\r\n", "\n").replace('\n', "\n    ");
    format!("{} [{server}] {level}{logger} {text}\n", iso_timestamp(now_ms))
}

/// Appends server log messages to one file. Write errors are ignored: logging must not break tools.
pub struct McpServerLog {
    path: PathBuf,
    size: Mutex<Option<u64>>,
}

impl McpServerLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), size: Mutex::new(None) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn current_size(&self) -> u64 {
        std::fs::metadata(&self.path).map(|metadata| metadata.len()).unwrap_or(0)
    }

    pub fn write(&self, server: &str, params: Option<&Value>) {
        let line = format_mcp_log_message(server, params, pi_ai::now_ms());
        let mut size = self.size.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = (|| -> std::io::Result<()> {
            if size.is_none() {
                if let Some(parent) = self.path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                *size = Some(self.current_size());
            }
            if size.unwrap_or(0) > MAX_LOG_BYTES {
                // Another process may have rotated it already; check before renaming.
                if self.current_size() > MAX_LOG_BYTES {
                    let mut rotated = self.path.clone().into_os_string();
                    rotated.push(".1");
                    std::fs::rename(&self.path, rotated)?;
                }
                *size = Some(self.current_size());
            }
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
            file.write_all(line.as_bytes())?;
            *size = Some(size.unwrap_or(0) + line.len() as u64);
            Ok(())
        })();
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn formats_log_lines() {
        assert_eq!(iso_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_timestamp(1_709_251_199_123), "2024-02-29T23:59:59.123Z");
        let params = json!({ "level": "warning", "logger": "db", "data": "slow\nquery" });
        assert_eq!(
            format_mcp_log_message("fake", Some(&params), 0),
            "1970-01-01T00:00:00.000Z [fake] warning db: slow\n    query\n"
        );
        let params = json!({ "level": "error", "data": { "code": 7 } });
        assert!(format_mcp_log_message("fake", Some(&params), 0).ends_with("[fake] error {\"code\":7}\n"));
        assert!(format_mcp_log_message("fake", Some(&json!("plain")), 0).ends_with("[fake] info plain\n"));
    }

    #[test]
    fn rotates_past_the_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("mcp.log");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize + 1]).unwrap();
        let log = McpServerLog::new(&path);
        log.write("s", Some(&json!({ "data": "first" })));
        let rotated = dir.path().join("logs").join("mcp.log.1");
        assert_eq!(std::fs::metadata(&rotated).unwrap().len(), MAX_LOG_BYTES + 1);
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("[s] info first\n"));
        log.write("s", Some(&json!({ "data": "second" })));
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
    }
}
