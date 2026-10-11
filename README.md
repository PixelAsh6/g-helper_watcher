# G-Helper Watcher

A hyper-lightweight, tray-first Windows background daemon that automatically
switches [G-Helper](https://github.com/seerge/g-helper) performance profiles
(Silent → Turbo by default) when watched games/apps start and stop — with manual-override
protection, start-with-Windows, and an optional elevated mode for instant,
zero-polling process events.

## How it works

- **Process monitoring** — Subscribes to WMI process start/stop events
  (`Win32_ProcessStartTrace` / `Win32_ProcessStopTrace`). Needs no polling
  loop and reacts instantly. Requires elevation; otherwise a lightweight
  2s sysinfo tick + 30s merged tick takes over automatically.
- **Silent switching** — Never launches `GHelper.exe` (any CLI arg would pop
  its window — verified against its source). Instead it presses G-Helper's
  own global hotkeys (`Ctrl+Shift+Alt+F16/F17/F18`),
  so only G-Helper's small toast appears. Nothing to install, nothing moved.
- **Manual-override protection** — Watches `%AppData%\GHelper\config.json`.
  A real *mode change* you make yourself (hotkey, G-Helper UI) pauses
  auto-switching for N minutes (default 10). Fan-curve edits and the
  daemon's own switches never trigger it.
- **Switch cooldown** — after each auto-switch, further switches wait out
  a configurable cooldown (default 60s ). Absorbs games that
  start/stop several times while launching; the correct profile still
  applies automatically once it lapses.
- **Lazy GUI** — The dashboard is a separate child process spawned only from
  the tray. Close it and it's gone; the daemon keeps running.
- **Startup** — `HKCU\...\Run` entry, or (elevated) a logon scheduled task
  with highest privileges so elevation survives reboots with zero UAC prompts.

## Dashboard (single window, single taskbar entry)

- **Status card** — live state (Active / Paused countdown with Pause /
  Resume button, or Paused-on-battery), G-Helper mode, watched-running
  count plus first running app, switch cooldown, event source
  (WMI-instant vs poll), daemon privilege, Start G-Helper button when
  G-Helper is missing.
- **Profiles** — Default and Trigger tiles, with a Settings button that
  expands the section below inline (the window grows, then shrinks back).
- **Watched apps + Settings** — tracked-apps list (click a row to stop
  tracking it), type-to-filter with running-process and open-window
  suggestions (windows share one process — e.g. Fans+Power lives inside
  `GHelper.exe` — so a window row tracks its owning exe; already-listed
  exes never repeat), click a suggestion to insert it, Enter inserts an
  exact/unique match, Esc closes the list, exact-name entry, `.exe`
  browser, plus settings: pause duration, switch cooldown, pause
  switching on battery, Start with Windows (with the actual boot entry
  shown underneath), Run as Administrator. Toggle failures explain
  themselves instead of snapping back. A **restart UAC** button next to
  the Events row restarts elevated on demand and reopens the window.

## Resource usage

Working set / private MB and avg CPU (% of one core), release build,
16-core machine, measured 2026-10-11 (v1.0.1; 60s daemon samples,
30s settled-dashboard samples):

| Setup | Daemon | Dashboard | Combined |
|-------|--------|-----------|----------|
| Standard, daemon only | 19.5–20 / 6.1–7 MB · ~1% | — | same |
| Standard, daemon + dashboard | 20 / 7 MB · ~0.6% | 83.5 / 80.5 MB · ~0.5% | ~104 / ~87 MB · ~1% |
| Elevated, daemon only | 21.5 / 7.2 MB · ~0.8% | — | same |
| Elevated, daemon + dashboard | 21.5 / 7.2 MB · ~0.8% | 83 / 81 MB · ~0.5% | ~105 / ~88 MB · ~1.3% |

The daemon ticks every 2s (exe-name scan only, fetched once per PID); the
30s merged tick (WMI process query, both modes) is only a safety net
behind the 2s tick and instant WMI events. The dashboard repaints every
1s while open and costs nothing when closed (separate process, exits on X).

## Configuration

`%AppData%\ghelper-watcher\config.json` (created on first run):

```jsonc
{
  "watched": ["cs2.exe"],
  "default_profile": "Silent",
  "trigger_profile": "Turbo",
  "pause_minutes": 10,
  "switch_cooldown_secs": 60,
  "pause_on_battery": false,
  "paused_until_millis": null,
  "event_source": null,
  "run_as_admin": false,
  "daemon_elevated": null,
  "daemon_build": null,
  "last_switch_millis": null,
  "restart_request": false,
  "reopen_dashboard": false
}
```

Append-only debug log: `%AppData%\ghelper-watcher\watcher.log`.

## Requirements

- Windows 10/11 (x64), ASUS laptop with G-Helper installed and running.
- Administrator only for *instant* WMI events; everything else works standard.
  Elevation model: the **Run as administrator** checkbox is a sticky
  preference that never acts on its own. It takes effect on restart —
  tray **Restart elevated** item or dashboard **restart UAC** button
  (one UAC prompt) — and silently at boot via the logon task.

## Build

```powershell
$env:RUSTFLAGS = "-C target-cpu=native"; cargo build --release
```

Ship the binary as `G-Helper_Watcher.exe` (any file name works — boot
entries heal to whatever binary is running). Release profile:
`opt-level=3`, fat LTO, single codegen unit, `panic=abort`, stripped;
eframe pinned to the `glow` renderer without accesskit/wayland shims
(`wgpu` would roughly triple the dashboard's open RAM). First run enables
autostart automatically; toggle **Run as administrator** once (one UAC
prompt) for silent elevated autostart afterwards.

## Boot reliability

- **Self-healing entries** — a moved/renamed binary rewrites its own Run
  key / logon task (same level, current path) instead of booting a dead
  path. The dashboard shows the actual entry (`Task: …` / `Run: …` / `Off`).
- **Battery-proof, immortal task** — the logon task starts on battery and
  has no execution time limit (defaults would skip unplugged logons and
  kill the daemon after 72h).
- **Logon-safe tray** — the daemon waits for Explorer's taskbar before
  registering its icon (no phantom twin), retries instead of panicking,
  and drops the icon before restart exits (no ghosts).
- **One instance, silent** — named-mutex single instance, double tray
  presses collapse into one window, no console flash and no UAC at boot.
- **Honest limitation** — removing an elevated task needs elevation:
  press restart UAC (one prompt), then toggle off.

## License

MIT — see [LICENSE](LICENSE).
Copyright (c) 2026 Nițică Dragoș-Marian.

## Credits

- [G-Helper](https://github.com/seerge/g-helper) by seerge (GPL-3.0) —
  the tool this watcher drives, via its published hotkeys. No G-Helper code
  is included or modified here.
- [egui / eframe](https://github.com/emilk/egui) — immediate-mode GUI
- [tray-icon / muda](https://github.com/tauri-apps/tray-icon) — system tray
- [sysinfo](https://github.com/GuillaumeGomez/sysinfo) — process enumeration
- [wmi](https://github.com/jarapy/wmi-rs) — WMI queries and event subscriptions
- [notify](https://github.com/notify-rs/notify) — config file watching
- [winreg](https://github.com/gentoo90/winreg) — startup registry key
- [rfd](https://github.com/PolyMeilex/rfd) — native file picker
- [serde / serde_json](https://github.com/serde-rs/serde) — configuration
- [windows](https://github.com/microsoft/windows-rs) — native Windows APIs
- The Rust language and community.
