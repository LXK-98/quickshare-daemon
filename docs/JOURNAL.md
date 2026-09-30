# Journal

Per-session record: what was done, what is verified, what broke, what is next.
Newest first.

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
