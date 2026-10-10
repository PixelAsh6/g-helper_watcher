use crate::config::AppConfig;
use crate::{
    cooldown_remaining, daemon_alive, ghelper_current_mode, is_elevated, is_startup_enabled,
    pause_remaining, set_startup,
};
use eframe::egui;
use std::os::windows::process::CommandExt as _;
use std::time::{Duration, Instant};

const PROFILES: [&str; 3] = ["Silent", "Balanced", "Turbo"];

// ROG-cockpit direction: near-black blue base, one green accent, mono readouts,
// borders-only depth. Who: an ASUS gamer picking trigger apps in <30s.
const VOID: egui::Color32 = egui::Color32::from_rgb(0x17, 0x18, 0x1C);
const CARD: egui::Color32 = egui::Color32::from_rgb(0x23, 0x26, 0x2C);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x22, 0xC5, 0x5E);
const AMBER: egui::Color32 = egui::Color32::from_rgb(0xFB, 0xBF, 0x24);
const RED: egui::Color32 = egui::Color32::from_rgb(0xF8, 0x71, 0x71);
const MUTED: egui::Color32 = egui::Color32::from_rgb(0x9A, 0xA0, 0xAE);

pub(crate) fn dashboard_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.window_fill = VOID;
    v.panel_fill = VOID;
    v.faint_bg_color = CARD;
    v.extreme_bg_color = VOID;
    v.selection.bg_fill = egui::Color32::from_rgb(0x1D, 0x3A, 0x2A);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = egui::CornerRadius::same(6);
    }
    v
}

fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let w = ui.available_width();
    egui::Frame::group(ui.style())
        .fill(CARD)
        .stroke(egui::Stroke::new(
            1.0_f32,
            egui::Color32::from_white_alpha(20),
        ))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_min_width((w - 24.0).max(0.0));
            add(ui);
        });
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .size(14.0)
            .strong()
            .color(egui::Color32::WHITE),
    );
    ui.add_space(6.0);
}

fn dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn mode_color(name: &str) -> egui::Color32 {
    match name {
        "Silent" => ACCENT,
        "Balanced" => egui::Color32::from_rgb(0x60, 0xA5, 0xFA),
        "Turbo" => egui::Color32::from_rgb(0xA7, 0x8B, 0xFA),
        _ => MUTED,
    }
}

/// G-Helper-style mode tile: outline-only color, mini bar icon, label.
/// Border drawn as outer fill + inset fill (no StrokeKind needed).
/// Taller targets (62px) with stronger unselected contrast.
fn mode_tile(ui: &mut egui::Ui, label: &str, selected: bool, width: f32) -> egui::Response {
    let color = mode_color(label);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 62.0), egui::Sense::click());
    let p = ui.painter();
    let edge = if selected {
        color
    } else {
        egui::Color32::from_white_alpha(60)
    };
    let inset = if selected { 2.0 } else { 1.0 };
    p.rect_filled(rect, egui::CornerRadius::same(8), edge);
    p.rect_filled(rect.shrink(inset), egui::CornerRadius::same(7), CARD);
    let bars = match label {
        "Silent" => [4.0, 5.0, 4.0],
        "Balanced" => [7.0, 10.0, 8.0],
        "Turbo" => [5.0, 9.0, 13.0],
        _ => [6.0, 6.0, 6.0],
    };
    for (i, h) in bars.iter().enumerate() {
        let r = egui::Rect::from_min_size(
            egui::pos2(
                rect.min.x + 12.0 + i as f32 * 6.0,
                rect.center().y + 7.0 - h,
            ),
            egui::vec2(4.0, *h),
        );
        p.rect_filled(r, egui::CornerRadius::same(1), color);
    }
    p.text(
        egui::pos2(rect.center().x + 10.0, rect.center().y),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(14.0),
        if selected {
            egui::Color32::WHITE
        } else {
            egui::Color32::from_rgb(0xC7, 0xCC, 0xD6)
        },
    );
    resp
}

/// Standalone dashboard child process (`--dashboard`). Syncs with the daemon
/// through the shared config file: reloads it every second, saves on edit.
/// Closing the window just exits this child; the daemon keeps running.
pub struct DashboardApp {
    cfg: AppConfig,
    filter: String,
    candidate: Option<String>,
    snap: Vec<String>,
    snap_gen: u64,
    bg: std::sync::Arc<BgState>,
    last_snap: Instant,
    last_reload: Instant,
    autostart_at: Instant,
    autostart: bool,
    admin: bool,
    was_focused: bool,
    popup_pin: bool,
    show_manage: bool,
    manage_placed: bool,
    sized_once: bool,
    content_h: f32,
    sent_h: f32,
    orphan_since: Option<Instant>,
}

/// Background sampler: sysinfo + WMI + schtasks stay OFF the UI thread.
/// (A full scan stalls frames and made dragging stutter.)
struct BgState {
    snap: std::sync::Mutex<Vec<String>>,
    snap_gen: std::sync::atomic::AtomicU64,
    want_snap: std::sync::atomic::AtomicBool,
    autostart: std::sync::Mutex<bool>,
}

impl BgState {
    fn spawn() -> std::sync::Arc<BgState> {
        let bg = std::sync::Arc::new(BgState {
            snap: std::sync::Mutex::new(Vec::new()),
            snap_gen: std::sync::atomic::AtomicU64::new(0),
            want_snap: std::sync::atomic::AtomicBool::new(true),
            autostart: std::sync::Mutex::new(false),
        });
        let bg2 = std::sync::Arc::clone(&bg);
        std::thread::spawn(move || {
            let mut slow = std::time::Instant::now() - std::time::Duration::from_secs(99);
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let want = bg2
                    .want_snap
                    .swap(false, std::sync::atomic::Ordering::Relaxed);
                if want || slow.elapsed() > std::time::Duration::from_secs(30) {
                    slow = std::time::Instant::now();
                    let mut names = crate::running_exe_names();
                    names.sort();
                    names.dedup();
                    if let Ok(mut snap) = bg2.snap.lock() {
                        *snap = names;
                    }
                    bg2.snap_gen
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if let Ok(mut a) = bg2.autostart.lock() {
                        *a = crate::is_startup_enabled();
                    }
                }
            }
        });
        bg
    }
}

impl DashboardApp {
    pub fn new() -> Self {
        let cfg = AppConfig::load();
        Self {
            admin: cfg.run_as_admin,
            autostart: is_startup_enabled(),
            cfg,
            filter: String::new(),
            candidate: None,
            snap: Vec::new(),
            snap_gen: 0,
            bg: BgState::spawn(),
            last_snap: Instant::now() - Duration::from_secs(99),
            last_reload: Instant::now(),
            autostart_at: Instant::now() - Duration::from_secs(99),
            was_focused: false,
            popup_pin: false,
            show_manage: false,
            manage_placed: false,
            sized_once: false,
            content_h: 0.0,
            sent_h: 0.0,
            orphan_since: None,
        }
    }

    /// Adopt background snapshots without ever blocking the UI thread.
    fn sync_snap(&mut self) {
        let gen = self.bg.snap_gen.load(std::sync::atomic::Ordering::Relaxed);
        if gen != self.snap_gen {
            self.snap_gen = gen;
            if let Ok(snap) = self.bg.snap.lock() {
                self.snap = snap.clone();
            }
        }
        if let Ok(a) = self.bg.autostart.try_lock() {
            self.autostart = *a;
        }
        self.last_snap = Instant::now();
    }

    /// Load, mutate, persist. The daemon's file watcher picks it up.
    fn edit(&mut self, f: impl FnOnce(&mut AppConfig)) {
        let mut cfg = AppConfig::load();
        f(&mut cfg);
        if cfg.save().is_ok() {
            self.admin = cfg.run_as_admin;
            self.cfg = cfg;
        }
    }

    fn matches(&self) -> Vec<String> {
        let q = self.filter.trim().to_lowercase();
        self.snap
            .iter()
            .filter(|n| {
                !self.cfg.watched.iter().any(|w| w == *n)
                    && (q.is_empty() || n.contains(q.as_str()))
            })
            .take(40)
            .cloned()
            .collect()
    }

    fn normalize(name: &str) -> Option<String> {
        let base = std::path::Path::new(name.trim())
            .file_name()?
            .to_string_lossy()
            .to_lowercase();
        if base.is_empty() {
            return None;
        }
        Some(if base.ends_with(".exe") {
            base
        } else {
            format!("{base}.exe")
        })
    }

    /// Add the explicit pick, an exact/unique match, or the typed name.
    fn add_current(&mut self) {
        let pick = self.candidate.clone().or_else(|| {
            let q = self.filter.trim().to_lowercase();
            if q.is_empty() {
                return None;
            }
            if let Some(exact) = self.snap.iter().find(|n| *n == &q) {
                return Some(exact.clone());
            }
            let matches = self.matches();
            if matches.len() == 1 {
                return matches.into_iter().next();
            }
            Self::normalize(&self.filter)
        });
        if let Some(p) = pick {
            self.edit(|c| {
                if !c.watched.iter().any(|w| w == &p) {
                    c.watched.push(p);
                }
            });
            self.candidate = None;
            self.filter.clear();
        }
    }

    fn browse_exe(&mut self) {
        let picked = rfd::FileDialog::new()
            .add_filter("Programs", &["exe"])
            .set_title("Choose an executable to watch")
            .pick_file();
        if let Some(path) = picked {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let exe = name.to_lowercase();
                self.edit(|c| {
                    if !c.watched.iter().any(|w| w == &exe) {
                        c.watched.push(exe);
                    }
                });
            }
        }
    }

    fn now_millis() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    fn is_running(&self, exe: &str) -> bool {
        self.snap.iter().any(|n| n.eq_ignore_ascii_case(exe))
    }

    /// Pause from the dashboard (writes the shared file; daemon adopts it).
    fn pause_now(&mut self) {
        let mins = self.cfg.pause_minutes.max(1);
        let until = Self::now_millis() + mins as i64 * 60_000;
        self.edit(|c| c.paused_until_millis = Some(until));
        // Refresh immediately so the countdown shows without waiting 1s.
        self.cfg.paused_until_millis = Some(until);
    }

    fn resume_now(&mut self) {
        // Clear both pause and cooldown so resume takes effect at once.
        self.edit(|c| {
            c.paused_until_millis = None;
            c.last_switch_millis = None;
        });
        self.cfg.paused_until_millis = None;
        self.cfg.last_switch_millis = None;
    }

    fn manage_id() -> egui::ViewportId {
        egui::ViewportId::from_hash_of("ghelper-manage")
    }

    /// Best-effort start of G-Helper when it is not running. Looks in the
    /// usual install spots plus PATH; logs and does nothing when missing.
    fn start_ghelper() {
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        for dir in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(pf) = std::env::var(dir) {
                candidates.push(
                    std::path::PathBuf::from(pf)
                        .join("GHelper")
                        .join("GHelper.exe"),
                );
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("GHelper.exe"));
            }
        }
        for path in &candidates {
            if path.is_file() {
                let _ = std::process::Command::new(path)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                crate::log_file(&format!("starting G-Helper from {}", path.display()));
                return;
            }
        }
        // Last resort: PATH lookup via `where` (no console flash).
        if let Ok(out) = std::process::Command::new("where")
            .arg("GHelper.exe")
            .stdin(std::process::Stdio::null())
            .creation_flags(0x08000000)
            .output()
        {
            let found = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !found.is_empty() {
                let _ = std::process::Command::new(&found)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                crate::log_file(&format!("starting G-Helper from {found}"));
                return;
            }
        }
        crate::log_file("Start G-Helper pressed but GHelper.exe was not found");
    }

    /// Combined Apps + Settings content for the side window (G-Helper style:
    /// separate OS window, not an in-dashboard popup).
    fn render_manage_inner(&mut self, ui: &mut egui::Ui, admin_live: bool) {
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                // ---- Watched apps (running first, trigger-colored) ----
                card(ui, |ui| {
                    section(ui, "WATCHED APPS");
                    let trigger_color = mode_color(&self.cfg.trigger_profile);
                    let mut watched = self.cfg.watched.clone();
                    watched.sort_by_key(|w| (!self.is_running(w), w.to_lowercase()));
                    let mut remove: Option<String> = None;
                    for exe in watched {
                        let running = self.is_running(&exe);
                        let color = if running { trigger_color } else { MUTED };
                        // Full-width clickable row, allocated first so the
                        // hover state is known before drawing: hover shows a
                        // red X + red text, click stops tracking the app.
                        let (row_rect, row_resp) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 20.0),
                            egui::Sense::click(),
                        );
                        let hovered = row_resp.hovered();
                        {
                            let mut row = ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(row_rect)
                                    .layout(egui::Layout::left_to_right(
                                        egui::Align::Center,
                                    ))
                                    .id_salt(exe.clone()),
                            );
                            if hovered {
                                let (drect, _) = row.allocate_exact_size(
                                    egui::vec2(10.0, 10.0),
                                    egui::Sense::hover(),
                                );
                                let c = drect.center();
                                let r = 4.0;
                                row.painter().line_segment(
                                    [c + egui::vec2(-r, -r), c + egui::vec2(r, r)],
                                    egui::Stroke::new(2.0_f32, RED),
                                );
                                row.painter().line_segment(
                                    [c + egui::vec2(-r, r), c + egui::vec2(r, -r)],
                                    egui::Stroke::new(2.0_f32, RED),
                                );
                                row.label(
                                    egui::RichText::new(exe.clone())
                                        .monospace()
                                        .strong()
                                        .color(RED),
                                );
                            } else {
                                dot(&mut row, color);
                                row.label(
                                    egui::RichText::new(exe.clone())
                                        .monospace()
                                        .color(color)
                                        .strong(),
                                );
                                if running {
                                    row.label(
                                        egui::RichText::new("running")
                                            .size(11.0)
                                            .color(trigger_color),
                                    );
                                }
                            }
                        }
                        if row_resp.clicked() {
                            remove = Some(exe.clone());
                        }
                    }
                    if self.cfg.watched.is_empty() {
                        ui.label(
                            egui::RichText::new("Nothing watched yet.")
                                .size(12.0)
                                .color(MUTED),
                        );
                    }
                    if let Some(exe) = remove {
                        self.edit(|c| {
                            c.watched.retain(|w| w != &exe);
                        });
                    }
                    ui.add_space(8.0);
                    let mut popup = false;
                    ui.horizontal(|ui| {
                        let avail = ui.available_width();
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.filter)
                                .hint_text("Filter running apps — or type an exact .exe")
                                .desired_width((avail - 92.0).max(60.0)),
                        );
                        if ui.button("Add").clicked()
                            || (resp.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        {
                            self.add_current();
                        }
                        let focused_now = resp.has_focus();
                        if focused_now && !self.was_focused {
                            self.bg
                                .want_snap
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        self.was_focused = focused_now;
                        popup = focused_now && !self.filter.trim().is_empty();
                    });
                    if popup || self.popup_pin {
                        let matches = self.matches();
                        if !matches.is_empty() {
                            let fr = egui::Frame::group(ui.style())
                                .fill(CARD)
                                .stroke(egui::Stroke::new(
                                    1.0_f32,
                                    egui::Color32::from_white_alpha(20),
                                ))
                                .corner_radius(egui::CornerRadius::same(8))
                                .inner_margin(egui::Margin::same(6))
                                .show(ui, |ui| {
                                    egui::ScrollArea::vertical()
                                        .max_height(150.0)
                                        .show(ui, |ui| {
                                            for m in matches {
                                                let sel =
                                                    self.candidate.as_deref() == Some(&m);
                                                if ui.selectable_label(sel, &m).clicked() {
                                                    self.filter = m.clone();
                                                    self.candidate = Some(m);
                                                }
                                            }
                                        });
                                });
                            self.popup_pin = fr.response.hovered();
                        }
                    }
                    ui.add_space(4.0);
                    if ui
                        .add_sized(
                            [ui.available_width(), 30.0],
                            egui::Button::new("Browse .exe…"),
                        )
                        .clicked()
                    {
                        self.browse_exe();
                    }
                });
                ui.add_space(12.0);
                // ---- Settings ----
                card(ui, |ui| {
                    section(ui, "SETTINGS");
                    ui.horizontal(|ui| {
                        ui.label("Pause (min):");
                        let mut mins = self.cfg.pause_minutes as i32;
                        if ui
                            .add(egui::DragValue::new(&mut mins).range(1..=120))
                            .changed()
                        {
                            self.edit(|c| c.pause_minutes = mins as u64);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Cooldown (sec):");
                        let mut secs = self.cfg.switch_cooldown_secs as i32;
                        if ui
                            .add(egui::DragValue::new(&mut secs).range(0..=600))
                            .changed()
                        {
                            self.edit(|c| c.switch_cooldown_secs = secs.max(0) as u64);
                        }
                    });
                    ui.label(
                        egui::RichText::new("Waits this long after each auto-switch. 0 = off.")
                            .size(11.0)
                            .color(MUTED),
                    );
                    ui.add_space(4.0);
                    if ui
                        .checkbox(&mut self.autostart, "Start with Windows")
                        .changed()
                        && set_startup(self.autostart).is_err()
                    {
                        self.autostart = is_startup_enabled();
                    }
                    if ui
                        .checkbox(&mut self.admin, "Run as administrator")
                        .changed()
                    {
                        let on = self.admin;
                        self.edit(move |c| c.run_as_admin = on);
                    }
                    if self.admin && !admin_live {
                        ui.label(
                            egui::RichText::new(
                                "Waiting for elevation — approve the UAC prompt, or uncheck + check again.",
                            )
                            .size(11.0)
                            .color(AMBER),
                        );
                    }
                    ui.label(
                        egui::RichText::new(
                            "Admin = instant process events via a logon task. UAC prompts on change.",
                        )
                        .size(11.0)
                        .color(MUTED),
                    );
                });
            });
    }

    /// Size to content on first frame, grow-only after (never fights
    /// manual resizing). Content-driven, so it converges. Also re-docks
    /// to the work-area bottom-right (just above the taskbar) so the
    /// window opens like G-Helper instead of floating mid-screen:
    /// InnerSize alone keeps the top-left anchored, which leaves a gap
    /// below after shrinking from the initial tall size.
    fn fit_window(&mut self, ctx: &egui::Context) {
        let want = (self.content_h + 28.0).clamp(420.0, crate::max_dash_height());
        let cur = ctx.screen_rect().height();
        let grow = want > cur + 2.0;
        if !self.sized_once || grow {
            self.sized_once = true;
            if (self.sent_h - want).abs() > 1.0 {
                self.sent_h = want;
                crate::log_file(&format!(
                    "dashboard fit: want={want:.0} cur={cur:.0} content={:.0}",
                    self.content_h
                ));
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                crate::DASH_W,
                want,
            )));
            // Re-pin bottom-right so shrink/grow keeps the bottom edge
            // docked 12px above the taskbar instead of floating.
            if let Some(p) = crate::dashboard_bottom_right(want) {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(p));
            }
        }
    }
}

fn mm_ss(d: Duration) -> String {
    format!("{:02}:{:02}", d.as_secs() / 60, d.as_secs() % 60)
}

impl eframe::App for DashboardApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Stale binary pairing: a newer daemon is running elsewhere.
        if self.cfg.daemon_build.as_deref() != Some(crate::BUILD_ID) {
            crate::log_file("stale dashboard (build mismatch), exiting");
            std::process::exit(0);
        }
        // Main window X closes everything: the Manage side window dies
        // with this process (Manage X alone only closes Manage).
        if ctx.input(|i| i.viewport().close_requested()) {
            crate::log_file("main dashboard closed, exiting");
            std::process::exit(0);
        }
        self.sync_snap();
        // No daemon, no GUI: close orphans instead of lingering.
        if crate::daemon_alive() {
            self.orphan_since = None;
        } else if self.orphan_since.is_none() {
            self.orphan_since = Some(Instant::now());
        } else if self
            .orphan_since
            .is_some_and(|t| t.elapsed() > Duration::from_secs(20))
        {
            crate::log_file("daemon gone 20s, closing orphan dashboard");
            std::process::exit(0);
        }
        // Hidden (tray toggle) means no countdowns, no snapshots, no disk
        // reads — just a 20s watchdog tick so a dead daemon still closes
        // us. Unhide wakes eframe immediately via SW_SHOW.
        let hidden = crate::main_hidden();
        if hidden {
            ctx.request_repaint_after(Duration::from_secs(20));
        } else {
            // Live countdown repaint.
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        if !hidden {
            if self.last_reload.elapsed() > Duration::from_secs(1) {
                self.last_reload = Instant::now();
                self.cfg = AppConfig::load();
                // schtasks query is slow: refresh autostart at most every 30s.
                if self.autostart_at.elapsed() > Duration::from_secs(30) {
                    self.autostart_at = Instant::now();
                    self.autostart = is_startup_enabled();
                }
                self.admin = self.cfg.run_as_admin;
            }
            if self.last_snap.elapsed() > Duration::from_secs(30) {
                self.last_snap = Instant::now();
                self.bg
                    .want_snap
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }

        let paused = pause_remaining(&self.cfg);
        let running_count = self
            .snap
            .iter()
            .filter(|n| self.cfg.watched.iter().any(|w| w.eq_ignore_ascii_case(n)))
            .count();
        // First running watched exe in watch-list order (shown next to the
        // count in its trigger-profile color; full list lives in Manage).
        let first_running = self
            .cfg
            .watched
            .iter()
            .find(|w| self.snap.iter().any(|n| n.eq_ignore_ascii_case(w)))
            .cloned();
        let trigger_color = mode_color(&self.cfg.trigger_profile);
        let admin_live =
            daemon_alive() && (self.cfg.daemon_elevated == Some(true) || is_elevated());

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(ui, |ui| {
                    ui.add_space(2.0);

                    // ---- Status cockpit card (focal point) ----
                    card(ui, |ui| {
                        // Pause sits beside the title + Active rows (not in
                        // the Active row itself), so no gap opens up between
                        // Active and the G-Helper mode row below.
                        // Top-aligned: button top matches the title top.
                        ui.horizontal_top(|ui| {
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new("STATUS")
                                        .size(14.0)
                                        .strong()
                                        .color(egui::Color32::WHITE),
                                );
                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    match paused {
                                        Some(left) => {
                                            dot(ui, AMBER);
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "Paused · {} left",
                                                    mm_ss(left)
                                                ))
                                                .size(16.0)
                                                .strong()
                                                .color(AMBER),
                                            );
                                        }
                                        None => {
                                            dot(ui, ACCENT);
                                            ui.label(
                                                egui::RichText::new("Active")
                                                    .size(16.0)
                                                    .strong()
                                                    .color(ACCENT),
                                            );
                                        }
                                    }
                                });
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Min),
                                |ui| {
                                    // Right margin so the button doesn't hug the card edge.
                                    ui.add_space(6.0);
                                    if paused.is_some() {
                                        if ui
                                            .add_sized(
                                                [86.0, 50.0],
                                                egui::Button::new(
                                                    egui::RichText::new("Resume Now")
                                                        .size(13.0)
                                                        .strong()
                                                        .color(AMBER),
                                                ),
                                            )
                                            .clicked()
                                        {
                                            self.resume_now();
                                        }
                                    } else if ui
                                        .add_sized(
                                            [86.0, 50.0],
                                            egui::Button::new(
                                                egui::RichText::new("Pause").size(13.0),
                                            ),
                                        )
                                        .clicked()
                                    {
                                        self.pause_now();
                                    }
                                },
                            );
                        });
                        ui.add_space(6.0);
                        egui::Grid::new("status_grid")
                            .num_columns(2)
                            .spacing([12.0, 4.0])
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new("G-Helper mode").size(12.0).color(MUTED),
                                );
                                let mode = ghelper_current_mode();
                                ui.label(
                                    egui::RichText::new(mode.clone())
                                        .monospace()
                                        .color(mode_color(&mode)),
                                );
                                ui.end_row();
                                ui.label(
                                    egui::RichText::new("Watched running")
                                        .size(12.0)
                                        .color(MUTED),
                                );
                                ui.horizontal(|ui| {
                                    // Count stays gray; only the game name
                                    // takes the trigger-profile color.
                                    ui.label(
                                        egui::RichText::new(running_count.to_string())
                                            .monospace()
                                            .color(MUTED),
                                    );
                                    if let Some(exe) = &first_running {
                                        ui.label(
                                            egui::RichText::new(exe)
                                                .monospace()
                                                .strong()
                                                .color(trigger_color),
                                        );
                                    }
                                });
                                ui.end_row();
                                ui.label(
                                    egui::RichText::new("Next switch").size(12.0).color(MUTED),
                                );
                                match cooldown_remaining(&self.cfg) {
                                    Some(left) => {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "in {}s",
                                                left.as_secs().max(1)
                                            ))
                                            .size(12.0)
                                            .color(AMBER),
                                        );
                                    }
                                    None => {
                                        ui.label(
                                            egui::RichText::new("ready")
                                                .size(12.0)
                                                .color(MUTED),
                                        );
                                    }
                                }
                                ui.end_row();
                                ui.label(egui::RichText::new("G-Helper").size(12.0).color(MUTED));
                                ui.horizontal(|ui| {
                                    if self.snap.iter().any(|n| n == "ghelper.exe") {
                                        dot(ui, ACCENT);
                                        ui.label(
                                            egui::RichText::new("running").size(12.0).color(ACCENT),
                                        );
                                    } else {
                                        dot(ui, RED);
                                        ui.label(
                                            egui::RichText::new("NOT running")
                                                .size(12.0)
                                                .color(RED),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                ui.add_space(6.0);
                                                if ui.button("Start G-Helper").clicked() {
                                                    Self::start_ghelper();
                                                }
                                            },
                                        );
                                    }
                                });
                                ui.end_row();
                                ui.label(egui::RichText::new("Events").size(12.0).color(MUTED));
                                let wmi = matches!(self.cfg.event_source.as_deref(), Some("wmi"));
                                ui.horizontal(|ui| {
                                    if wmi {
                                        ui.label(
                                            egui::RichText::new("WMI · instant")
                                                .size(12.0)
                                                .color(ACCENT),
                                        );
                                    } else {
                                        ui.label(
                                            egui::RichText::new("poll · ~2s")
                                                .size(12.0)
                                                .color(AMBER),
                                        );
                                        if ui.small_button("restart UAC").clicked() {
                                            self.edit(|c| {
                                                c.run_as_admin = true;
                                                c.restart_request = true;
                                            });
                                        }
                                    }
                                });
                                ui.end_row();
                                ui.label(egui::RichText::new("Privilege").size(12.0).color(MUTED));
                                // The daemon's elevation, not this child's:
                                // the child usually runs standard either way.
                                if !daemon_alive() {
                                    ui.label(
                                        egui::RichText::new("daemon not running")
                                            .size(12.0)
                                            .color(RED),
                                    );
                                } else if admin_live {
                                    ui.label(
                                        egui::RichText::new("administrator")
                                            .size(12.0)
                                            .color(ACCENT),
                                    );
                                } else {
                                    ui.label(
                                        egui::RichText::new("standard").size(12.0).color(MUTED),
                                    );
                                }
                                ui.end_row();
                            });
                    });
                    ui.add_space(12.0);

                    // ---- Profiles (Settings lives in the header) ----
                    // Top-aligned: button top matches the title top.
                    card(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            ui.label(
                                egui::RichText::new("PROFILES")
                                    .size(14.0)
                                    .strong()
                                    .color(egui::Color32::WHITE),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Min),
                                |ui| {
                                    // Right margin so the button doesn't hug the card edge.
                                    ui.add_space(6.0);
                                    if ui
                                        .add_sized(
                                            [92.0, 30.0],
                                            egui::Button::new(
                                                egui::RichText::new("Settings").size(12.0),
                                            ),
                                        )
                                        .clicked()
                                    {
                                        self.show_manage = true;
                                        self.manage_placed = false;
                                        self.bg.want_snap.store(
                                            true,
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                    }
                                },
                            );
                        });
                        ui.add_space(6.0);
                        ui.label("Default:");
                        ui.horizontal(|ui| {
                            let w = (ui.available_width() - 16.0) / 3.0;
                            for p in PROFILES {
                                let selected = self.cfg.default_profile == *p;
                                if mode_tile(ui, p, selected, w).clicked() {
                                    let p = p.to_string();
                                    self.edit(|c| c.default_profile = p.clone());
                                }
                            }
                        });
                        ui.label("Trigger:");
                        ui.horizontal(|ui| {
                            let w = (ui.available_width() - 16.0) / 3.0;
                            for p in PROFILES {
                                let selected = self.cfg.trigger_profile == *p;
                                if mode_tile(ui, p, selected, w).clicked() {
                                    let p = p.to_string();
                                    self.edit(|c| c.trigger_profile = p.clone());
                                }
                            }
                        });
                    });
                    self.content_h = ui.min_rect().height();
                });
        });
        // ---- Manage window: separate OS window left of the dashboard ----
        // (G-Helper style: Fans+Power opens beside the main window, not as
        // an in-dashboard popup clipping the content.)
        if self.show_manage {
            let mid = Self::manage_id();
            if !self.manage_placed {
                self.manage_placed = true;
                // Dock left of the main window, same top edge.
                if let Some(r) = ctx.input(|i| i.viewport().outer_rect) {
                    let x = (r.min.x - 388.0).max(8.0);
                    ctx.send_viewport_cmd_to(
                        mid,
                        egui::ViewportCommand::OuterPosition(egui::pos2(x, r.min.y)),
                    );
                }
            }
            let mut close = false;
            ctx.show_viewport_immediate(
                mid,
                egui::ViewportBuilder::default()
                    .with_title("Manage - G-Helper Watcher")
                    .with_inner_size([380.0, 560.0])
                    .with_min_inner_size([340.0, 420.0])
                    .with_maximize_button(false)
                    .with_minimize_button(false)
                    .with_resizable(true),
                |mctx, class| {
                    if class == egui::ViewportClass::Embedded {
                        // Fallback when the backend has no multi-window:
                        // inline window inside the dashboard.
                        egui::Window::new(crate::MANAGE_TITLE).show(mctx, |ui| {
                            self.render_manage_inner(ui, admin_live);
                        });
                    } else {
                        egui::CentralPanel::default().show(mctx, |ui| {
                            ui.add_space(4.0);
                            self.render_manage_inner(ui, admin_live);
                        });
                        if mctx.input(|i| i.viewport().close_requested()) {
                            close = true;
                        }
                    }
                    mctx.request_repaint_after(Duration::from_secs(1));
                },
            );
            if close {
                self.show_manage = false;
                self.manage_placed = false;
            }
        }
        // Hidden windows need no sizing: no content work happens there.
        if !hidden {
            self.fit_window(ctx);
        }
    }
}
