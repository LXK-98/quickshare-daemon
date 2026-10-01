# Journal

Per-session record: what was done, what is verified, what broke, what is next.
Newest first.

## 2026-10-01 — Sending tested against the phone; an upstream crypto bug

**The send path reaches `Finished` and transfers every byte**, confirmed on a
57-byte text file: PIN shown, `SendingFiles`, 57/57 acknowledged, "All files
have been transferred". Whether the file is usable on the phone is **still
unconfirmed** — see the caution below.

**A caution I earned.** I reported an earlier run as "Sent" on the strength of
our side reaching `Finished`, and the phone showed an error. `Finished` means our
send loop completed after the receiver's Accept frame; it is not a report from
the receiving application. Do not call a transfer successful without the
receiving end agreeing.

**Bug fixed: the PIN was never shown when sending.** `rqs_lib`'s outbound path
never emits `WaitingForUserConsent` — zero occurrences in `hdl/outbound.rs` —
and that was the only branch where the CLI printed the PIN. So the PIN was
computed, logged by `rqs_lib` at INFO, and never surfaced, while the user watched
a terminal that gave no sign it was waiting for them. Result: `consent denied:
TimedOut`. The PIN now prints from whatever event first carries it.

**Upstream bug found and then confirmed empirically:
`elliptic curve point encoding error`.** A send failed during the handshake, and
the phone said *"Dateien können nicht übertragen werden"*. Both `inbound.rs` and
`outbound.rs` rebuild the peer's public key without left-padding a short
coordinate, producing 64 bytes where SEC1 requires 65.

Confirmed rather than assumed: `docs/ec-point-bug-repro.rs` replicates
`encode_point` and the rebuild logic against 500,000 real P-256 keys. It emits
the identical error string, and the failure count equals the short-coordinate
count exactly — 1910 of 500,000 — with no other failure mode. Left-padding
yields 65 bytes that decode to a valid point.

**The measurement corrected my own estimate.** I had said 1 in 128, reasoning
from 1/256 per coordinate. It is **1 in 262**: a short result needs the value
below 2^247, not 2^248, because a 31-byte value with its top bit set gets a 0x00
prepended and lands back at exactly 32 bytes. The measured 0.190% per coordinate
matches 1/512, not 1/256. A separate figure in the same run — 75.001% of keys
having at least one 33-byte coordinate, against a predicted 1−(1/2)² — confirms
the two's-complement model the whole analysis rests on.

Intermittent, unrelated to file type, and **it affects receiving too**. Full
analysis in RQS_LIB_NOTES.md; the fix is three lines in a fork, not yet taken.

One honest caveat: at 1 in 262 we were unlucky to hit this within a handful of
attempts (~1.5%). Consistent with chance, but a single observation cannot rule
out something else raising the real-world rate.

**Error messages corrected.** A failed handshake was being reported as
"{device} never accepted the transfer", which blames the user for something they
were never asked. `Disconnected` now resolves three ways, by how far the
handshake got: no PIN means the key exchange failed, a PIN but no bytes means
consent was really refused, and bytes in flight means a genuine drop.

**Testing constraint, from Alex:** only ever transfer to "Pixel 6a", and
only with files generated for the test. I had suggested sending one of their
received files from `~/Downloads`, which is someone else's content and not mine
to retransmit. Also disclosed: `MDnsDiscovery` TCP-probes every advertiser it
resolves, so `devices` is not passive.

---

## 2026-10-01 — Discovery verified against a real phone; two bugs from it

Testing `devices` against a Pixel 6a found two things no unit test would have.

**Device names were being wiped.** The phone resolved correctly as
`Pixel 6a`, then reappeared as `Unknown device`. Cause: a backwards
assumption about `rqs_lib`. Its `ServiceRemoved` branch sends
`EndpointInfo { id, ..Default::default() }` — the id and *nothing else*, so
`present` is `None` because that is `Option`'s default, and `Some(false)` is
never sent anywhere in the library. Reading `None` as "still present" applied
the empty removal as an update and erased the name.

**My test asserted the same wrong belief, which is why it passed.** It is now
inverted, named after the real behaviour, and carries a note saying what it used
to claim. The name is additionally guarded against being overwritten by an
absent one, because losing a device's name is the one failure of this class a
user actually sees.

Removals now *withdraw* with a 10 s grace rather than deleting. The same run
showed `ServiceRemoved` arriving seconds after `ServiceResolved` while the
phone's sheet was open and it was still reachable, so acting instantly would
blink a device out of the picker exactly as the user reaches for it. `rqs_lib`'s
own comment claims a threshold here; none is implemented.

**Confirmed fixed against the phone:** a fresh run (different random port and
endpoint id, which is how one run is told from another — both are randomised per
start) printed the name once, took the `ServiceRemoved` silently, and listed
`Pixel 6a`.

**`send` could not find a device that `devices` had just found.** The sheet had
closed in between, and `send` gave up after its 8 s window. That is not a bug so
much as a design mistake: a phone advertises only while its Quick Share sheet is
open, and nobody can hold that open while typing a command. `send` now waits up
to 45 s and returns the *moment* the named device appears, so the wait costs
nothing when it is already there and makes "start the send, then pick up the
phone" the normal way to use it.

Clippy clean, 45/45 tests.

---

## 2026-10-01 — Phase 3 started: discovery and sending

**Added.** `devices` and `send` subcommands, a device registry, and the
`--help`/argv handling they needed.

- `src/devices.rs` — the device registry: applies discovery events, expires
  entries not seen in 120 s, and hands out **refcounted discovery leases** so a
  second front-end closing its menu cannot stop discovery out from under the
  first. The leases are not reachable yet — only the D-Bus API can hold one —
  but refcounting is designed in now because retrofitting it is the bug
  docs/EXTENSION_PLAN.md exists to prevent.
- `src/cli.rs` — one-shot `devices` and `send`. Both run their own short-lived
  service and advertise as **invisible**: sending needs discovery, not
  discoverability, and a one-shot should not briefly appear as a second receiver
  beside a running daemon. They use a random port so they cannot collide with a
  daemon holding a configured one.
- `send` validates every file before starting a service, so a typo fails
  immediately rather than after ten seconds of discovery, and refuses
  directories, which Quick Share cannot send.
- Ambiguous device names are an error listing the ids, never a guess. Sending a
  file to the wrong device is not recoverable.

**Three things reading `rqs_lib` turned up**, all now in RQS_LIB_NOTES.md:

1. **`EndpointInfo.id` is not a device id — it is literally `"ip:port"`.** It
   changes whenever a phone re-advertises on a fresh port, so the same device
   arrives under a new id and a naive picker lists it twice. The registry now
   deduplicates by address. This also **corrects advice I had written into
   EXTENSION_PLAN.md** about exposing "stable device ids" over D-Bus: there are
   none to expose.
2. **Discovery filters out devices on this machine's own IPs**
   (`is_not_self_ip`). So Packet running here is invisible to us — correct, but
   it means "no devices found" is expected when the only advertiser is local.
3. **`OutboundPayload` has one variant, `Files`.** Sending text, a URL or Wi-Fi
   credentials is not supported by the library at all; those exist only inbound.

**Verified.** Clippy clean, 42/42 tests. `devices` runs end to end: mDNS
discovery, BLE advertiser on hci0, clean shutdown. It correctly found nothing —
`avahi-browse` confirmed the only Quick Share advertiser on the LAN at the time
was on this host, which `is_not_self_ip` filters by design.

**Not verified: the send path itself.** No file has been sent. It needs a second
device with its Quick Share sheet open, which needs a person holding a phone.

**Two test fixtures were wrong and the new deduplication caught them.** Both
built several "devices" sharing one IP, which is not a thing that happens on a
LAN; the dedup correctly collapsed them into one and two tests failed. The
fixtures now mirror `rqs_lib`'s real id format.

---

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
