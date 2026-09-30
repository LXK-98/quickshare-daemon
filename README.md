# quickshare-daemon

A headless [Quick Share](https://www.android.com/better-together/quick-share/)
(Nearby Share) daemon for Linux. No window, no tray icon, no settings dialog:
a background service, a config file, and desktop notifications to accept or
deny incoming transfers.

**Status: it builds, runs, and is discoverable from other devices — but no
file has been received through it yet.** See [docs/JOURNAL.md](docs/JOURNAL.md)
for exactly where things stand and [PLAN.md](PLAN.md) for where they are going.

At idle it uses **6.2 MB RSS / 3.5 MB PSS across 2 threads**, against Packet's
44.7 MB / 42.4 MB across 30 ([docs/MEASUREMENTS.md](docs/MEASUREMENTS.md)).

## Why

[Packet](https://github.com/nozwock/packet) does this job well, but it is a
GTK4 application, and on this machine it idles at **44.7 MB RSS / 42.4 MB
PSS across 30 threads** — measured 2026-09-30, window closed. Nearly all of
that is GTK, the Vulkan/GL drivers it loads even while hidden, glibc's
per-thread malloc arenas, and the Flatpak sandbox. Very little of it is the
Quick Share protocol.

This daemon targets **≤ 8 MB idle RSS** by keeping the protocol and dropping
the interface. The protocol implementation is the same one Packet uses.

## Building

Requires a protobuf compiler, because `rqs_lib` generates the Nearby protocol
types at build time and `prost-build` does not bundle one:

```bash
sudo dnf install protobuf-compiler   # Fedora
cargo build --release
```

`libdbus` development files are also needed (`dbus-devel` on Fedora), for
`bluer`.

## Configuration

`$XDG_CONFIG_HOME/quickshare-daemon/config.toml`, written with defaults on
first run:

```toml
device_name = "dell"          # shown on other devices; defaults to the hostname
visible = true                # discoverable by everyone nearby
download_dir = "/home/you/Downloads"
# port = 45000                # omit for a random port
accept_timeout_secs = 60      # unanswered requests are declined after this
auto_accept_from = []         # sender names that skip the prompt
```

Reload with `systemctl --user reload quickshare-daemon`. An unknown key is a
startup error rather than a setting that silently does nothing.

`auto_accept_from` matches the name a sender advertises for itself, which
nothing verifies. It is a convenience, not an authorisation check.

## Credits and license

**GPL-3.0-or-later.** This is a requirement, not a preference: the daemon
adapts GPL-3.0 code.

- [Packet](https://github.com/nozwock/packet) by nozwock — the application this
  replaces, and the reference for the RQS service lifecycle, the inbound
  transfer handling and (later) the Nautilus extension. Adapted files say so in
  their headers.
- [rquickshare](https://github.com/Martichou/rquickshare) by Martichou — the
  Quick Share protocol implementation (`rqs_lib`), used via
  [nozwock's fork](https://github.com/nozwock/rquickshare).

Neither project is affiliated with this one, and neither is affiliated with
Google. "Quick Share" and "Nearby Share" are Google's trademarks.
