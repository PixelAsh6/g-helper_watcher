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
use windows::Win32::Foundation::{BOOL, ERROR_ALREADY_EXISTS, FALSE, HANDLE, HWND, LPARAM, SYSTEMTIME, TRUE};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
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
    DispatchMessageW, EnumWindows, FindWindowW, GetForegroundWindow, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, MsgWaitForMultipleObjects, PeekMessageW,
    SetForegroundWindow, ShowWindow, TranslateMessage, MSG, PM_REMOVE, QS_ALLINPUT, SW_HIDE,
    SW_NORMAL, SW_RESTORE, SW_SHOW, WM_QUIT,
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

/// Local timestamp for the debug log: DD-MM-YYYY:HH-MM-SS.
fn stamp() -> String {
    // SAFETY: plain system query into a local.
    unsafe {
        let st: SYSTEMTIME = GetLocalTime();
        format!(
            "{:02}-{:02}-{:04}:{:02}-{:02}-{:02}",
            st.wDay, st.wMonth, st.wYear, st.wHour, st.wMinute, st.wSecond
        )
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
        let _ = writeln!(f, "{} {line}", stamp());
    }
}

fn error_console(msg: &str) {
    if CONSOLE.load(Ordering::SeqCst) == 1 {
        eprintln!("{msg}");
        return;
    }
    // Boot/autostart launches always carry --minimized: stay file-only and
    // silent (no console flash, no prompt). Manual runs may pop a console.
    if std::env::args().any(|a| a == "--minimized") {
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
    /// Last battery-hold state (transition-only logging, polled every 2s).
    batt_hold: AtomicBool,
    /// Last dashboard spawn (debounces double tray presses into one window).
    last_dash_spawn: Mutex<Instant>,
    /// Elevation restart requested (watcher thread signals, main thread acts).
    restart_pending: AtomicBool,
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

/// Hot-path refresh: exe names only, fetched once per PID (a path never
/// changes for a live process). `cmd` is excluded: reading it doubled the
/// per-process handle opens on every 2s tick.
fn exe_kinds() -> ProcessRefreshKind {
    ProcessRefreshKind::new().with_exe(sysinfo::UpdateKind::OnlyIfNotSet)
}

/// Full refresh for the rare paths that read command lines (stray-killer).
fn full_kinds() -> ProcessRefreshKind {
    ProcessRefreshKind::new()
        .with_exe(sysinfo::UpdateKind::OnlyIfNotSet)
        .with_cmd(sysinfo::UpdateKind::Always)
}

/// Run `f` against a thread-local reused `System` instead of building a
/// fresh `System::new()` per scan (each build re-enumerates everything).
fn with_system<R>(kinds: ProcessRefreshKind, f: impl FnOnce(&mut System) -> R) -> R {
    thread_local! {
        static SYS: std::cell::RefCell<System> = std::cell::RefCell::new(System::new());
    }
    SYS.with(|sys| {
        let mut sys = sys.borrow_mut();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kinds);
        f(&mut sys)
    })
}

/// Lowercase exe file names of all running processes (sysinfo view).
fn sysinfo_names() -> Vec<String> {
    with_system(exe_kinds(), |sys| {
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

/// Visible top-level windows with their owning exe: (title, exe).
/// Windows share processes (G-Helper's Fans+Power lives inside the single
/// GHelper.exe), so a process-only picker can never show them.
pub fn open_windows() -> Vec<(String, String)> {
    struct Ctx {
        items: Vec<(String, u32)>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        // SAFETY: lp is our Ctx pointer for the duration of this call.
        unsafe {
            let ctx = &mut *(lp.0 as *mut Ctx);
            if !IsWindowVisible(hwnd).as_bool() {
                return TRUE;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 {
                return TRUE;
            }
            let mut buf = [0u16; 256];
            if GetWindowTextW(hwnd, &mut buf) == 0 {
                return TRUE;
            }
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let title = String::from_utf16_lossy(&buf[..end]);
            if !title.trim().is_empty() {
                ctx.items.push((title, pid));
            }
        }
        TRUE
    }
    let mut ctx = Ctx { items: Vec::new() };
    // SAFETY: the callback only touches ctx; EnumWindows is synchronous.
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    with_system(exe_kinds(), |sys| {
        let mut out = Vec::new();
        for (title, pid) in &ctx.items {
            if title == MAIN_TITLE {
                continue; // our own dashboard, not a watch target
            }
            let exe = sys
                .processes()
                .get(&sysinfo::Pid::from_u32(*pid))
                .and_then(|p| p.exe().and_then(|x| x.file_name()))
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if exe.is_empty() {
                continue;
            }
            out.push((title.clone(), exe));
        }
        out.sort();
        out.dedup();
        out.truncate(200);
        out
    })
}

/// Human-readable boot entry for the dashboard, e.g.
/// `Task: C:\Apps\GHelper\G-Helper Watcher.exe --minimized`,
/// `Run: "..." --minimized`, or `Off`.
pub fn startup_location() -> String {
    if let Some(exe) = admin_task_exe() {
        return format!("Task: {exe} --minimized");
    }
    if let Some(cmd) = run_key_cmd() {
        return format!("Run: {cmd}");
    }
    "Off".to_string()
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

/// True when running on battery power (unknown = false: never hold blindly).
pub fn on_battery() -> bool {
    // SAFETY: plain system query into a local.
    unsafe {
        let mut st = SYSTEM_POWER_STATUS::default();
        if GetSystemPowerStatus(&mut st).is_err() {
            return false;
        }
        st.ACLineStatus == 0
    }
}

/// Battery hold: with the toggle on and the charger unplugged, revert to
/// the default profile and skip all further auto-switching until AC
/// returns. Logs transitions only (evaluated every 2s, never spam).
fn battery_hold(st: &Arc<Shared>) -> bool {
    let hold = cfg_snapshot(st).pause_on_battery && on_battery();
    if hold != st.batt_hold.swap(hold, Ordering::SeqCst) {
        log_file(if hold {
            "on battery: reverting to default profile, auto-switch held"
        } else {
            "on AC power: auto-switch resumed"
        });
    }
    if hold {
        // Converge to the default profile while held (the switch cooldown
        // inside apply_profile still applies; ticks retry until it lands).
        let cfg = cfg_snapshot(st);
        if ghelper_running() && st.current_applied() != cfg.default_profile {
            apply_profile(st, &cfg.default_profile);
        }
    }
    hold
}

/// Re-evaluate with the full (sysinfo + WMI) picture. No-op while paused.
pub fn evaluate(st: &Arc<Shared>) {
    if st.is_paused() {
        return;
    }
    if battery_hold(st) {
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
    if battery_hold(st) {
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
        // Signal the main thread: it drops the tray icon first, so the
        // restart exit leaves no ghost icon behind.
        st.restart_pending.store(true, Ordering::SeqCst);
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
        Err(_) => "\"G-Helper Watcher.exe\" --minimized".to_string(),
    }
}

/// File name of this binary (lowercased). The exe may be renamed freely
/// (e.g. `G-Helper Watcher.exe`); never hardcode the file name.
fn own_exe_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.file_name()
                .map(|s| s.to_string_lossy().to_lowercase())
        })
        .unwrap_or_else(|| "g-helper watcher.exe".to_string())
}

fn is_autostart() -> bool {
    run_key_cmd().is_some()
}

/// Effective autostart (Run key or elevated logon task).
pub fn is_startup_enabled() -> bool {
    is_autostart() || admin_task_exists()
}

/// Raw Run-key command, if present. Existence = the user once enabled
/// standard autostart; the VALUE may still be stale (binary moved).
fn run_key_cmd() -> Option<String> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .and_then(|k| k.get_value::<String, _>(REG_NAME))
        .ok()
}

/// True when the Run value exists AND launches this exact binary.
/// Comparison is case-insensitive: Windows paths and schtasks XML
/// disagree on case far too often for an exact match.
fn run_key_valid() -> bool {
    match run_key_cmd() {
        Some(v) => same_exe(&v, &exe_cmdline()),
        None => false,
    }
}

/// Case-insensitive exe-command comparison (ignores quoting/case only).
fn same_exe(a: &str, b: &str) -> bool {
    let clean = |s: &str| s.trim().trim_matches('"').trim().to_lowercase();
    clean(a) == clean(b)
}

/// Set autostart, honoring the run-as-admin preference.
/// Overwrites unconditionally so a moved/renamed binary heals its own
/// entries instead of booting a dead path forever. Silent: registry writes
/// need no elevation; task writes fail standard-side (caller reverts UI).
pub fn set_startup(on: bool) -> std::io::Result<()> {
    let admin = AppConfig::load().run_as_admin;
    if on {
        if admin {
            // Requires elevation: fails standard-side with access denied
            // (GUI reverts the checkbox); the next elevated converge heals.
            set_admin_task(true)?;
            remove_run_key();
        } else {
            set_autostart(true)?;
            // A stale elevated task would double-launch: best-effort remove
            // (denied standard-side until the next elevated converge).
            if admin_task_exists() {
                let _ = set_admin_task(false);
            }
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

fn build_tray() -> anyhow::Result<TrayBundle> {
    let menu = Menu::new();
    let exit_id = MenuId::new("exit");
    let restart_id = MenuId::new("restart");
    let restart = MenuItem::with_id(restart_id.clone(), "Restart elevated", true, None);
    let exit = MenuItem::with_id(exit_id.clone(), "Quit", true, None);
    if !is_elevated() {
        // Already admin: nothing to escalate to, so no item.
        menu.append(&restart)
            .map_err(|e| anyhow::anyhow!("tray menu append restart: {e:?}"))?;
    }
    menu.append(&exit)
        .map_err(|e| anyhow::anyhow!("tray menu append exit: {e:?}"))?;
    let icon = Icon::from_rgba(g_icon_rgba(32), 32, 32)
        .map_err(|e| anyhow::anyhow!("tray icon: {e:?}"))?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("G-Helper Watcher")
        .with_icon(icon)
        .build()
        .map_err(|e| anyhow::anyhow!("tray build: {e:?}"))?;
    Ok(TrayBundle {
        tray,
        restart,
        exit,
        restart_id,
        exit_id,
    })
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

/// Wait until Explorer's taskbar exists (bounded). The daemon (logon task)
/// routinely starts before the notification area does at boot.
fn wait_for_shell(timeout: Duration) {
    let start = Instant::now();
    loop {
        // SAFETY: read-only window lookup + state query.
        let ready = unsafe {
            match FindWindowW(windows::core::w!("Shell_TrayWnd"), PCWSTR::null()) {
                Ok(hwnd) if !hwnd.is_invalid() => IsWindowVisible(hwnd).as_bool(),
                _ => false,
            }
        };
        if ready {
            // The notify area settles just after the taskbar window appears.
            std::thread::sleep(Duration::from_secs(2));
            return;
        }
        if start.elapsed() > timeout {
            log_file("taskbar not found, building tray anyway");
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Bring the dashboard window forward (used on unhide/refocus).
/// Single OS window, single taskbar entry: show first (a plain foreground
/// call can leave it behind other apps), then take focus.
fn focus_dashboard() {
    if let Some(h) = find_window(MAIN_TITLE) {
        show_window(h);
    }
    focus_window(MAIN_TITLE);
}

/// Kill any dashboard child processes, including orphans from older
/// generations that this instance never tracked (e.g. after a taskkill).
fn kill_stray_dashboards() {
    let me = std::process::id();
    with_system(full_kinds(), |sys| {
        for (pid, p) in sys.processes() {
        if pid.as_u32() == me {
            continue;
        }
        let exe = p
            .exe()
            .and_then(|x| x.file_name())
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if exe != own_exe_name() {
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
/// - nothing open -> open the main window;
/// - open but behind other apps -> bring it forward;
/// - ours in front -> hide it (unhidden on the next click).
/// Single OS window: at most one dashboard process ever (strays die on
/// spawn), so the taskbar shows exactly one entry.
fn toggle_dashboard(st: &Arc<Shared>, tray: &TrayBundle) {
    if let Some(h) = find_visible(MAIN_TITLE) {
        if foreground_window() == Some(h) {
            // Ours in front: hide it, remember for the next click.
            // SAFETY: hiding a looked-up handle.
            unsafe {
                let _ = ShowWindow(h, SW_HIDE);
            }
            let mut hidden = st.hidden_windows.lock().unwrap();
            hidden.clear();
            hidden.push(MAIN_TITLE.to_string());
            log_file("dashboard hidden by tray toggle");
        } else {
            // Open but unfocused: bring it forward.
            focus_dashboard();
        }
        return;
    }
    // Nothing visible: unhide the remembered window if it still exists.
    let hidden: Vec<String> = st.hidden_windows.lock().unwrap().clone();
    st.hidden_windows.lock().unwrap().clear();
    if hidden.iter().any(|n| n == MAIN_TITLE) {
        if find_window(MAIN_TITLE).is_some() {
            focus_dashboard();
            return;
        }
    }
    // No window at all: drop any windowless zombie, kill strays, spawn.
    spawn_dashboard(st, tray);
}

/// Spawn the dashboard child (tray toggle + post-restart reopen share it).
/// At most one dashboard process ever: strays die here, so duplicates
/// (e.g. after a rebuild while old windows linger) cannot accumulate.
/// Double tray presses within 2s collapse into one window: the second
/// spawn is skipped (the first child is still showing its window).
fn spawn_dashboard(st: &Arc<Shared>, tray: &TrayBundle) {
    {
        let mut t = st.last_dash_spawn.lock().unwrap();
        if t.elapsed() < Duration::from_secs(2) {
            log_file("dashboard spawn debounced (double toggle)");
            focus_dashboard();
            return;
        }
        *t = Instant::now();
    }    let mut slot = st.dashboard_child.lock().unwrap();
    if let Some(child) = slot.as_mut() {
        if let Ok(None) = child.try_wait() {
            let _ = child.kill();
        }
        *slot = None;
    }
    drop(slot);
    kill_stray_dashboards();
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("G-Helper Watcher.exe"));
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
        // XML import carries the battery-proof + immortal settings; fall
        // back to the legacy /TR form if the import is ever rejected.
        let exe = std::env::current_exe()?;
        let exe_s = exe.to_string_lossy().into_owned();
        let dir = exe
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        match import_task_xml(&task_xml_doc(&exe_s, &dir)) {
            Ok(()) => Ok(()),
            Err(e) => {
                log_file(&format!("task XML import failed ({e:?}); trying legacy /TR"));
                let task_run = format!("\"{exe_s}\" --minimized");
                let out = schtasks(&[
                    "/Create", "/TN", TASK_NAME, "/TR", &task_run, "/SC", "ONLOGON", "/RL",
                    "HIGHEST", "/F",
                ])?;
                if out.status.success() {
                    Ok(())
                } else {
                    Err(err(out))
                }
            }
        }
    } else if admin_task_exists() {
        let out = schtasks(&["/Delete", "/TN", TASK_NAME, "/F"])?;
        if !out.status.success() {
            return Err(err(out));
        }
        Ok(())
    } else {
        Ok(())
    }
}

/// Raw `schtasks /Query /TN TASK /XML` output, if the task exists.
/// schtasks emits UTF-16; plain UTF-8 is accepted as fallback.
fn admin_task_xml() -> Option<String> {
    let out = schtasks(&["/Query", "/TN", TASK_NAME, "/XML"]).ok()?;
    if !out.status.success() {
        return None;
    }
    let b = &out.stdout;
    let text = if b.len() >= 2 && b[0] == 0xFF && b[1] == 0xFE {
        let u16v: Vec<u16> = b[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&u16v)
    } else {
        String::from_utf8_lossy(b).into_owned()
    };
    (!text.trim().is_empty()).then_some(text)
}

/// Exe the logon task points at (parsed from `<Command>`), if any.
/// Existence = the user once enabled elevated autostart; the VALUE may
/// still be stale (binary moved) — see `admin_task_valid`.
fn admin_task_exe() -> Option<String> {
    let xml = admin_task_xml()?;
    let open = "<Command>";
    let close = "</Command>";
    let start = xml.find(open)? + open.len();
    let end = xml[start..].find(close)? + start;
    let cmd = xml[start..end].trim().trim_matches('"').trim().to_string();
    (!cmd.is_empty()).then_some(cmd)
}

/// True when the logon task exists AND launches this exact binary.
fn admin_task_valid() -> bool {
    match (admin_task_exe(), std::env::current_exe()) {
        (Some(t), Ok(exe)) => same_exe(&t, &exe.to_string_lossy()),
        _ => false,
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Logon-task definition: silent elevation at logon (InteractiveToken +
/// HighestAvailable = zero UAC prompts), battery-proof for laptops
/// (schtasks defaults refuse to start on battery and kill the daemon when
/// unplugging), and immortal (defaults kill long-running tasks after 72h).
/// Command is the bare path (canonical form); arguments stay separate so
/// spaces in the install dir can never misparse.
fn task_xml_doc(exe: &str, workdir: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
         <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
         <RegistrationInfo><URI>\\{task}</URI></RegistrationInfo>\n\
         <Principals><Principal id=\"Author\"><LogonType>InteractiveToken</LogonType>\
         <RunLevel>HighestAvailable</RunLevel></Principal></Principals>\n\
         <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\
         <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\
         <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\
         <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\
         <Enabled>true</Enabled><AllowStartOnDemand>true</AllowStartOnDemand>\
         <StartWhenAvailable>true</StartWhenAvailable></Settings>\n\
         <Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>\n\
         <Actions Context=\"Author\"><Exec><Command>{cmd}</Command>\
         <Arguments>--minimized</Arguments><WorkingDirectory>{dir}</WorkingDirectory>\
         </Exec></Actions>\n</Task>",
        task = TASK_NAME,
        cmd = xml_escape(exe),
        dir = xml_escape(workdir),
    )
}

/// Register the task from an XML document (UTF-16LE with BOM, like the
/// track schtasks itself exports). Always `/F`: idempotent overwrite.
fn import_task_xml(xml: &str) -> std::io::Result<()> {
    let mut path = std::env::temp_dir();
    path.push("ghelper-watcher-task.xml");
    let mut bytes = vec![0xFFu8, 0xFE];
    for u in xml.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(&path, &bytes)?;
    let path_s = path.to_string_lossy().into_owned();
    let out = schtasks(&["/Create", "/TN", TASK_NAME, "/XML", &path_s, "/F"])?;
    let _ = std::fs::remove_file(&path);
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }
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
/// Storage is self-healing: a moved/renamed binary overwrites its own stale
/// entries (same silent level, current path) instead of booting dead paths.
fn converge_admin(st: &Arc<Shared>) {
    let want = cfg_snapshot(st).run_as_admin;
    let have = is_elevated();
    // Intent is inferred from storage: any leftover (Run key or task) means
    // the user had autostart ON. Queried fresh every time: a just-deleted
    // OFF state must never be resurrected by a stale cache.
    let intent_on = is_startup_enabled();
    if !intent_on {
        // Respect OFF: remove leftovers, create nothing. (Previously an
        // elevated daemon with a standard pref force-created the Run key,
        // making OFF impossible while elevated.)
        remove_run_key();
        if admin_task_exists() {
            let _ = set_admin_task(false);
        }
        return;
    }
    if want && have {
        // Elevated + admin pref: the task is the single source. Overwrite
        // only when stale (keeps Last Run info on every game launch);
        // the overwrite also repairs battery + 72h-limit settings.
        if admin_task_valid() {
            remove_run_key();
        } else if let Err(e) = set_admin_task(true) {
            log_file(&format!("admin task setup failed: {e:?}"));
        } else {
            remove_run_key();
        }
    } else if !want {
        // Standard pref: the Run key is the single source (HKCU needs no
        // elevation). Overwrite when stale; drop any leftover task.
        if !run_key_valid() {
            if let Err(e) = set_autostart(true) {
                log_file(&format!("autostart repair failed: {e:?}"));
            }
        }
        if admin_task_exists() {
            let _ = set_admin_task(false);
        }
    }
    // want && !have (standard process, admin pref): nothing — escalation
    // happens only via explicit Restart (button/menu) or boot. Never prompt.
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
    // A visible dashboard already exists (tray double-press raced a second
    // spawn past the debounce): exit instead of opening a twin window.
    if find_visible(MAIN_TITLE).is_some() {
        log_file("second visible dashboard, exiting");
        std::process::exit(0);
    }
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
    // Mixed-integrity leftovers (e.g. a standard dashboard child surviving
    // an elevated restart, or vice versa) would run at the wrong level and
    // confuse every elevation-gated toggle: drop them, tray reopens on demand.
    kill_stray_dashboards();
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
        batt_hold: AtomicBool::new(false),
        last_dash_spawn: Mutex::new(Instant::now() - Duration::from_secs(3600)),
        restart_pending: AtomicBool::new(false),
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
    // A stale restart request (crash between set and consume) is cleared
    // without acting: auto-escalating here would UAC-prompt at logon.
    // The user re-presses the button when they actually want elevation.
    if st.cfg.lock().unwrap().restart_request {
        modify_cfg(&st, |c| c.restart_request = false);
        log_file("stale restart request cleared (no auto-escalation at startup)");
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
    // At logon Explorer's notification area may not exist yet — and a
    // too-early build can HALF-register, leaving a phantom twin icon next
    // to the real one. Wait for the taskbar first, then build.
    wait_for_shell(Duration::from_secs(60));
    // Retries remain as a backstop (file-only logging, never a console
    // flash): a panic here is silent death with no tray and no error.
    let tray = loop {
        match build_tray() {
            Ok(t) => break t,
            Err(e) => {
                log_file(&format!("tray not ready ({e:?}); retrying"));
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    };
    log_file("tray ready");
    // Post-restart reopen: the dashboard the restart was requested from was
    // killed with the old generation — spawn it back (at the new integrity
    // level) so the window the user clicked in comes back focused.
    if st.cfg.lock().unwrap().reopen_dashboard {
        modify_cfg(&st, |c| c.reopen_dashboard = false);
        log_file("reopening dashboard after restart");
        spawn_dashboard(&st, &tray);
    }
    let mut quit = false;
    loop {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            log_file(&format!("tray menu event: {ev:?}"));
            if ev.id == tray.restart_id {
                // Keep the window: reopen it once the restart lands (the
                // startup stray-kill would otherwise eat it). The main
                // thread performs the restart AFTER dropping the tray icon,
                // so no ghost icon is left behind (process::exit skips Drop).
                let ui_open = find_visible(MAIN_TITLE).is_some()
                    || !st.hidden_windows.lock().unwrap().is_empty()
                    || st
                        .dashboard_child
                        .lock()
                        .unwrap()
                        .as_mut()
                        .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
                modify_cfg(&st, |c| {
                    c.run_as_admin = true;
                    if ui_open {
                        c.reopen_dashboard = true;
                    }
                });
                st.restart_pending.store(true, Ordering::SeqCst);
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
        if st.restart_pending.load(Ordering::SeqCst) {
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
    // Drop the tray icon BEFORE any restart exit: process::exit skips Drop,
    // which used to leave one ghost tray icon per elevated restart.
    drop(tray);
    if !quit && st.restart_pending.swap(false, Ordering::SeqCst) {
        restart_app(&st);
    }
    Ok(())
}
