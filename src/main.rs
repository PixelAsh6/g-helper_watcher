#![windows_subsystem = "windows"]
//! ghelper-watcher: tray-first daemon that auto-switches G-Helper profiles
//! on process start/stop via WMI events, with manual-override protection.
//! Silent by default: no console window unless something actually errors.

/// Info logs: file always, terminal only when launched from one.
/// (Plain `eprintln!` would panic with no console attached, so never use it.)
macro_rules! log_info {
    ($($t:tt)*) => {{
        let m = format!($($t)*);
        crate::log_file(&format!("INFO {m}"));
        if crate::CONSOLE.load(std::sync::atomic::Ordering::SeqCst) == 1 {
            eprintln!("{m}");
        }
    }};
}

/// Errors: file always, plus terminal — popping a console window if needed.
macro_rules! log_error {
    ($($t:tt)*) => {{
        let m = format!($($t)*);
        crate::log_file(&format!("ERROR {m}"));
        crate::error_console(&m);
    }};
}

mod config;
mod gui;

use config::AppConfig;
use notify::{Config as NotifyConfig, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuId, MenuItem},
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};
use winreg::{enums::*, RegKey};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const REG_NAME: &str = "G-Helper Watcher";
const BUILD_ID: &str = env!("BUILD_ID");

// ---------------------------------------------------------------------------
// Silent-by-default console: no window unless something actually errors.
// ---------------------------------------------------------------------------
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::Foundation::{BOOL, ERROR_ALREADY_EXISTS, FALSE, HANDLE, HWND};
use windows::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, WinBuiltinAdministratorsSid, PSID,
};
use windows::Win32::System::Console::{
    AllocConsole, AttachConsole, GetStdHandle, WriteConsoleW, ATTACH_PARENT_PROCESS,
    STD_ERROR_HANDLE,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_F16, VK_F17, VK_F18, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, FindWindowW, GetForegroundWindow, IsIconic, IsWindowVisible,
    MsgWaitForMultipleObjects, PeekMessageW, SetForegroundWindow, ShowWindow, TranslateMessage,
    MSG, PM_REMOVE, QS_ALLINPUT, SW_HIDE, SW_NORMAL, SW_RESTORE, SW_SHOW, WM_QUIT,
};

/// 0 = no console (boot/double-click: fully silent), 1 = inherited parent
/// terminal (`cargo run`), 2 = console allocated to show an error.
static CONSOLE: AtomicU8 = AtomicU8::new(0);

fn init_console() {
    // SAFETY: attaching to the parent console only wires our std handles to it.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
            CONSOLE.store(1, Ordering::SeqCst);
        }
    }
}

/// Append-only debug log for a daemon that usually has no console.
/// Caps at ~1MB (then restarts) so it can't grow forever.
fn log_file(line: &str) {
    let path = std::env::var("APPDATA")
        .map(|a| PathBuf::from(a).join("ghelper-watcher").join("watcher.log"))
        .ok();
    let Some(path) = path else { return };
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 1024 * 1024 {
        let _ = std::fs::remove_file(&path);
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write as _;
        let _ = writeln!(f, "{:?} {line}", std::time::SystemTime::now());
    }
}

fn error_console(msg: &str) {
    if CONSOLE.load(Ordering::SeqCst) == 1 {
        eprintln!("{msg}");
        return;
    }
    // SAFETY: AllocConsole is idempotent-safe here (a racing caller just fails).
    unsafe {
        if CONSOLE.load(Ordering::SeqCst) == 0 {
            let _ = AllocConsole();
            CONSOLE.store(2, Ordering::SeqCst);
        }
    }
    write_raw(msg);
}

fn write_raw(msg: &str) {
    // SAFETY: buffer outlives the call; handle validity is checked via Result.
    unsafe {
        if let Ok(h) = GetStdHandle(STD_ERROR_HANDLE) {
            let wide: Vec<u16> = msg
                .encode_utf16()
                .chain(['\r' as u16, '\n' as u16])
                .collect();
            let mut written = 0u32;
            let _ = WriteConsoleW(h, &wide, Some(&mut written), None);
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

pub struct Shared {
    cfg: Mutex<AppConfig>,
    paused_until: Mutex<Option<Instant>>,
    initiated_until: Mutex<Option<Instant>>,
    last_mode: Mutex<Option<String>>,
    applied: Mutex<String>,
    trigger_sys: Mutex<bool>,
    wmi_ok: AtomicBool,
    dashboard_child: Mutex<Option<Child>>,
    last_switch: Mutex<Option<Instant>>,
    /// Titles hidden by the last tray toggle (unhidden on the next one).
    hidden_windows: Mutex<Vec<String>>,
    /// Cached `is_startup_enabled` (60s TTL, see `startup_enabled_ttl`).
    startup_cache: Mutex<(bool, Instant)>,
}

impl Shared {
    pub fn is_paused(&self) -> bool {
        self.remaining().is_some()
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.paused_until
            .lock()
            .unwrap()
            .and_then(|t| t.checked_duration_since(Instant::now()))
    }

    pub fn pause(&self, mins: u64) {
        *self.paused_until.lock().unwrap() =
            Some(Instant::now() + Duration::from_secs(mins.max(1) * 60));
        modify_cfg(self, |c| {
            c.paused_until_millis = Some(now_millis() + mins.max(1) as i64 * 60_000)
        });
    }

    pub fn current_applied(&self) -> String {
        self.applied.lock().unwrap().clone()
    }

    pub fn wmi_live(&self) -> bool {
        self.wmi_ok.load(Ordering::SeqCst)
    }

    /// True while our own switch is still landing in G-Helper's config.
    pub fn initiated(&self) -> bool {
        self.initiated_until
            .lock()
            .unwrap()
            .is_some_and(|t| Instant::now() < t)
    }

    pub fn mark_initiated(&self, secs: u64) {
        *self.initiated_until.lock().unwrap() =
            Some(Instant::now() + Duration::from_secs(secs.max(1)));
    }
}

pub fn cfg_snapshot(st: &Shared) -> AppConfig {
    st.cfg.lock().unwrap().clone()
}

/// Mutate config and persist. Save errors are intentionally ignored
/// (daemon keeps running with in-memory state).
pub fn modify_cfg(st: &Shared, f: impl FnOnce(&mut AppConfig)) {
    let mut cfg = st.cfg.lock().unwrap();
    f(&mut cfg);
    let _ = cfg.save();
}

pub fn resume_now(st: &Arc<Shared>) {
    *st.paused_until.lock().unwrap() = None;
    *st.last_switch.lock().unwrap() = None; // explicit resume bypasses cooldown
    modify_cfg(st, |c| {
        c.paused_until_millis = None;
        c.last_switch_millis = None;
    });
    evaluate(st);
}

/// Remaining cooldown before the next auto-switch is allowed.
/// Reads the persisted switch time so the dashboard and daemon agree.
pub fn cooldown_remaining(cfg: &AppConfig) -> Option<Duration> {
    let last = cfg.last_switch_millis?;
    let secs = cfg.switch_cooldown_secs;
    if secs == 0 {
        return None;
    }
    let now = now_millis();
    let ready_at = last.saturating_add(secs as i64 * 1000);
    (ready_at > now).then(|| Duration::from_millis((ready_at - now) as u64))
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Remaining pause from the shared config file (daemon and dashboard agree).
pub fn pause_remaining(cfg: &AppConfig) -> Option<Duration> {
    let until = cfg.paused_until_millis?;
    let now = now_millis();
    (until > now).then(|| Duration::from_millis((until - now) as u64))
}

// ---------------------------------------------------------------------------
// G-Helper control
// ---------------------------------------------------------------------------

/// Refresh kinds for every sysinfo scan here: exe (+cmd for the stray
/// killer) is all anyone reads. `everything()` also walks memory/disk/cwd
/// per process and was the hottest idle cost.
fn proc_kinds() -> ProcessRefreshKind {
    ProcessRefreshKind::new()
        .with_exe(sysinfo::UpdateKind::Always)
        .with_cmd(sysinfo::UpdateKind::Always)
}

/// Run `f` against a thread-local reused `System` instead of building a
/// fresh `System::new()` per scan (each build re-enumerates everything).
fn with_system<R>(f: impl FnOnce(&mut System) -> R) -> R {
    thread_local! {
        static SYS: std::cell::RefCell<System> = std::cell::RefCell::new(System::new());
    }
    SYS.with(|sys| {
        let mut sys = sys.borrow_mut();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, proc_kinds());
        f(&mut sys)
    })
}

/// Lowercase exe file names of all running processes (sysinfo view).
fn sysinfo_names() -> Vec<String> {
    with_system(|sys| {
        sys.processes()
            .values()
            .map(|p| {
                p.exe()
                    .and_then(|x| x.file_name())
                    .map(|s| s.to_string_lossy().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| p.name().to_string_lossy().to_lowercase())
            })
            .collect()
    })
}

/// Extra names via WMI (sees elevated processes sysinfo cannot).
/// Empty on failure — callers always union with sysinfo.
fn wmi_process_names() -> Vec<String> {
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct ProcName {
        name: Option<String>,
    }
    let rows: Vec<ProcName> = (|| {
        let com = wmi::COMLibrary::new().ok()?;
        let conn = wmi::WMIConnection::new(com).ok()?;
        conn.raw_query::<ProcName>("SELECT Name FROM Win32_Process")
            .ok()
    })()
    .unwrap_or_default();
    rows.into_iter()
        .filter_map(|r| r.name)
        .map(|n| n.to_lowercase())
        .filter(|n| !n.is_empty())
        .collect()
}

/// Union of both views, deduplicated. Used by the picker, the G-Helper
/// presence check, and the slow evaluation path.
pub fn running_exe_names() -> Vec<String> {
    let mut all = sysinfo_names();
    all.extend(wmi_process_names());
    all.sort();
    all.dedup();
    all
}

/// G-Helper mode index (Modes.GetName: 0=Balanced, 1=Turbo, 2=Silent).
fn profile_index(profile: &str) -> Option<i32> {
    match profile.to_ascii_lowercase().as_str() {
        "silent" => Some(2),
        "balanced" => Some(0),
        "turbo" => Some(1),
        _ => None,
    }
}

fn index_name(i: i64) -> String {
    match i {
        2 => "Silent".to_string(),
        0 => "Balanced".to_string(),
        1 => "Turbo".to_string(),
        _ => format!("Custom {i}"),
    }
}

fn apply_profile(st: &Arc<Shared>, profile: &str) {
    // Switch cooldown: absorb launch flapping (games that start/stop
    // several times while opening). Ticks re-evaluate after it lapses.
    let cooldown = Duration::from_secs(cfg_snapshot(st).switch_cooldown_secs);
    if !cooldown.is_zero() {
        if let Some(last) = *st.last_switch.lock().unwrap() {
            if last.elapsed() < cooldown {
                return;
            }
        }
    }
    let Some(idx) = profile_index(profile) else {
        log_error!("unknown profile '{profile}'; want Silent, Balanced or Turbo");
        return;
    };
    if !ghelper_running() {
        log_error!("G-Helper is not running; cannot switch to {profile}");
        return;
    }
    // Cover G-Helper's own config rewrites (fan/power apply writes for
    // several seconds) so the watcher ignores our switch.
    st.mark_initiated(8);
    if press_mode_hotkey(idx) {
        *st.applied.lock().unwrap() = profile.to_string();
        *st.last_switch.lock().unwrap() = Some(Instant::now());
        let now = now_millis();
        modify_cfg(st, |c| c.last_switch_millis = Some(now));
    } else {
        log_error!("failed to send {profile} hotkey to G-Helper");
    }
}

pub fn ghelper_running() -> bool {
    running_exe_names().iter().any(|n| n == "ghelper.exe")
}

/// Press G-Helper's global mode hotkey (default Ctrl+Shift+Alt+F16/F17/F18,
/// honoring user remaps). Background-only: no window, just its small toast.
fn press_mode_hotkey(idx: i32) -> bool {
    let (mods, key) = mode_hotkey(idx);
    let mut seq = Vec::with_capacity(2 * mods.len() + 2);
    for m in &mods {
        seq.push(key_event(*m, false));
    }
    seq.push(key_event(key, false));
    seq.push(key_event(key, true));
    for m in mods.iter().rev() {
        seq.push(key_event(*m, true));
    }
    // SAFETY: plain SendInput of key up/down pairs; buffer outlives the call.
    unsafe { SendInput(&seq, size_of::<INPUT>() as i32) == seq.len() as u32 }
}

fn key_event(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Hotkey for mode index, honoring G-Helper remaps
/// (`keybind_profile_N` + `modifier_keybind_alt` in its config).
fn mode_hotkey(idx: i32) -> (Vec<VIRTUAL_KEY>, VIRTUAL_KEY) {
    let cfg = ghelper_cfg_json();
    let defaults = [VK_F17, VK_F18, VK_F16]; // slots 0,1,2
    let key = cfg
        .as_ref()
        .and_then(|v| {
            v.get(format!("keybind_profile_{idx}"))
                .and_then(|k| k.as_i64())
        })
        .map(|k| VIRTUAL_KEY(k as u16))
        .unwrap_or(defaults[idx as usize]);
    let mods = cfg
        .as_ref()
        .and_then(|v| v.get("modifier_keybind_alt").and_then(|m| m.as_str()))
        .map(parse_modifiers)
        .unwrap_or_else(default_modifiers);
    (mods, key)
}

fn default_modifiers() -> Vec<VIRTUAL_KEY> {
    vec![VK_LCONTROL, VK_LSHIFT, VK_LMENU]
}

fn parse_modifiers(s: &str) -> Vec<VIRTUAL_KEY> {
    let mut v = Vec::new();
    for part in s.split('-') {
        match part.trim().to_ascii_lowercase().as_str() {
            "control" | "ctrl" => v.push(VK_LCONTROL),
            "shift" => v.push(VK_LSHIFT),
            "alt" => v.push(VK_LMENU),
            "win" | "windows" => v.push(VK_LWIN),
            _ => {}
        }
    }
    if v.is_empty() {
        v = default_modifiers();
    }
    v
}

/// Target profile + watched running count + whether the hit is sysinfo-visible.
fn desired_profile(st: &Arc<Shared>) -> (String, usize, bool) {
    let cfg = cfg_snapshot(st);
    let sys = sysinfo_names();
    let mut merged = sys.clone();
    merged.extend(wmi_process_names());
    merged.sort();
    merged.dedup();
    let hit = |names: &[String]| {
        cfg.watched
            .iter()
            .any(|w| names.iter().any(|n| n.eq_ignore_ascii_case(w)))
    };
    let count = merged
        .iter()
        .filter(|n| cfg.watched.iter().any(|w| w.eq_ignore_ascii_case(n)))
        .count();
    let target = if hit(&merged) {
        cfg.trigger_profile
    } else {
        cfg.default_profile
    };
    (target, count, hit(&sys))
}

/// Re-evaluate with the full (sysinfo + WMI) picture. No-op while paused.
pub fn evaluate(st: &Arc<Shared>) {
    if st.is_paused() {
        return;
    }
    let (target, _count, hit_sys) = desired_profile(st);
    if st.current_applied() != target {
        apply_profile(st, &target);
        *st.trigger_sys.lock().unwrap() = hit_sys;
    }
}

/// Fast sysinfo-only tick (2s): instant reaction for ordinary (unelevated)
/// processes. Never switches DOWN unless the trigger came from a sysinfo
/// sighting — elevated-only apps are invisible here and owned by slow_tick.
fn fast_tick(st: &Arc<Shared>) {
    if st.is_paused() {
        return;
    }
    let cfg = cfg_snapshot(st);
    let sys = sysinfo_names();
    let hit = cfg
        .watched
        .iter()
        .any(|w| sys.iter().any(|n| n.eq_ignore_ascii_case(w)));
    let applied = st.current_applied();
    if hit && applied != cfg.trigger_profile {
        apply_profile(st, &cfg.trigger_profile);
        *st.trigger_sys.lock().unwrap() = true;
    } else if !hit && applied == cfg.trigger_profile && *st.trigger_sys.lock().unwrap() {
        apply_profile(st, &cfg.default_profile);
    }
}

/// Background cadence: 2s sysinfo tick + 30s full (WMI-merged) tick.
/// The slow tick is only a safety net (fast tick + WMI events carry the
/// load), so it runs half as often to halve its ~10% query spikes.
fn poll_thread(st: Arc<Shared>) {
    std::thread::spawn(move || {
        let mut slow = Instant::now() - Duration::from_secs(99);
        loop {
            std::thread::sleep(Duration::from_secs(2));
            fast_tick(&st);
            if slow.elapsed() > Duration::from_secs(30) {
                slow = Instant::now();
                evaluate(&st);
            }
        }
    });
}

/// Best-effort read of the current G-Helper mode from its own config file
/// (`performance_mode` int: 0=Balanced, 1=Turbo, 2=Silent).
pub fn ghelper_current_mode() -> String {
    read_ghelper_mode().unwrap_or_else(|| "unknown".to_string())
}

fn read_ghelper_mode() -> Option<String> {
    let v = ghelper_cfg_json()?;
    let m = v.get("performance_mode")?;
    if let Some(i) = m.as_i64() {
        return Some(index_name(i));
    }
    if let Some(s) = m.as_str() {
        for name in ["Silent", "Balanced", "Turbo"] {
            if s.eq_ignore_ascii_case(name) {
                return Some(name.to_string());
            }
        }
        return Some(s.to_string());
    }
    None
}

fn ghelper_cfg_json() -> Option<serde_json::Value> {
    let data = std::fs::read_to_string(ghelper_cfg_path()?).ok()?;
    serde_json::from_str(&data).ok()
}

fn ghelper_cfg_path() -> Option<PathBuf> {
    std::env::var("APPDATA")
        .ok()
        .map(|a| PathBuf::from(a).join("GHelper").join("config.json"))
}

// ---------------------------------------------------------------------------
// Zero-CPU process monitoring (WMI event subscriptions, one thread each)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ProcTrace {
    process_name: String,
    #[allow(dead_code)]
    process_id: u32,
}

// ponytail: WMI event classes need elevation; if setup fails we degrade to a
// 10s sysinfo poll instead of exiting. Remove the fallback if admin is guaranteed.
fn wmi_thread(st: Arc<Shared>, class: &'static str) {
    let setup: wmi::WMIResult<()> = (|| {
        // COMLibrary/WMIConnection are !Send: create AND block on this thread.
        let com = wmi::COMLibrary::new()?;
        let conn = wmi::WMIConnection::new(com)?;
        let iter = conn.raw_notification::<ProcTrace>(format!("SELECT * FROM {class}"))?;
        st.wmi_ok.store(true, Ordering::SeqCst);
        modify_cfg(&st, |c| c.event_source = Some("wmi".to_string()));
        for event in iter {
            let Ok(e) = event else { continue };
            let name = e.process_name.to_lowercase();
            if cfg_snapshot(&st)
                .watched
                .iter()
                .any(|w| w.to_lowercase() == name)
            {
                evaluate(&st);
            }
        }
        Ok(())
    })();
    if let Err(e) = setup {
        log_info!("WMI {class} unavailable ({e:?}); poll thread covers it");
    }
}

// ---------------------------------------------------------------------------
// Manual override protection (watch G-Helper's own config file)
// ---------------------------------------------------------------------------

fn notify_thread(st: Arc<Shared>) {
    let Some(target) = ghelper_cfg_path() else {
        return;
    };
    let Some(parent) = target.parent().map(|p| p.to_path_buf()) else {
        return;
    };
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match RecommendedWatcher::new(tx, NotifyConfig::default()) {
            Ok(w) => w,
            Err(e) => {
                log_error!("file watcher init failed: {e:?}");
                return;
            }
        };
        if watcher.watch(&parent, RecursiveMode::NonRecursive).is_err() {
            return;
        }
        // `watcher` lives for the thread lifetime; dropping it stops events.
        for res in rx {
            let Ok(ev) = res else { continue };
            if !matches!(ev.kind, EventKind::Modify(_)) {
                continue;
            }
            if !ev.paths.iter().any(|p| p == &target) {
                continue;
            }
            // Content-aware: fan curves, window prefs etc. rewrite this file
            // constantly — only an actual MODE change counts as manual override.
            let current = read_ghelper_mode();
            let mut last = st.last_mode.lock().unwrap();
            match (&current, &*last) {
                (Some(cur), Some(prev)) if cur == prev => {}
                (Some(cur), _) => {
                    let cur = cur.clone();
                    *last = Some(cur.clone());
                    drop(last);
                    if st.initiated() {
                        continue; // our own hotkey switch landing
                    }
                    let mins = cfg_snapshot(&st).pause_minutes;
                    st.pause(mins);
                    log_info!("manual G-Helper mode change to {cur}; auto-switch paused {mins}m");
                }
                (None, _) => {}
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Own-config sync (dashboard child edits the same file)
// ---------------------------------------------------------------------------

/// Adopt on-disk state (watch list, profiles, pause) then re-evaluate.
/// Idempotent: our own saves round-trip harmlessly.
fn reload_cfg(st: &Arc<Shared>) {
    let cfg = AppConfig::load();
    *st.cfg.lock().unwrap() = cfg.clone();
    *st.paused_until.lock().unwrap() = pause_remaining(&cfg).map(|d| Instant::now() + d);
    // Explicit resume from the dashboard clears the persisted switch time;
    // mirror that into memory so resume bypasses the cooldown (like the
    // old tray Resume did). A fresh switch time written by apply_profile
    // never reads back into memory — memory stays the source of truth.
    if cfg.last_switch_millis.is_none() {
        *st.last_switch.lock().unwrap() = None;
    }
    if cfg.restart_request {
        modify_cfg(st, |c| c.restart_request = false);
        restart_app(st);
        return;
    }
    converge_admin(st);
    evaluate(st);
    log_file("config reloaded");
}

fn own_cfg_thread(st: Arc<Shared>) {
    let path = match AppConfig::path() {
        Ok(p) => p,
        Err(_) => return,
    };
    let Some(parent) = path.parent().map(|p| p.to_path_buf()) else {
        return;
    };
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match RecommendedWatcher::new(tx, NotifyConfig::default()) {
            Ok(w) => w,
            Err(e) => {
                log_error!("own config watcher failed: {e:?}");
                return;
            }
        };
        if watcher.watch(&parent, RecursiveMode::NonRecursive).is_err() {
            return;
        }
        // Borrowed iteration: the debounce drain below needs `rx` too.
        for res in &rx {
            let Ok(ev) = res else { continue };
            if !matches!(ev.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                continue;
            }
            if !ev.paths.iter().any(|p| p == &path) {
                continue;
            }
            // Debounce: one save fires several Modify events on Windows;
            // coalesce 200ms of them into a single reload.
            while rx.recv_timeout(Duration::from_millis(200)).is_ok() {}
            reload_cfg(&st);
        }
    });
}

// ---------------------------------------------------------------------------
// Startup at boot (HKCU ...\Run)
// ---------------------------------------------------------------------------

fn exe_cmdline() -> String {
    match std::env::current_exe() {
        Ok(p) => format!("\"{}\" --minimized", p.to_string_lossy()),
        Err(_) => "\"ghelper-watcher.exe\" --minimized".to_string(),
    }
}

fn is_autostart() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .and_then(|k| k.get_value::<String, _>(REG_NAME))
        .is_ok()
}

/// Effective autostart (Run key or elevated logon task).
pub fn is_startup_enabled() -> bool {
    is_autostart() || admin_task_exists()
}

/// TTL-cached variant for hot paths (converge runs per config reload).
/// Refreshes at most every 60s; storage writes below are idempotent
/// (`/F` overwrite, tolerant delete), so slight staleness is harmless.
fn startup_enabled_ttl(st: &Arc<Shared>) -> bool {
    let now = Instant::now();
    let mut cache = st.startup_cache.lock().unwrap();
    if now.duration_since(cache.1) > Duration::from_secs(60) {
        *cache = (is_startup_enabled(), now);
    }
    cache.0
}

/// Set autostart, honoring the run-as-admin preference.
pub fn set_startup(on: bool) -> std::io::Result<()> {
    let admin = AppConfig::load().run_as_admin;
    if on {
        if admin {
            if !admin_task_exists() {
                set_admin_task(true)?;
            }
            remove_run_key();
        } else {
            set_autostart(true)?;
        }
    } else {
        remove_run_key();
        if admin_task_exists() {
            set_admin_task(false)?;
        }
    }
    Ok(())
}

pub fn set_autostart(on: bool) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if on {
        let (key, _) = hkcu.create_subkey(RUN_KEY)?;
        let cmd = exe_cmdline();
        key.set_value(REG_NAME, &cmd)
    } else {
        remove_run_key();
        Ok(())
    }
}

fn delete_run_value(key: &RegKey, name: &str) {
    match key.delete_value(name) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log_file(&format!("run key delete {name} failed: {e:?}")),
    }
}

// ---------------------------------------------------------------------------
// Tray (always-on) + lazy GUI (spawned on demand)
// ---------------------------------------------------------------------------

/// G glyph, painted natively at any square size (cell = size/8, so every
/// downscale by 2 stays crisp). Mirrored by build.rs for the .ico.
fn g_icon_rgba(size: u32) -> Vec<u8> {
    let size = size as usize;
    let cell = size / 8;
    let ox = (size - 5 * cell) / 2;
    let oy = (size - 7 * cell) / 2;
    let mut px = vec![0u8; size * size * 4];
    let paint = |px: &mut [u8], x: usize, y: usize, c: [u8; 4]| {
        let i = (y * size + x) * 4;
        px[i..i + 4].copy_from_slice(&c);
    };
    for y in 0..size {
        for x in 0..size {
            paint(&mut px, x, y, [15, 23, 42, 255]); // slate-900
        }
    }
    let glyph = [
        ".XXX.", "X...X", "X....", "X.XXX", "X...X", "X...X", ".XXX.",
    ];
    for (gy, row) in glyph.iter().enumerate() {
        for (gx, ch) in row.chars().enumerate() {
            if ch == 'X' {
                for dy in 0..cell {
                    for dx in 0..cell {
                        paint(
                            &mut px,
                            ox + gx * cell + dx,
                            oy + gy * cell + dy,
                            [34, 197, 94, 255],
                        );
                    }
                }
            }
        }
    }
    px
}

// ---------------------------------------------------------------------------
// Tray (always-on). One process, one event loop: eframe owns the main thread
// (window hidden), tray menu events arrive via channel + repaint.
// ---------------------------------------------------------------------------

/// Right-click menu is deliberately tiny: Restart-elevated (standard only)
/// plus Quit. Everything else (pause, autostart, admin) lives in the GUI.
pub struct TrayBundle {
    pub tray: TrayIcon,
    pub restart: MenuItem,
    pub exit: MenuItem,
    pub restart_id: MenuId,
    pub exit_id: MenuId,
}

fn build_tray() -> TrayBundle {
    let menu = Menu::new();
    let exit_id = MenuId::new("exit");
    let restart_id = MenuId::new("restart");
    let restart = MenuItem::with_id(restart_id.clone(), "Restart elevated", true, None);
    let exit = MenuItem::with_id(exit_id.clone(), "Quit", true, None);
    if !is_elevated() {
        // Already admin: nothing to escalate to, so no item.
        menu.append(&restart).unwrap();
    }
    menu.append(&exit).unwrap();
    let icon = Icon::from_rgba(g_icon_rgba(32), 32, 32).expect("tray icon");
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("G-Helper Watcher")
        .with_icon(icon)
        .build()
        .expect("tray build");
    TrayBundle {
        tray,
        restart,
        exit,
        restart_id,
        exit_id,
    }
}

/// Bring one window to the front by exact title. No-op when absent.
fn focus_window(title: &str) {
    // SAFETY: read-only window lookup + focus request.
    unsafe {
        let title = HSTRING::from(title);
        let Ok(hwnd) = FindWindowW(PCWSTR::null(), &title) else {
            return;
        };
        if hwnd.is_invalid() {
            return;
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Foreground window right now, if any.
fn foreground_window() -> Option<HWND> {
    // SAFETY: plain query, no side effects.
    unsafe {
        let hwnd = GetForegroundWindow();
        (!hwnd.is_invalid()).then_some(hwnd)
    }
}

/// Bring all open dashboard windows forward (used on unhide/refocus):
/// show both first (a plain foreground call can leave the second one
/// behind other apps), then foreground Manage + main so main keeps focus.
/// Missing windows are skipped, so the single-window case is unchanged.
fn focus_dashboard() {
    if let Some(h) = find_window(MANAGE_TITLE) {
        show_window(h);
    }
    if let Some(h) = find_window(MAIN_TITLE) {
        show_window(h);
    }
    focus_window(MANAGE_TITLE);
    focus_window(MAIN_TITLE);
}

/// Kill any dashboard child processes, including orphans from older
/// generations that this instance never tracked (e.g. after a taskkill).
fn kill_stray_dashboards() {
    let me = std::process::id();
    with_system(|sys| {
        for (pid, p) in sys.processes() {
        if pid.as_u32() == me {
            continue;
        }
        let exe = p
            .exe()
            .and_then(|x| x.file_name())
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if exe != "ghelper-watcher.exe" {
            continue;
        }
        if p.cmd().iter().any(|a| a.to_string_lossy() == "--dashboard") {
            log_file(&format!("stray dashboard {} killed", pid.as_u32()));
            let _ = p.kill();
        }
        }
    });
}

/// Position above the tray icon, on the icon's own monitor (DPI aware).
/// X is centered above the tray icon, Y is docked to that monitor's work
/// area bottom (just above the taskbar, like G-Helper). Falls back to
/// primary work-area math when the icon rect is unavailable.
fn dashboard_pos_near_tray(tray: &TrayBundle) -> Option<(f32, f32)> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    let r = tray.tray.rect()?;
    let (ix, iw) = (r.position.x as f32, r.size.width as f32);
    // SAFETY: pure queries into locals.
    unsafe {
        let pt = windows::Win32::Foundation::POINT {
            x: ix as i32,
            y: r.position.y as i32,
        };
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut dx = 0u32;
        let mut dy = 0u32;
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_err() || dx == 0 {
            return None;
        }
        let s = dx as f32 / 96.0;
        // That monitor's work area (excludes taskbar), in physical pixels.
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(mon, &mut info).as_bool() {
            let l = info.rcWork.left as f32 / s;
            let rr = info.rcWork.right as f32 / s;
            let bb = info.rcWork.bottom as f32 / s;
            // Center above the tray icon, clamped inside the work area.
            let mut x = (ix + iw / 2.0) / s - DASH_W / 2.0;
            x = x.clamp(l + 8.0, (rr - DASH_W - 8.0).max(l + 8.0));
            let y = bb - DASH_H - 12.0;
            return Some((x, y));
        }
        // Fallback: primary work-area bottom-right.
        dashboard_bottom_right(DASH_H).map(|p| (p.x, p.y))
    }
}

const MAIN_TITLE: &str = "G-Helper Watcher";
const MANAGE_TITLE: &str = "Manage - G-Helper Watcher";

/// Visible top-level window by exact title (minimized counts as visible).
fn find_visible(title: &str) -> Option<HWND> {
    // SAFETY: read-only lookup + state query.
    unsafe {
        let t = HSTRING::from(title);
        let Ok(hwnd) = FindWindowW(PCWSTR::null(), &t) else {
            return None;
        };
        if hwnd.is_invalid() {
            return None;
        }
        IsWindowVisible(hwnd).as_bool().then_some(hwnd)
    }
}

/// True when our own main window exists but is hidden (tray toggle).
/// Minimized still counts as visible. Lets the hidden dashboard sleep
/// instead of repainting countdowns nobody can see.
pub(crate) fn main_hidden() -> bool {
    find_window(MAIN_TITLE).is_some() && find_visible(MAIN_TITLE).is_none()
}

/// Any top-level window by exact title, visible or hidden.
fn find_window(title: &str) -> Option<HWND> {
    // SAFETY: read-only lookup.
    unsafe {
        let t = HSTRING::from(title);
        let Ok(hwnd) = FindWindowW(PCWSTR::null(), &t) else {
            return None;
        };
        (!hwnd.is_invalid()).then_some(hwnd)
    }
}

/// Show a looked-up window in whatever state fits (restore if minimized).
fn show_window(hwnd: HWND) {
    // SAFETY: show request on a looked-up handle.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
    }
}

/// Tray-click toggle, G-Helper style:
/// - nothing open -> open the main window only;
/// - open but behind other apps -> bring all ours forward;
/// - ours in front -> hide them (remembered for the next click).
/// At most one dashboard process ever: strays die on spawn.
fn toggle_dashboard(st: &Arc<Shared>, tray: &TrayBundle) {
    let main = find_visible(MAIN_TITLE);
    let manage = find_visible(MANAGE_TITLE);
    if main.is_some() || manage.is_some() {
        let fg = foreground_window();
        if fg != main && fg != manage {
            // Open but unfocused: bring everything forward.
            focus_dashboard();
            return;
        }
        // Ours in front: hide everything visible, remember the set.
        let mut hidden = st.hidden_windows.lock().unwrap();
        hidden.clear();
        // SAFETY: hiding looked-up handles.
        unsafe {
            for (hdl, name) in [(main, MAIN_TITLE), (manage, MANAGE_TITLE)] {
                if let Some(h) = hdl {
                    let _ = ShowWindow(h, SW_HIDE);
                    hidden.push(name.to_string());
                }
            }
        }
        log_file("dashboard hidden by tray toggle");
        return;
    }
    // Nothing visible: unhide the remembered set if it still exists.
    let hidden: Vec<String> = st.hidden_windows.lock().unwrap().clone();
    st.hidden_windows.lock().unwrap().clear();
    let mut restored = false;
    for name in &hidden {
        if let Some(h) = find_window(name) {
            show_window(h);
            restored = true;
        }
    }
    if restored {
        focus_dashboard();
        return;
    }
    // No windows at all: drop any windowless zombie, kill strays, spawn.
    // This is the single place dashboard processes are born, so duplicates
    // (e.g. after a rebuild while old windows linger) cannot accumulate.
    let mut slot = st.dashboard_child.lock().unwrap();
    if let Some(child) = slot.as_mut() {
        if let Ok(None) = child.try_wait() {
            let _ = child.kill();
        }
        *slot = None;
    }
    drop(slot);
    kill_stray_dashboards();
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ghelper-watcher.exe"));
    let mut cmd = Command::new(exe);
    cmd.arg("--dashboard");
    if let Some((x, y)) = dashboard_pos_near_tray(tray) {
        cmd.arg(format!("--pos={x:.0},{y:.0}"));
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    cmd.creation_flags(0x00000008 | 0x08000000); // DETACHED_PROCESS | CREATE_NO_WINDOW
    match cmd.spawn() {
        Ok(child) => {
            *st.dashboard_child.lock().unwrap() = Some(child);
            log_file("dashboard child spawned");
        }
        Err(e) => log_error!("failed to open dashboard: {e:?}"),
    }
}

const TASK_NAME: &str = "G-Helper Watcher";

/// True if the daemon holds the single-instance mutex right now.
/// The probe handle is closed immediately so it never blocks a restart.
pub fn daemon_alive() -> bool {
    // SAFETY: short-lived probe handle, always closed below.
    unsafe {
        let name = HSTRING::from("ghelper-watcher-single-instance");
        match CreateMutexW(None, FALSE, &name) {
            Ok(h) => {
                let alive = GetLastError() == ERROR_ALREADY_EXISTS;
                let _ = CloseHandle(h);
                alive
            }
            Err(_) => false,
        }
    }
}

/// Second daemon instance exits (first one owns the tray).
/// Retries briefly first: an escalating twin often starts while the old
/// instance is still tearing down, and must survive that, not die on it.
/// Kills ghost-icon confusion at the source; hover the tray to clear dead icons.
fn ensure_single_instance() {
    // SAFETY: named mutex; on success the handle is intentionally never
    // closed (HANDLE is Copy with no Drop) so it is held for process lifetime.
    unsafe {
        let name = HSTRING::from("ghelper-watcher-single-instance");
        for _ in 0..12 {
            match CreateMutexW(None, FALSE, &name) {
                Ok(h) => {
                    if GetLastError() == ERROR_ALREADY_EXISTS {
                        let _ = CloseHandle(h);
                        std::thread::sleep(Duration::from_millis(300));
                        continue;
                    }
                    let _ = h;
                    return;
                }
                Err(e) => {
                    log_file(&format!("instance mutex failed (continuing): {e:?}"));
                    return;
                }
            }
        }
        log_file("second instance detected, exiting");
        std::process::exit(0);
    }
}

pub fn is_elevated() -> bool {
    // SAFETY: plain SID query, no side effects.
    unsafe {
        let mut buf = vec![0u8; 68];
        let mut len = buf.len() as u32;
        let ok = CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            PSID::default(),
            PSID(buf.as_mut_ptr() as *mut _),
            &mut len,
        )
        .is_ok();
        if !ok {
            return false;
        }
        let mut member = BOOL(0);
        if CheckTokenMembership(HANDLE::default(), PSID(buf.as_ptr() as *mut _), &mut member)
            .is_err()
        {
            return false;
        }
        member.0 != 0
    }
}

fn spawn_elevated() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let file = HSTRING::from(exe.to_string_lossy().into_owned());
    let params = HSTRING::from("--minimized".to_string());
    // SAFETY: plain ShellExecute, no callbacks.
    unsafe {
        let r = ShellExecuteW(
            HWND::default(),
            windows::core::w!("runas"),
            &file,
            &params,
            PCWSTR::null(),
            SW_NORMAL,
        );
        if (r.0 as isize) <= 32 {
            return Err(std::io::Error::other(format!(
                "ShellExecute runas failed ({})",
                r.0 as isize
            )));
        }
    }
    Ok(())
}

fn spawn_unelevated() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    Command::new(exe)
        .arg("--minimized")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x00000008 | 0x08000000) // DETACHED_PROCESS | CREATE_NO_WINDOW
        .spawn()?;
    Ok(())
}

fn admin_task_exists() -> bool {
    schtasks(&["/Query", "/TN", TASK_NAME])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// schtasks without flashing a console window.
fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("schtasks")
        .args(args)
        .stdin(std::process::Stdio::null())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .output()
}

fn set_admin_task(on: bool) -> std::io::Result<()> {
    let err = |out: std::process::Output| {
        std::io::Error::other(String::from_utf8_lossy(&out.stderr).into_owned())
    };
    if on {
        let exe = std::env::current_exe()?;
        let task_run = format!("\"{}\" --minimized", exe.to_string_lossy());
        let out = schtasks(&[
            "/Create", "/TN", TASK_NAME, "/TR", &task_run, "/SC", "ONLOGON", "/RL", "HIGHEST", "/F",
        ])?;
        if !out.status.success() {
            return Err(err(out));
        }
    } else if admin_task_exists() {
        let out = schtasks(&["/Delete", "/TN", TASK_NAME, "/F"])?;
        if !out.status.success() {
            return Err(err(out));
        }
    }
    Ok(())
}

fn remove_run_key() {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(key) = hkcu.open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE) {
        delete_run_value(&key, REG_NAME);
    }
}

/// Converge process elevation + autostart storage to the sticky preference.
/// UAC prompts happen ONLY on toggle gestures (rising edge): never on
/// startup or reload, so an approved setup never asks again.
/// Explicit restart (dashboard button / tray menu). The ONLY path that
/// escalates: with the preference on while standard, this UAC-prompts —
/// you pressed the button, so a prompt is expected, not a surprise.
fn restart_app(st: &Arc<Shared>) {
    let want = cfg_snapshot(st).run_as_admin;
    if want && !is_elevated() {
        match spawn_elevated() {
            Ok(()) => {
                log_file("restarting elevated");
                std::process::exit(0);
            }
            Err(e) => log_error!("elevated restart failed: {e:?}"),
        }
        return;
    }
    // Same-level restart (preserves elevation either way, no prompt).
    match spawn_unelevated() {
        Ok(()) => {
            log_file("restarting");
            std::process::exit(0);
        }
        Err(e) => log_error!("restart failed: {e:?}"),
    }
}

/// Sync admin storage to the sticky preference. Never prompts, never
/// relaunches: elevation changes happen only via explicit Restart
/// (button/menu) or boot. Called at startup and on config reload.
fn converge_admin(st: &Arc<Shared>) {
    let want = cfg_snapshot(st).run_as_admin;
    let have = is_elevated();
    if want && have {
        // Approved setup with autostart on: silent elevation via logon task
        // (no UAC ever again). Without autostart there is nothing to persist
        // to — a manual launch simply runs at whatever level it got.
        // TTL-cached: schtasks spawns a process per query and converge runs
        // on every config reload (including each auto-switch save).
        if startup_enabled_ttl(st) {
            if !admin_task_exists() {
                if let Err(e) = set_admin_task(true) {
                    log_error!("admin task setup failed: {e:?}");
                }
            }
            remove_run_key();
        }
    } else if !want && have {
        let _ = set_admin_task(false);
        let _ = set_autostart(true); // preserve autostart at standard level
    }
}

pub(crate) const DASH_W: f32 = 350.0;
const DASH_H: f32 = 600.0;

/// Work area in physical pixels (excludes the taskbar).
pub(crate) fn work_area_phys() -> Option<(i32, i32, i32, i32)> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS;
    use windows::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};
    // SAFETY: plain system query into a local.
    unsafe {
        let mut rect = RECT::default();
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rect as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
        Some((rect.left, rect.top, rect.right, rect.bottom))
    }
}

pub(crate) fn display_scale() -> f32 {
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForSystem, MDT_EFFECTIVE_DPI};
    // SAFETY: pure queries into locals.
    unsafe {
        let pt = windows::Win32::Foundation::POINT { x: 0, y: 0 };
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY);
        let mut dx = 0u32;
        let mut dy = 0u32;
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_ok() && dx != 0 {
            return dx as f32 / 96.0;
        }
        let dpi = GetDpiForSystem();
        if dpi == 0 {
            1.0
        } else {
            dpi as f32 / 96.0
        }
    }
}

/// Max dashboard height in points (work area minus dock margins).
pub(crate) fn max_dash_height() -> f32 {
    let s = display_scale();
    work_area_phys()
        .map(|(_, t, _, b)| (b - t) as f32 / s - 24.0)
        .unwrap_or(900.0)
}

/// Bottom-right of the work area (above the taskbar), DPI aware.
/// Height is a parameter so the window stays docked after fit_window
/// shrinks/grows it — top-left anchoring would otherwise leave it floating.
pub(crate) fn dashboard_bottom_right(height: f32) -> Option<eframe::egui::Pos2> {
    let (_, _, r, b) = work_area_phys()?;
    let s = display_scale();
    Some(eframe::egui::Pos2::new(
        r as f32 / s - DASH_W - 12.0,
        b as f32 / s - height - 12.0,
    ))
}

/// Bottom-right of the work area (above the taskbar), DPI aware.
fn dashboard_position() -> Option<eframe::egui::Pos2> {
    dashboard_bottom_right(DASH_H)
}

/// Dashboard child process: owns the ONE eframe event loop, born visible.
fn run_dashboard() -> anyhow::Result<()> {
    // Position from the daemon (--pos, above the tray icon on its monitor),
    // else the work-area fallback.
    let mut pos = dashboard_position();
    for a in std::env::args() {
        if let Some(xy) = a.strip_prefix("--pos=") {
            let mut it = xy.split(',');
            if let (Some(x), Some(y)) = (it.next(), it.next()) {
                if let (Ok(x), Ok(y)) = (x.parse::<f32>(), y.parse::<f32>()) {
                    pos = Some(eframe::egui::Pos2::new(x, y));
                }
            }
        }
    }
    let mut vp = eframe::egui::ViewportBuilder::default()
        .with_inner_size([DASH_W, DASH_H])
        .with_min_inner_size([300.0, 400.0])
        .with_maximize_button(false)
        .with_minimize_button(false)
        .with_resizable(true)
        .with_icon(eframe::egui::IconData {
            rgba: g_icon_rgba(64),
            width: 64,
            height: 64,
        });
    if let Some(pos) = pos {
        vp = vp.with_position(pos);
    }
    let opts = eframe::NativeOptions {
        viewport: vp,
        ..Default::default()
    };
    match eframe::run_native(
        MAIN_TITLE,
        opts,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(gui::dashboard_visuals());
            Ok(Box::new(gui::DashboardApp::new()))
        }),
    ) {
        Ok(()) => Ok(()),
        Err(e) => {
            log_error!("dashboard failed to open: {e:?}");
            Ok(())
        }
    }
}

fn main() -> anyhow::Result<()> {
    init_console();
    std::panic::set_hook(Box::new(|info| {
        log_file(&format!("PANIC: {info}"));
    }));
    if std::env::args().any(|a| a == "--dashboard") {
        return run_dashboard();
    }
    ensure_single_instance();
    log_file(&format!("daemon starting, elevated={}", is_elevated()));
    // Accepted for Run-key compat; app is ALWAYS tray-first.
    let _ = std::env::args().any(|a| a == "--minimized");

    let st = Arc::new(Shared {
        cfg: Mutex::new(AppConfig::load()),
        paused_until: Mutex::new(None),
        initiated_until: Mutex::new(None),
        last_mode: Mutex::new(None),
        applied: Mutex::new(String::new()),
        trigger_sys: Mutex::new(false),
        wmi_ok: AtomicBool::new(false),
        dashboard_child: Mutex::new(None),
        last_switch: Mutex::new(None),
        hidden_windows: Mutex::new(Vec::new()),
        startup_cache: Mutex::new((false, Instant::now() - Duration::from_secs(3600))),
    });
    let fresh = AppConfig::path().map(|p| !p.exists()).unwrap_or(false);
    let _ = st.cfg.lock().unwrap().save(); // persist defaults on first run
    if fresh {
        // First run: autostart on by default (standard level, no prompt).
        // An explicit user choice (registry/task) is never overridden.
        if let Err(e) = set_startup(true) {
            log_file(&format!("default autostart failed: {e:?}"));
        } else {
            log_file("autostart enabled by default on first run");
        }
    }
    modify_cfg(&st, |c| {
        c.daemon_elevated = Some(is_elevated());
        c.daemon_build = Some(BUILD_ID.to_string());
    });

    // Seed from G-Helper's live mode. Never switch blindly at startup:
    // only real drift (with G-Helper running) triggers one silent switch.
    let current = read_ghelper_mode();
    *st.last_mode.lock().unwrap() = current.clone();
    // Honor a pending restart request (set just before a crash/exit).
    if st.cfg.lock().unwrap().restart_request {
        modify_cfg(&st, |c| c.restart_request = false);
        restart_app(&st);
    }
    // A stale "wmi" value from an earlier elevated run would lie to the
    // dashboard; only a live subscription may set it (see wmi_thread).
    // Same for a stale switch time: a fresh daemon hasn't switched yet.
    modify_cfg(&st, |c| {
        c.event_source = None;
        c.last_switch_millis = None;
    });
    let paused_for = pause_remaining(&st.cfg.lock().unwrap().clone()).map(|d| Instant::now() + d);
    *st.paused_until.lock().unwrap() = paused_for;
    let (target, _count, _hit_sys) = desired_profile(&st);
    *st.applied.lock().unwrap() = current.clone().unwrap_or_else(|| target.clone());
    if let Some(cur) = current {
        if cur != target && ghelper_running() {
            log_info!("startup drift: G-Helper is {cur}, want {target}; switching silently");
            apply_profile(&st, &target);
        }
    }

    for class in ["Win32_ProcessStartTrace", "Win32_ProcessStopTrace"] {
        let s = Arc::clone(&st);
        std::thread::spawn(move || wmi_thread(s, class));
    }
    converge_admin(&st);
    notify_thread(Arc::clone(&st));
    own_cfg_thread(Arc::clone(&st));
    poll_thread(Arc::clone(&st));

    // Daemon message pump: tray icon lives on this thread. Zero CPU while
    // idle (500ms timeout only pumps tray/window messages).
    let tray = build_tray();
    log_file("tray ready");
    let mut quit = false;
    loop {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            log_file(&format!("tray menu event: {ev:?}"));
            if ev.id == tray.restart_id {
                modify_cfg(&st, |c| c.run_as_admin = true);
                restart_app(&st);
            } else if ev.id == tray.exit_id {
                if let Some(mut child) = st.dashboard_child.lock().unwrap().take() {
                    let _ = child.kill();
                }
                kill_stray_dashboards();
                log_file("exiting");
                quit = true;
                break;
            }
        }
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            match ev {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
                | TrayIconEvent::DoubleClick { .. } => toggle_dashboard(&st, &tray),
                _ => {}
            }
        }
        if quit {
            break;
        }
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = MsgWaitForMultipleObjects(Some(&[]), false, 500, QS_ALLINPUT);
        }
    }
    Ok(())
}
