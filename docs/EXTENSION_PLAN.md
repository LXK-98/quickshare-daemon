# Possible GNOME Shell extension

Not committed to, and deliberately **after** the original goals in PLAN.md.
Written down now because the constraints below are what shape the Phase 3
D-Bus API, and getting them wrong there is expensive to undo later.

## The constraint that decides the shape

GNOME Shell extensions are **GJS only**, running inside the `gnome-shell`
process. No Rust goes in there.

That rules out moving the daemon into an extension: `rqs_lib` — UKEY2 key
exchange, AES, protobuf, mDNS, BLE — cannot run in GJS, and reimplementing the
Quick Share protocol in JavaScript would discard the best decision in this
project (depending on `rqs_lib` so upstream protocol fixes arrive via a rev
bump, D-001).

**So the only sane shape is the one PLAN.md already draws:** the daemon is the
engine, and an extension is one more D-Bus client beside the Nautilus extension
and the CLI. Three front-ends, one daemon.

## Two costs that are easy to miss

**A fault takes the desktop with it.** An uncaught exception or a leak in an
extension degrades the whole session. The daemon's equivalent failure is a
service systemd restarts. This is a materially worse failure mode for the same
functionality.

**Memory inside `gnome-shell` is not free, only unattributed.** This project
exists because of a measured number. Moving cost into a process we do not
measure would be quietly cheating — an extension holding JS objects in the
Shell is not 0 MB, it just stops appearing in our figures.

A third cost, less subtle: GNOME breaks extension APIs roughly every six-month
release. That is a recurring compat tax, and it sits badly beside PLAN.md's
non-goals, which cut translations and Flatpak specifically to avoid maintenance
burden.

## What it would actually buy

It fixes the weak spot PLAN.md admits to: *"Picking a recipient is the only
real UI"*, with a `zenity --list` fallback for when the Nautilus submenu gets
awkward. A Quick Settings device picker beats that. A Quick Settings toggle for
visibility beats editing `config.toml` and sending SIGHUP.

Receiving needs nothing. Notifications already work, verified against GNOME
Shell 50.4.

## Sequencing

1. Phase 3's D-Bus API (needed regardless).
2. The Nautilus extension — it is the stated goal in PLAN.md.
3. **Then** decide, having used the Nautilus picker in daily life, whether it
   is annoying enough to justify the extension and its compat tax.

## Three API decisions this imposes, to make now

Free today, painful to retrofit.

1. **Signals, not just methods.** `TransferStarted` / `TransferProgress` /
   `TransferFinished`, plus a correct `PropertiesChanged` on `Visibility`. A
   polling client is bad; a polling client inside `gnome-shell` is worse.
2. **`ListDevices` returns `a{sv}` dicts, not fixed tuples.** Fields can then
   be added without breaking existing clients — and with three front-ends,
   fields will be added.

   Corrected 2026-10-01: an earlier version of this said to expose "stable
   device ids". **There are none to expose.** `rqs_lib` sets
   `EndpointInfo.id` to `"ip:port"`, which changes every time a phone
   re-advertises on a fresh port. A client must therefore not cache a device id
   across a discovery session, and the daemon has to deduplicate by address or
   it will list the same phone twice. See docs/RQS_LIB_NOTES.md.
3. **Discovery must be refcounted, not a global on/off.** With two clients a
   toggle breaks immediately: the Nautilus menu closes, calls
   `StopDiscovery`, and kills the device list the extension still has open.
   Hand out per-client leases and release them when the client drops off the
   bus (`zbus` can watch `NameOwnerChanged`).

Point 3 also settles D-005's open question: `zbus` handlers are separate entry
points that would each need `&mut RQS`, so they must pass commands into the
existing event loop over a channel rather than share the object. Multiple
clients make that certain rather than merely likely.

## Sketch, if it is built

- `quickshare@`<domain>, `metadata.json` declaring the `shell-version`s tested
- A Quick Settings toggle bound to the daemon's `Visibility` property
- A device list populated from `ListDevices` and kept current by the signals
- Transfer progress as a panel indicator, or just leave it to notifications,
  which already work and cost nothing
- Drag-and-drop onto the indicator would be the one thing no other front-end
  can offer. Wayland makes it non-trivial and it should not be in v1

Roughly 300–600 lines of GJS; one to three days for someone fluent, more while
learning. The D-Bus API is the bulk of the work and is not extra.

## Worth re-checking before building

Nothing share-related exists on `org.gnome.Shell` on GNOME 50.4 (checked
2026-10-01), so there is no native Quick Share to be obsoleted by today. If
GNOME ever ships one, every bit of UI work here evaporates while the daemon
stays useful — which is one more argument for keeping the engine separate.
