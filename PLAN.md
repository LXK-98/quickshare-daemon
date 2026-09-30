# quickshare-daemon — Plan

A headless Quick Share (Nearby Share) daemon for Linux with the lowest practical
memory footprint. It replaces the Packet GUI app for everyday use: no windows,
just a background service, a config file, notifications to accept/deny incoming
transfers, and a Nautilus "Send to" menu.

## Motivation

Packet (io.github.nozwock.Packet 0.7.1, Flatpak) idles at ~45 MB RSS / ~41 MB
PSS (measured 2026-09-30), plus ~4 MB for its two `bwrap` wrapper processes.
Breakdown of the running process:

- ~31 MB heap/anonymous memory: GTK4/libadwaita state and glibc's per-thread
  malloc arenas (30 threads)
- ~3 MB Vulkan/GL drivers (LLVM, nouveau, intel, lavapipe), loaded even though
  the window is hidden
- ~4 MB GTK/GLib/fonts, ~4 MB the Packet binary itself

Almost all of that comes from the GUI and the Flatpak sandbox, not from the
Quick Share protocol.

## Goals

- Receive files/text from Android devices; accept/deny via desktop notification
- Send files from Nautilus to a discovered nearby device
- Settings in a plain config file, no settings UI
- Idle memory: **≤ 8 MB RSS** (target ~4–6 MB)
- Run natively as a systemd user service (no Flatpak)

### Non-goals

- Any GUI window, tray icon, or settings dialog
- Translations
- Flatpak packaging
- Socket activation / 0 MB idle (possible later, see Phase 5)

## Key decision: new application, not a fork of Packet

Packet is ~5,600 lines of Rust, almost all UI. The RQS lifecycle is tangled
into `src/window.rs` (2,702 lines) with GTK widgets and GObject properties, and
the transfer handling lives in `src/widgets/receive_transfer.rs` (831 lines) and
`src/widgets/recipient_card.rs` (567 lines). A fork would delete or rewrite
nearly everything, so upstream merges would always conflict.

The protocol lives in `rqs_lib` (nozwock's fork of rquickshare), which has no
GUI. We depend on it directly and get upstream protocol fixes by updating the
git revision.

- Depend on `rqs_lib` from `https://github.com/nozwock/rquickshare.git`, pinned
  to rev `17702edc9f8b0d1e67c50f125e03ca3f8c291f3c` (the rev Packet pins), with
  `default-features = false, features = ["experimental"]`. Corrected
  2026-09-30: this plan originally said `c9ac93f...` "(same as Packet)", but
  that is upstream HEAD, not what Packet is built against — see
  `docs/DECISIONS.md` D-001
- Use Packet (commit `f634d66`) as reference code; copy/adapt the RQS
  lifecycle from `src/window.rs` (`setup_rqs_service`), the transfer handling
  from `src/widgets/receive_transfer.rs` and `src/widgets/recipient_card.rs`,
  and the Nautilus extension from
  `data/resources/plugins/packet_nautilus.py.in`. `f634d66` already contains
  the network-restart logic Phase 2 needs
- **License: GPL-3.0** (required, since we adapt GPL-3.0 code). Credit Packet
  and rquickshare in the README and in headers of adapted files
- If `rqs_lib` ever needs patching (e.g. socket activation), fork `rqs_lib`,
  not Packet

## Architecture

```
┌─────────────────────────── quickshare-daemon (systemd --user) ───────────────┐
│  config.toml ─► Config                                                        │
│                                                                               │
│  rqs_lib::RQS  ◄── start/stop/restart ── Supervisor ◄── NetworkManager signal │
│   (TCP listener, mDNS, BLE)                  │                                │
│        │ transfer events                     ▼                                │
│        └──────────────► Transfer handler ──► Notifications (D-Bus)            │
│                               ▲               accept / deny / done / failed   │
│                               │                                               │
│  D-Bus API  (org.quickshare.Daemon1)                                          │
│   ListDevices(), SendFiles(device, paths), Visibility property                │
└───────────────────────────────▲──────────────────────────────────────────────┘
                                │ session bus
                 Nautilus Python extension: "Send to ▸ <device>"
```

- **Runtime:** tokio `current_thread` (Packet uses `rt-multi-thread`). Fewer
  threads mean fewer malloc arenas and stacks
- **D-Bus:** `zbus` for our API, notifications and NetworkManager
- **Notifications:** `org.freedesktop.Notifications.Notify` with `accept` /
  `deny` actions; listen for `ActionInvoked` and `NotificationClosed`.
  Auto-deny after a timeout (configurable). GNOME Shell draws them, so the
  daemon pays nothing for UI
- **Network changes:** watch NetworkManager `StateChanged` over D-Bus (replaces
  GIO's NetworkMonitor in Packet). Restart RQS on reconnect, deferred until any
  active transfer finishes (Packet does the same)
- **Logging:** `tracing` to stderr → journald. Drop Packet's rolling log files

## Config file

`$XDG_CONFIG_HOME/quickshare-daemon/config.toml`, created with defaults on first
run. Covers the Packet GSettings keys that still matter (`device-name`,
`device-visibility`, `download-folder`, `enable-static-port`,
`static-port-number`); the window, tray, autostart and plugin keys are dropped.

```toml
device_name = "alex-laptop"        # default: hostname
visible = true                     # visible to everyone nearby
download_dir = "~/Downloads"       # default: XDG download dir
# port = 45000                     # omit for a random port
accept_timeout_secs = 60           # auto-deny unanswered requests
# auto_accept_from = ["Pixel 9"]   # optional, later
```

Reload on `SIGHUP` (or `systemctl --user reload`); restart RQS only if name,
visibility or port changed.

## Sending from Nautilus

Picking a recipient is the only real UI. Plan: adapt Packet's Nautilus Python
extension (needs `nautilus-python`, which Packet's plugin already requires):

- Selected files → "Send to ▸" submenu listing devices from `ListDevices()`
- Discovery runs only while needed: the extension calls `StartDiscovery()` when
  the menu is built; the daemon stops discovery after ~60 s idle
- Choosing a device calls `SendFiles(device_id, paths)`; progress/result shown
  as notifications
- Fallback if the submenu is awkward (devices appear after the menu opens): a
  single "Send via Quick Share…" item that opens a `zenity --list` picker

The extension runs inside Nautilus, not the daemon, so it adds nothing to the
daemon's footprint.

## Phases

### Phase 1 — Receive-only MVP (~1.5–2 days)

- [x] Cargo project, GPL-3.0 license, README with credits
- [x] Config loading with defaults (`serde` + `toml`) — unit-tested
- [ ] Start `rqs_lib::RQS` with name/visibility/port from config — **written,
      never executed.** Type-checked only against a stub of `rqs_lib`; the real
      crate cannot be built until `protoc` is installed
- [ ] Handle inbound transfer events: request → notification → accept/deny —
      **half verified.** The notification half is confirmed against live GNOME
      Shell 50.4 (prompt, buttons, timeout, close); the `rqs_lib` half is
      unexecuted, as above
- [ ] Save files to `download_dir`; text/URL payloads shown in a notification
      (with an "Open"/"Copy" action) — text, URL and Wi-Fi payloads are shown;
      **the "Open"/"Copy" actions are not implemented.** Open needs a scheme
      allowlist before handing a remote-supplied URL to `xdg-open`, and Copy
      needs a Wayland clipboard path, which a daemon with no window does not
      have for free. Deliberately deferred, not forgotten
- [ ] Completion/failure notifications — written, not exercised by a real
      transfer
- [x] systemd user unit (`quickshare-daemon.service`) — written, not yet
      installed or tested
- [ ] **Checkpoint:** measure idle RSS/PSS, test with an Android phone —
      blocked on `protoc`

### Phase 2 — Robustness (~0.5–1 day)

- [ ] Restart RQS on network changes (NetworkManager D-Bus), deferred during
      transfers
- [ ] Accept timeout, cancelled transfers, sender disconnects mid-transfer
- [ ] Filename collisions in `download_dir`
- [ ] `SIGHUP` config reload

### Phase 3 — Sending (~1–2 days)

- [ ] D-Bus API: `StartDiscovery`, `ListDevices`, `SendFiles`, `Visibility`
- [ ] Outbound transfers with progress/result notifications
- [ ] Nautilus extension with device submenu (or zenity fallback)
- [ ] Small CLI: `quickshare-daemon send <files>` / `devices` for scripting

### Phase 4 — Memory tuning & packaging (~0.5 day)

- [ ] Release profile: `lto = true`, `codegen-units = 1`, `opt-level = "s"`,
      `panic = "abort"`, `strip = true`
- [ ] `MALLOC_ARENA_MAX=2` in the unit; try `malloc_trim` after transfers
- [ ] Check which threads rqs_lib, mdns-sd and the BLE stack spawn; trim if
      possible
- [ ] Measure again; compare with Packet
- [ ] Install script or `cargo install` + unit/extension install

### Phase 5 — Optional: ~0 MB idle via socket activation (~1–2 days)

- Patch (fork) `rqs_lib` to accept a listening socket fd from systemd instead of
  binding its own
- Advertise `_FC9F5ED42C8A._tcp` with a static Avahi service file; disable
  rqs_lib's mDNS
- Daemon exits after an idle timeout
- Caveat: loses BLE-based wake-up of hidden receivers. Only worth it if 4–6 MB
  always-on is still too much

## Memory targets

| Setup                                          | Idle RSS      |
|------------------------------------------------|---------------|
| Packet today (Flatpak, GTK4)                   | ~45 MB (+4 MB bwrap) |
| This daemon, multi-thread runtime              | ~6–10 MB      |
| This daemon, `current_thread` + tuning         | ~4–6 MB       |
| Phase 5 socket activation                      | ~0 MB idle    |

Measure with `/proc/<pid>/smaps_rollup` (Pss, Private_Dirty) and `VmRSS` from
`/proc/<pid>/status`.

## Risks

- **rqs_lib API stability:** it's marked `experimental` and has no stable API;
  pin the rev and update deliberately
- **Transfer edge cases:** cancels, network restarts mid-transfer, and Android
  version quirks are where most debugging time will go
- **BLE:** `bluer`/`btleplug` may add threads and memory; check whether BLE is
  needed for our use or can be feature-gated off
- **Nautilus menu timing:** devices discovered after the menu is built won't
  appear until it's reopened (hence the zenity fallback)

## References

- Packet: https://github.com/nozwock/packet (reference commit `f634d66`)
- rqs_lib fork: https://github.com/nozwock/rquickshare (rev `17702ed`)
- Upstream rquickshare: https://github.com/Martichou/rquickshare
- Desktop notifications spec:
  https://specifications.freedesktop.org/notification-spec/latest/

## Documentation

Kept current as work proceeds; see `docs/`:

- `docs/DECISIONS.md` — why each non-obvious choice was made, and what was
  rejected. D-001 the `rqs_lib` pin, D-002 the BLE feature, D-003 the runtime
  flavour, D-004 the build order, D-005 ownership of `RQS`
- `docs/RQS_LIB_NOTES.md` — the `rqs_lib` API as actually observed in the
  pinned checkout, since none is published. Re-verify when the pin moves
- `docs/MEASUREMENTS.md` — dated RSS/PSS/thread numbers with the command used
- `docs/JOURNAL.md` — per-session progress, what is verified, what is blocked

## Build prerequisites

Discovered 2026-09-30, not previously in this plan:

- **`protoc`** must be on `PATH`. `rqs_lib`'s build script runs `prost_build`
  and `.unwrap()`s it, and `prost-build` 0.13.5 bundles no compiler, so a
  missing `protoc` is an opaque build-script panic. `sudo dnf install
  protobuf-compiler` (Fedora 44 ships 3.19.6, which is new enough)
- **`libdbus` headers** (`dbus-devel`), for `bluer`
