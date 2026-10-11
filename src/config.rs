use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Persistent daemon configuration.
/// Lives at `%AppData%\ghelper-watcher\config.json` (NOT the G-Helper config).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Executables to watch, e.g. `["cs2.exe", "cyberpunk2077.exe"]` (case-insensitive).
    pub watched: Vec<String>,
    /// Profile applied when no watched process is running.
    pub default_profile: String,
    /// Profile applied when a watched process starts.
    pub trigger_profile: String,
    /// Manual-override pause length in minutes.
    pub pause_minutes: u64,
    /// Min seconds between auto-switches (absorbs launch flapping, e.g. R6).
    /// 0 disables. Re-evaluation happens automatically after it lapses.
    #[serde(default = "default_cooldown")]
    pub switch_cooldown_secs: u64,
    /// Pause auto-switching while on battery power.
    #[serde(default)]
    pub pause_on_battery: bool,
    /// Pause expiry as unix millis (shared with the dashboard child process).
    #[serde(default)]
    pub paused_until_millis: Option<i64>,
    /// Event source state ("wmi" once subscribed, shown by the dashboard).
    #[serde(default)]
    pub event_source: Option<String>,
    /// Sticky preference: always run elevated (logon task). Converged at startup.
    #[serde(default)]
    pub run_as_admin: bool,
    /// Last known daemon elevation (written by the daemon; read by dashboard).
    #[serde(default)]
    pub daemon_elevated: Option<bool>,
    /// Build stamp of the running daemon (stale dashboard children exit on mismatch).
    #[serde(default)]
    pub daemon_build: Option<String>,
    /// Explicit restart requested (dashboard button). Consumed by the daemon.
    #[serde(default)]
    pub restart_request: bool,
    /// Reopen the dashboard after the restart lands (restart UAC would
    /// otherwise eat the window the button was pressed in).
    #[serde(default)]
    pub reopen_dashboard: bool,
    /// Last auto-switch time as unix millis (written by daemon; read by
    /// dashboard for the cooldown countdown).
    #[serde(default)]
    pub last_switch_millis: Option<i64>,
}

fn default_cooldown() -> u64 {
    60
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            watched: vec!["cs2.exe".to_string()],
            default_profile: "Silent".to_string(),
            trigger_profile: "Turbo".to_string(),
            pause_minutes: 10,
            switch_cooldown_secs: default_cooldown(),
            pause_on_battery: false,
            paused_until_millis: None,
            event_source: None,
            run_as_admin: false,
            daemon_elevated: None,
            daemon_build: None,
            restart_request: false,
            reopen_dashboard: false,
            last_switch_millis: None,
        }
    }
}

impl AppConfig {
    pub fn path() -> Result<PathBuf> {
        let appdata =
            std::env::var("APPDATA").context("APPDATA env var not set (Windows required)")?;
        Ok(PathBuf::from(appdata)
            .join("ghelper-watcher")
            .join("config.json"))
    }

    pub fn load() -> Self {
        let path = match Self::path() {
            Ok(p) => p,
            Err(_) => return Self::default(),
        };
        let Ok(data) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&data).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, data)?;
        Ok(())
    }
}
