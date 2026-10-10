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

## Dashboard

- **Status card** — live state (Active / Paused countdown with Pause /
  Resume button), G-Helper mode, watched-running count plus first running
  app, switch cooldown, event source (WMI-instant vs poll), daemon
  privilege, Start G-Helper button when G-Helper is missing.
- **Profiles** — Default and Trigger tiles, with a Settings button.
- **Manage window** — side window with the tracked-apps list (click a row
  to stop tracking it), type-to-filter, exact-name entry, `.exe` browser,
  plus settings: pause duration, switch cooldown, Start with Windows,
  Run as Administrator. A **restart UAC** button next to the
  Events row restarts elevated on demand.

## Resource usage

Working set / private MB and avg CPU (% of one core), release build,
16-core machine, measured 2026-10-10:

| Setup | Daemon | Dashboard | Combined |
|-------|--------|-----------|----------|
| Standard, daemon only | 20.5 / 7.2 MB · ~2% | — | same |
| Standard, daemon + dashboard | 21.3 / 8.1 MB · ~2% | 85 / 83 MB · ~1% | ~106 / ~91 MB · ~3% |
| Elevated, daemon only | 20.2 / 7.3 MB · ~2% | — | same |
| Elevated, daemon + dashboard | 20.7 / 7.6 MB · ~2% | 85 / 83 MB · ~1% | ~106 / ~91 MB · ~3% |

The daemon ticks every 2s (~2–6% blips); the 15s merged tick spikes to
~10% once (WMI process query, both modes). The dashboard repaints every
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
  "paused_until_millis": null,
  "event_source": null,
  "run_as_admin": false,
  "daemon_elevated": null,
  "daemon_build": null,
  "last_switch_millis": null,
  "restart_request": false
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
cargo build --release
```

The binary lands at `target\release\ghelper-watcher.exe`. First run enables
autostart automatically; toggle **Run as administrator** once (one UAC
prompt) for silent elevated autostart afterwards.

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
