# quickshare-daemon

[![ci](https://github.com/LXK-98/quickshare-daemon/actions/workflows/ci.yml/badge.svg)](https://github.com/LXK-98/quickshare-daemon/actions/workflows/ci.yml)
[![license: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)

A headless [Quick Share](https://www.android.com/better-together/quick-share/)
(Nearby Share) daemon for Linux. No window, no tray icon, no settings dialog:
a background service, a config file, desktop notifications to accept or deny
incoming transfers, and a `quickshare-daemon send` command going the other
way.

It exists because the desktop clients for this protocol are GUI applications
that must stay running to receive anything, and a GTK process idling at 45 MB
is a lot of memory to spend on waiting for a file.

|                     | quickshare-daemon | [Packet](https://github.com/nozwock/packet) |
| ------------------- | ----------------: | ------------------------------------------: |
| Idle RSS            |        **6.2 MB** |                                      44.7 MB |
| Idle PSS            |        **3.5 MB** |                                      42.4 MB |
| Threads             |         **2**     |                                           30 |

Measured on Fedora with both idle and nothing transferred; method and raw
numbers in [docs/MEASUREMENTS.md](docs/MEASUREMENTS.md).

## Status

Sending and receiving both work and are tested against an Android phone over
Wi-Fi.

| Feature                                         | State |
| ----------------------------------------------- | ----- |
| Receive files, with an accept/decline prompt     | ✅ |
| Send files from the CLI                          | ✅ |
| Open / show-in-files buttons on the notification | ✅ |
| D-Bus API (`org.quickshare.Daemon1`)             | ✅ |
| Nautilus “Send with Quick Share” menu            | ✅ (optional) |
| Transfers without a shared Wi-Fi network         | ❌ — see [What does not work](#what-does-not-work) |

This is a personal project at version 0.1.0. The protocol implementation it
builds on is marked experimental upstream, and the on-disk config and the
D-Bus API may still change.

## Installing

### Fedora

```bash
sudo dnf copr enable LXK-98/quickshare-daemon
sudo dnf install quickshare-daemon
systemctl --user enable --now quickshare-daemon
```

Updates then arrive with `dnf upgrade` like anything else. The Nautilus menu
is a separate package: `sudo dnf install quickshare-daemon-nautilus`.

### Any distribution

```bash
curl -fsSLO https://github.com/LXK-98/quickshare-daemon/releases/latest/download/install.sh
less install.sh      # read it before running it
sh install.sh
```

Installs to `~/.local/bin`, adds a systemd user unit, and adds the Nautilus
menu if `nautilus-python` is present. It verifies a published SHA-256 before
unpacking. **Re-run it to update**; `sh install.sh --uninstall` removes it,
leaving your config and received files alone.

The published binary needs glibc 2.35 or newer (Ubuntu 22.04, Fedora 36,
Debian 12 and later). Older systems can build from source.

#### There is no auto-update

Deliberately. Adding one would mean an HTTP and TLS stack in a binary whose
whole point is its footprint, and a background daemon that overwrites itself
is security surface that needs signing to do properly — for something your
package manager already does. See
[docs/DECISIONS.md](docs/DECISIONS.md) D-017.

### From source

```bash
sudo dnf install protobuf-compiler dbus-devel          # Fedora
sudo apt install protobuf-compiler libdbus-1-dev       # Debian/Ubuntu
cargo build --release
```

A protobuf compiler is required because `rqs_lib` generates the Nearby
protocol types at build time and `prost-build` does not bundle one. The
`libdbus` headers come in through `bluer`, which `rqs_lib` depends on.

The binary lands at `target/release/quickshare-daemon`. To run it as a
service, copy [`data/quickshare-daemon.service`](data/quickshare-daemon.service)
to `~/.config/systemd/user/`.

## Using it

### Receiving

Start the daemon and it is discoverable:

```bash
systemctl --user start quickshare-daemon
```

When a phone sends you something, a notification asks you to accept it. Accept
and the file lands in your download directory, with **Open** and **Show in
Files** buttons on the notification that follows.

### Sending

```console
$ quickshare-daemon devices
Looking for nearby devices for 8s…
NAME                      VIA         ADDRESS                 ID
Pixel 7                   wifi_lan    192.168.1.24:38217      aB3dEf0g

$ quickshare-daemon send "Pixel 7" photo.jpg notes.pdf
Sending 2 files (4.1 MB) to Pixel 7…

  ACCEPT ON PIXEL 7 — PIN 4517
  (check the device shows the same PIN)
Sent to Pixel 7.
```

`<to>` is a device name or the id from `devices`. `send` waits up to 45
seconds for the device to appear, because a phone only advertises itself while
its Quick Share sheet is open and nobody can hold that open while typing a
command.

Both commands talk to the running daemon over D-Bus when there is one, and
start their own short-lived service when there is not — so they work either
way, but reuse the daemon's discovery when it is available.

### Nautilus menu

An optional “Send with Quick Share” menu for Nautilus, listing nearby devices
from the running daemon. The daemon, the CLI and the D-Bus API all work
without it.

```bash
sudo dnf install nautilus-python     # required at runtime, not just to build
mkdir -p ~/.local/share/nautilus-python/extensions
cp data/quickshare_nautilus.py ~/.local/share/nautilus-python/extensions/
nautilus -q                          # restart Nautilus to load it
```

`nautilus-python` is what lets Nautilus load Python extensions at all; without
it the file is inert. It is **not** a development-only package — anyone using
the menu needs it. It is small (about 58 KB) and its only substantial
dependency, `python3-gobject-base`, is already present on any GNOME install.

Known devices show as a submenu. **“Other device…” opens a picker that waits
up to 45 seconds** for one to appear, which is the usual path the first time:
Nautilus builds the menu synchronously, while a phone only starts advertising
once its Quick Share screen is open. Open Quick Share on the other device
first and the submenu will already be populated.

With no daemon running the menu does not appear at all, rather than appearing
and failing.

## Configuration

`$XDG_CONFIG_HOME/quickshare-daemon/config.toml`, written with defaults on
first run:

```toml
device_name = "laptop"        # shown on other devices; defaults to the hostname
visible = true                # discoverable by everyone nearby
download_dir = "/home/you/Downloads"
# port = 45000                # omit for a random port
accept_timeout_secs = 60      # unanswered requests are declined after this
auto_accept_from = []         # sender names that skip the prompt
```

Reload with `systemctl --user reload quickshare-daemon` (or send `SIGHUP`).
An unknown key is a startup error rather than a setting that silently does
nothing.

`auto_accept_from` matches the name a sender advertises for itself, which
nothing verifies. It is a convenience, not an authorisation check.

Logging goes to stderr and is filtered with `RUST_LOG`:

```bash
RUST_LOG=quickshare_daemon=debug,rqs_lib=debug quickshare-daemon
```

## D-Bus API

The daemon owns `org.quickshare.Daemon1` on the session bus at
`/org/quickshare/Daemon1`. This is how the CLI and the Nautilus extension talk
to it, and it is a supported way to script it.

| Member | Signature | Notes |
| ------ | --------- | ----- |
| `StartDiscovery()` | `() → ()` | Reference-counted: discovery runs while at least one client holds a lease, and a lease is released when the client drops off the bus. |
| `StopDiscovery()` | `() → ()` | Releases this client's lease. |
| `ListDevices()` | `() → aa{sv}` | One dict per device. |
| `SendFiles(target, paths)` | `(s, as) → s` | Returns a transfer id. |
| `Visible` | `b`, read/write | Toggles discoverability; writing it restarts the service. |
| `DevicesChanged` | signal `()` | The device list changed; call `ListDevices` again. |
| `TransferFinished` | signal `(s id, b succeeded, s error)` | |
| `TransferPin` | signal `(s id, s pin)` | The PIN to confirm on the other device. |

Each device dict carries `id` (`s`), `name` (`s`), `reachable` (`b`),
`addresses` (`a{ss}`, medium → address) and `medium` (`s`, the one the daemon
would actually use).

```bash
busctl --user call org.quickshare.Daemon1 /org/quickshare/Daemon1 \
  org.quickshare.Daemon1 ListDevices
```

The shape and its reasoning are in [docs/DECISIONS.md](docs/DECISIONS.md)
D-013.

## What does not work

**Transfers need both devices on the same network.** Android can fall back to
Bluetooth and to a peer-to-peer Wi-Fi link when there is no shared network;
this daemon cannot, and the reason is not something a patch here fixes.

The short version: the phone's own Bluetooth advertisement for Quick Share is
visible, and a Bluetooth connection to it can be opened — a packet capture
from a Pixel confirms both. What is missing is the identity half. The offline
path authenticates against Google account state that a Linux client has no way
to present, so the phone has nothing to match a connection attempt against.
The full investigation, including the HCI captures, is in
[docs/BLUETOOTH_FINDINGS.md](docs/BLUETOOTH_FINDINGS.md).

Smaller limitations:

- The other device must have its Quick Share sheet open to be discoverable.
  That is how the protocol works, not a bug here.
- Only Nautilus has a file-manager menu.
- Wayland and X11 are both fine, but the notification buttons need a
  notification daemon that implements actions — every mainstream one does.

## Documentation

This project is documented as it was built, not after the fact.

| Document | What is in it |
| -------- | ------------- |
| [docs/DECISIONS.md](docs/DECISIONS.md) | Every non-obvious choice, numbered, with what was rejected and why. |
| [docs/JOURNAL.md](docs/JOURNAL.md) | Chronological log of the work, including the things that did not pan out. |
| [docs/MEASUREMENTS.md](docs/MEASUREMENTS.md) | Memory numbers and how to reproduce them. |
| [docs/RQS_LIB_NOTES.md](docs/RQS_LIB_NOTES.md) | Notes on the protocol library, its API and its bugs. |
| [docs/BLUETOOTH_FINDINGS.md](docs/BLUETOOTH_FINDINGS.md) | Why the offline transport does not work, with captures. |
| [docs/TRANSPORT_IDENTITY.md](docs/TRANSPORT_IDENTITY.md) | Design note on separating device identity from address. |
| [docs/EXTENSION_PLAN.md](docs/EXTENSION_PLAN.md) | Plan for the file-manager integration. |
| [PLAN.md](PLAN.md) | The original build plan, with its status. |

## Credits and license

**GPL-3.0-or-later.** This is a requirement, not a preference: the daemon
adapts GPL-3.0 code.

- [Packet](https://github.com/nozwock/packet) by nozwock — the application
  this replaces, and the reference for the RQS service lifecycle, the inbound
  transfer handling and the Nautilus extension. Adapted files say so in their
  headers.
- [rquickshare](https://github.com/Martichou/rquickshare) by Martichou — the
  Quick Share protocol implementation (`rqs_lib`), by way of
  [nozwock's fork](https://github.com/nozwock/rquickshare).

`rqs_lib` is used from [a further
fork](https://github.com/LXK-98/rquickshare), branched at the revision Packet
is tested against and carrying three fixes: P-256 coordinate padding
(DECISIONS D-012), the `Disconnection` frame being sent after `Finished` is
announced rather than before (D-014), and a peer's ordinary post-transfer
hangup being logged as an error. The revision is pinned in `Cargo.toml`.

Neither project is affiliated with this one, and neither is affiliated with
Google. “Quick Share” and “Nearby Share” are Google's trademarks.
