# Journal

Per-session record: what was done, what is verified, what broke, what is next.
Newest first.

## 2026-10-01 — Local cleanup, and two bugs it turned up

A pass over local state after the GitHub remote was removed. Two of the six
items were real bugs, both found by reading `rqs_lib` rather than by testing.

**Bug: the "Open" button could open the wrong file.** `rqs_lib` resolves
filename collisions itself (`lorem.txt` → `lorem (1).txt`) but reports the
**original** name in the transfer metadata, not the one it wrote. Joining the
reported name with `download_dir` therefore pointed at the *pre-existing* file
whenever a collision happened — so receiving a second `IMG_1234.jpg` and
pressing Open would have shown the first one. `open::resolve_received` now picks
the newest of the candidates `rqs_lib` could have chosen. It is a heuristic,
reliable only because the file was written seconds earlier; the real fix belongs
upstream, in reporting the path actually written.

**Bug: any argument was silently ignored.** `quickshare-daemon --help` started
the daemon and bound a port. So would `--visible`, or any typo. Now `--help`
and `--version` do what they should and an unknown argument exits 2 without
starting a network service. Found by running `--help` to check the install,
which started a daemon instead.

**Also fixed.**

- Removed the dead `origin` remote, so git operations no longer point at a
  repository that is being deleted.
- `~/.config/quickshare-daemon/config.toml` still had
  `device_name = "dell.fritz.box"` from before the FQDN fix — the file is
  written once, so fixing the default did not fix the existing file. Now `dell`.
- `cargo install --path .` so the systemd unit's `ExecStart`
  (`%h/.cargo/bin/quickshare-daemon`) resolves. It previously pointed at
  nothing.
- Removed a stray `/tmp/qsd-final.log` left by an earlier test run.

**Resolved, with no work needed:** PLAN.md's Phase 2 item for filename
collisions. `rqs_lib` already handles it, in the conventional way, and anything
we added would race with it. Documented in RQS_LIB_NOTES.md.

**Corrected an earlier claim of mine.** I had flagged the `LICENSE` file's
`<year> <name of author>` placeholders as something to fill in for GitHub's
license detection. They are part of the GPL's own "How to Apply These Terms"
appendix, which is meant to ship verbatim — filling them in would be editing
the license text. Nothing to fix. GitHub's "not detected" on a freshly created
repository was almost certainly indexing lag.

Clippy clean, 25/25 tests.

---

## 2026-09-30 — Receiving confirmed from Android; Open / Show in Folder buttons

**A file was sent successfully from an Android phone.** That closes the last
open item in Phase 1's checkpoint. Reported by the user rather than observed
here — see the caveat at the end of this entry.

**Added, on request:** the notification for a completed file transfer now
carries **"Open"** and **"Show in Folder"** buttons.

- `src/open.rs` — `reveal()` via `org.freedesktop.FileManager1.ShowItems`,
  which opens the folder with the files *selected* and takes several URIs at
  once (D-011), falling back to `xdg-open` on the directory if no file manager
  implements it. `open_file()` via `xdg-open`, refusing extensions that would
  execute rather than display (D-010).
- `src/notify.rs` — the signal-waiting logic behind `ask_consent` extracted
  into `await_action`, so button-carrying notifications are one primitive
  rather than two copies. `ask_consent` is now a thin mapping over it.
- `src/main.rs` — completed file transfers post their notification from a
  spawned task with a one-hour button lifetime, since the user may be away from
  the machine when a file lands.

Two details that came out of reading the source rather than guessing:
`rqs_lib` reports **bare file names, not paths**, so they must be joined with
`download_dir`; and `file://` URIs are percent-encoded by hand because received
names routinely contain spaces, `#` and non-ASCII, any of which silently
corrupts a naively formatted URI.

**Verified.** Clippy clean, 21/21 tests. `org.freedesktop.FileManager1` is
present on this system (Nautilus), and `reveal()` was called against it for
real with two URIs at once — the multi-file selection path — and accepted, so
the D-Bus signature is right. That was worth checking directly: a wrong
signature is a runtime error that unit tests cannot see and that would make
the button fail silently.

The notification-with-buttons path itself is the same `await_action` code
already confirmed twice by hand on the consent prompt (Accept at 31.3 s,
Decline at 8.8 s). A later attempt to have the new buttons clicked timed out
unanswered after 90 s, so "Open" and "Show in Folder" have not been pressed by
a human — only the code underneath them has been exercised.

**Caveat on the Android test.** No file newer than 20:56 exists in
`~/Downloads`, and this daemon first ran at about 21:09 — so the successful
transfer cannot be attributed to it from the filesystem alone. Packet was also
running and advertising throughout. Worth redoing once with the daemon in the
foreground and Packet stopped, watching for the consent prompt and the
`transfer finished` log line, before treating end-to-end receiving as proven.

---

## 2026-09-30 (milestone) — It builds, it runs, it hits the memory target

`protoc` was installed, which unblocked everything.

**It compiled on the first attempt**, with no changes to code that had only
ever been checked against a hand-written stub. The stub approach in
RQS_LIB_NOTES.md was worth the time.

**It runs.** Config written on first start, notification server reached, TCP
listener bound, mDNS broadcasting, BLE listener started, NetworkManager
watched, clean SIGTERM shutdown that cancels every task and unregisters the
mDNS service.

**It is discoverable.** `avahi-browse -rpt _FC9F5ED42C8A._tcp` sees it on every
interface at the port it logged, device name in the TXT record. An Android
phone was advertising the same service type on the same network.

**It hits the target.** Release build, idle, measured from `/proc` externally:

| | RSS | PSS | PrivDirty | Threads |
|---|---|---|---|---|
| Packet | 44.7 MB | 42.4 MB | 34.6 MB | 30 |
| this | 6.2 MB | 3.5 MB | 0.8 MB | 2 |

PLAN.md asked for ≤ 8 MB. Clippy clean, 17/17 tests, now against the real
`rqs_lib`.

**`MALLOC_ARENA_MAX=2` measured as a no-op**, three runs each way. The first
comparison appeared to show a 4.5× private-dirty win; it was an outlier from a
process still starting, and an empty `MALLOC_ARENA_MAX=` is not the same as
unset. Both mistakes were mine and both would have gone into the docs as fact
if the run had not been repeated.

**Fixed:** the default device name was the FQDN (`dell.fritz.box`), which is
what a phone would have shown on its share sheet. Now the short hostname. An
existing `config.toml` keeps the old value — delete it to regenerate.

**The one thing left in Phase 1:** sending an actual file from a phone. It
needs a human holding one.

---

## 2026-09-30 (later still) — Network watcher and SIGHUP reload

**Done.** Both of Phase 2's items that do not depend on `rqs_lib` being
buildable.

- `src/network.rs` — NetworkManager `StateChanged` watcher: state mapping,
  down → up edge detection, 3 s debounce, cancel-safe across `select!`. D-009.
- `src/main.rs` — the reconnect branch and a SIGHUP branch that reloads the
  config, applies `download_dir` in place through `rqs_lib`'s setter, and
  restarts the service only when `needs_service_restart` says so.
- Restarts are **deferred** while transfers are in flight and run the moment
  the last one ends, via a `restart_pending` flag.

**Verified.** `src/network.rs` run against the live system bus, read-only:
connects, reads `ConnectedGlobal`, and reports no spurious reconnect on a
stable link over 5 s. Clippy clean, 16/16 tests.

**Not verified.** The down → up transition and the debounce, which would mean
taking the network down — not something to do to a working machine unasked.
The SIGHUP path cannot run at all until the daemon can start.

**Two bugs caught while writing this, both mine.** `settle()` had a
non-exhaustive inner match and lost a reconnect if `select!` dropped the future
mid-debounce (fixed with `pending_reconnect`). And the first cut of
`restart_service` *refused* a restart during a transfer rather than deferring
it, which is weaker than what PLAN.md asks for; rewritten to actually defer.

---

## 2026-09-30 (later) — Config and notifications, verified for real

**Done.**

- `src/config.rs` — the `config.toml` from PLAN.md: serde + toml, defaults on
  first run with a commented header, `deny_unknown_fields` so a typo is a
  startup error rather than a setting that silently does not apply, validation
  of the values that would otherwise fail somewhere less obvious, and
  `needs_service_restart` stating which keys need the service restarted (the
  SIGHUP path in Phase 2 consumes it).
- `src/notify.rs` — `org.freedesktop.Notifications` over `zbus`: the consent
  prompt with Accept/Decline buttons, the timeout, completion and failure
  notifications, and `format_bytes`.
- `src/main.rs` rewritten to use both. Consent prompts are spawned off the
  event loop (D-008); auto-accept from config skips the prompt but still posts
  a notification; text, URL and Wi-Fi payloads each get their own completion
  message; failures are announced only for transfers that had been accepted.
- Decisions D-006 (raw notification spec over the XDG portal), D-007 (`zbus`
  on the tokio reactor, not async-io), D-008 (what gets spawned and why).

**Verified — against the live session bus, no stub involved.** `src/notify.rs`
was compiled directly into a test harness and run against GNOME Shell 50.4:

- `GetCapabilities` → `actions`, `body`, `body-markup`, `icon-static`,
  `persistence`, `sound`. So `supports_actions()` is true here.
- `Notify` returns a server id; `CloseNotification` works.
- A consent prompt nobody answers returns `TimedOut` after exactly its timeout
  and closes itself.
- A prompt answered by a human resolved in 8.8 s. It returned `Decline`, which
  is correct for both the Decline button and a swipe-away, and logging was off
  in that run, so which of the two happened is unknown.

**Also verified, via the stub:** `cargo check --all-targets` and
`cargo clippy --all-targets` clean, 14/14 tests passing across all four
modules.

`format_bytes` had a bug the tests caught: the unit table stopped at TB, so
`u64::MAX` formatted as "18446744 TB". Extended to PB and EB.

**Still blocked.** `protoc`. Nothing above involves `rqs_lib`, which is why it
could be verified at all.

---

## 2026-09-30 — Project bootstrap, blocked on `protoc`

**Done.**

- Read PLAN.md and verified its claims against this machine and against the
  pinned `rqs_lib` source. Three factual errors found and corrected in PLAN.md;
  see "Plan corrections" below.
- `git init`; GPL-3.0 `LICENSE` (verbatim, copied from the Packet clone,
  md5 `1ebbd3e3…`, 674 lines); `Cargo.toml`; `README.md`.
- `src/meminfo.rs` — self-measurement of RSS/PSS/private-dirty/threads from
  procfs. Reused by the Phase 4 tuning work.
- `src/main.rs` — `current_thread` runtime, RQS boot, transfer event loop,
  consent plumbing, SIGTERM/SIGINT shutdown, boot + interval memory logging.
- `data/quickshare-daemon.service` — systemd user unit.
- `docs/` — DECISIONS (D-001…D-005), RQS_LIB_NOTES, MEASUREMENTS, this journal.

**Verified.**

- Rust 1.94.1; `dbus-1` 1.16.2; NetworkManager and bluetooth both active.
- GNOME Shell 50.4's notification server advertises `actions` and `persistence`
  (`GetCapabilities`), so PLAN.md's accept/deny-inside-a-notification design
  works. No fallback UI needed.
- The dependency tree resolves against `rqs_lib` rev `17702ed` and every
  dependency compiles — `bluer`, `btleplug`, `mdns-sd`, `p256`, `sys_metrics`,
  `tracing-subscriber` all built.
- `src/meminfo.rs` compiles clean under edition 2024 and its 3 unit tests pass,
  checked with `rustc --test` against the already-built `anyhow` rlib, since
  `cargo test` is blocked by the same build failure as `cargo build`.
- Packet baseline re-measured: 44.7 MB RSS / 42.4 MB PSS / 30 threads. In
  MEASUREMENTS.md.
- 154 unique crates in the resolved tree. Seven of them are Bluetooth/D-Bus:
  `bluer`, `btleplug`, `bluez-async`, `dbus`, `dbus-tokio`, `dbus-crossroads`,
  `libdbus-sys` — recorded as evidence in D-002.

- **`src/main.rs` type-checks, via a stub.** Since the real `rqs_lib` cannot be
  built here, its public API was transcribed from the pinned source into a
  signature-only stub crate (218 lines, no behaviour) and the project's real
  sources were checked against it:
  `cargo check --all-targets` clean, `cargo clippy --all-targets` clean with
  zero warnings, `cargo test` 3/3 passing.
- **The event loop runs.** Built against the stub and executed for 8 s: the
  boot log, the 5 s settle measurement, the `QSD_MEASURE_SECS` interval
  (correctly *not* firing immediately, because of the `measure.reset()`),
  SIGTERM handling and the shutdown measurement all behaved as intended, and it
  reported **4.8 MB RSS / 3.1 MB PSS / 1 thread**. Recorded in MEASUREMENTS.md
  with the caveats it deserves.

  The stub lives in the session scratchpad at `typecheck/` and is deliberately
  **not** committed: it would drift out of sync with the pin and start lying.
  RQS_LIB_NOTES.md holds everything needed to regenerate it.

**Still not verified.** The stub proves the code is type-correct against the API
*as documented in RQS_LIB_NOTES.md*. If a signature there was transcribed
wrongly, the stub reproduces the mistake faithfully and the check passes anyway.
Nothing about protocol behaviour, mDNS, BLE or the real footprint is verified.

**Blocked.**

`cargo build` fails in `rqs_lib`'s build script:

```
core_lib/build.rs:15:6: called `Result::unwrap()` on an `Err` value:
Custom { kind: NotFound, error: "Could not find `protoc`. …" }
```

`prost-build` 0.13.5 does not bundle a protobuf compiler (confirmed: no
`protoc` anywhere in the crate), `protoc` is not installed, `$PROTOC` is unset,
and `sudo` on this machine requires a password. Fedora 44 ships
`protobuf-compiler` 3.19.6, which is new enough for prost-build. Needs:

```
sudo dnf install protobuf-compiler
```

Nothing else is known to block the build. `src/main.rs` has therefore **never
been compiled** — expect ordinary type errors on the first successful build.

**Next.**

1. Install `protoc`; `cargo build`; fix whatever `src/main.rs` got wrong.
2. Run it, confirm the daemon appears on an Android device's Quick Share sheet,
   and record the idle footprint in MEASUREMENTS.md — the Phase 1 checkpoint.
3. Then Phase 1 proper, in PLAN.md's order: `config.toml` loading, then
   notifications with accept/deny actions.

**Plan corrections made to PLAN.md.**

- `rqs_lib` rev: the plan's `c9ac93f…` is upstream HEAD, not what Packet pins.
  Corrected to `17702ed…`, with reasoning in D-001.
- Packet reference commit: `e42c097` → `f634d66`, the local clone's HEAD, which
  already contains the network-restart logic Phase 2 needs.
- "The logic we need is tangled into `src/window.rs` (~2,900 lines)": the RQS
  lifecycle is there, but the transfer handling is in
  `src/widgets/receive_transfer.rs` (831 lines) and
  `src/widgets/recipient_card.rs` (567). Total Packet `src/` is 5,578 lines,
  not ~6,200.

**Risks found that PLAN.md did not name.** All now recorded: the `experimental`
feature pulling in two BLE stacks *and* a second D-Bus stack (D-002),
`rqs_lib` logging through `log` rather than `tracing` (RQS_LIB_NOTES), the
`protoc` build requirement (above), and `TransferPayload::Wifi` existing as a
payload kind Phase 1 should not mishandle (RQS_LIB_NOTES).
