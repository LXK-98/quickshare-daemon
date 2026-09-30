# Decisions

One entry per decision that would be expensive to rediscover: what was decided,
why, and what was rejected. Newest last. Referenced from code comments as
`docs/DECISIONS.md#d-00N`.

---

## D-001 — Pin `rqs_lib` to `17702ed`, not to upstream HEAD

**Date:** 2026-09-30

**Context.** PLAN.md specified rev `c9ac93f5584a4265e8da679ad689df5be1e80c6b`,
described as "same as Packet". That is not what Packet pins. Checked on
2026-09-30:

- `nozwock/packet` at `f634d66` pins `rqs_lib` rev
  `17702edc9f8b0d1e67c50f125e03ca3f8c291f3c` in its `Cargo.toml`.
- `c9ac93f...` is the *current remote HEAD* of `nozwock/rquickshare`. It is
  newer, and it is not in the local cargo cache, so nothing here has ever been
  built against it.

**Decision.** Pin `17702edc9f8b0d1e67c50f125e03ca3f8c291f3c`.

The reason PLAN.md gives for the pin is that Packet is our working reference for
an undocumented, explicitly `experimental` API. That reason only holds for the
rev Packet is actually built and tested against. Pinning HEAD would keep the
citation while discarding the thing that made it worth citing.

**Rejected.**

- *`c9ac93f` (upstream HEAD), as written in the plan.* Would make every
  disagreement between our code and Packet's ambiguous: our bug, or an API
  change in the intervening commits? Not worth it before anything works.
- *A branch or tag instead of a rev.* `rqs_lib` publishes no stable API and no
  release tags that Packet follows; a moving reference would break the build at
  an arbitrary future `cargo update`.

**Revisit when** a protocol fix we need lands upstream. Move the rev
deliberately, in its own commit, and re-run the Phase 1 checkpoint afterwards.

---

## D-002 — Keep the `experimental` feature (both BLE stacks) for now

**Date:** 2026-09-30

**Context.** PLAN.md lists BLE as a memory risk and asks whether it can be
gated off. Inspecting the pinned checkout, `experimental` is a single feature
that pulls in **two** Bluetooth stacks, with no way to select one:

```toml
[features]
experimental = ["bluer", "btleplug"]
```

`bluer` is built with `features = ["full"]`, which enables `bluetoothd` — the C
`libdbus` stack — plus `id`, a 260 KB embedded Bluetooth-numbers database.
`btleplug` brings a *third* BlueZ client layer of its own. The resolved tree
confirms all of it:

```
bluer v0.17.4        btleplug v0.11.8     bluez-async v0.8.2
dbus v0.9.12         dbus-tokio v0.7.6    dbus-crossroads v0.5.3
libdbus-sys v0.2.7
```

So the daemon would carry `zbus` (ours, for notifications and the Phase 3 API)
*and* `libdbus` in one process, with three separate BlueZ abstractions on top
of the latter.

Turning the feature off costs two things: `BleListener`, which is how a *hidden*
receiver learns that a nearby device has started sharing, and `BleAdvertiser`,
used on the sending side during discovery.

**Decision.** Keep `experimental` through Phase 1 and measure it. Do not
pre-emptively optimise it away.

The feature is the configuration Packet ships and is therefore the tested path;
dropping it changes protocol behaviour, not just footprint. Whether it is
affordable is a question with a number for an answer, and Phase 1's checkpoint
produces that number. If the idle floor lands above PLAN.md's 8 MB ceiling, this
is the first thing to cut, and the cost of cutting it is already written down
here.

**Rejected.**

- *Building without `experimental` now.* Would hit the memory target by
  disabling a protocol feature before establishing that the target is missed.
- *Forking `rqs_lib` to split the feature.* PLAN.md's own guidance is to fork
  `rqs_lib` rather than Packet if patching is needed, so this stays available —
  but a fork is a maintenance burden to take on for a measured problem, not a
  suspected one.

---

## D-003 — `current_thread` runtime

**Date:** 2026-09-30

**Context.** Packet uses tokio's multi-threaded runtime. PLAN.md calls for
`current_thread`, on the grounds that threads cost stacks and glibc malloc
arenas — the mechanism behind ~31 MB of Packet's 44.7 MB.

Verified in the pinned `core_lib`: it contains no `Runtime::new`, no
`runtime::Builder`, no `block_on`, no `spawn_blocking` and no
`std::thread::spawn`. It only calls `tokio::spawn`, which works on a
`current_thread` runtime.

**Decision.** `Builder::new_current_thread().enable_all()`.

**Consequence to watch.** Transfer I/O and the AES work for a transfer share one
thread with D-Bus and notification handling. It is all `await`-driven and
chunked, so it should interleave, but progress-notification latency during a
large transfer is the place this would show up first. Compare against a
multi-threaded runtime at the Phase 4 measurement if it looks bad.

`rqs_lib` declares tokio's `rt-multi-thread` feature itself, so feature
unification compiles that runtime in regardless. It costs binary size, not
threads, because we never construct one.

---

## D-004 — The Phase 1 skeleton is the daemon, not a throwaway spike

**Date:** 2026-09-30

**Context.** The build had never been attempted on this machine, and three
premises were unverified: that it compiles at all, that `current_thread`
suffices, and what the idle floor is with both BLE stacks linked in. Building
config loading and a notification layer on top of an unverified boot path risks
discarding all of it.

**Decision.** Write the RQS boot and the event loop first, as
[`src/main.rs`](../src/main.rs), and measure. Config and notifications layer on
top of the same loop.

This is deliberately *not* a separate throwaway binary: the boot sequence, the
event-loop shape and the consent plumbing are all keeper code, and PLAN.md's
Phase 1 needs them in exactly this form. Only the environment-variable
configuration and `QSD_AUTO_ACCEPT` are temporary, and both are marked as such
in the module docs.

**Rejected.** *An `examples/spike.rs` thrown away afterwards.* Would have meant
writing the same event loop twice.

---

## D-005 — No `Arc<Mutex<…>>` around `RQS`

**Date:** 2026-09-30

**Context.** Packet stores `rqs: Arc<Mutex<Option<rqs_lib::RQS>>>` and locks it
throughout `window.rs`, because GTK signal handlers are independent entry points
that each need access.

**Decision.** `main` owns `RQS` directly; the event loop borrows it.

We have one entry point — the `select!` loop — so ownership is linear and the
lock has nothing to protect against. `rqs.message_sender` is a
`broadcast::Sender`, which is cloneable and needs no lock to send consent
decisions. This removes most of the ceremony a reader familiar with Packet would
expect to find.

**Revisit when** the Phase 3 D-Bus API lands: `zbus` handlers are separate entry
points and will need `&mut RQS` for `discovery()`/`change_visibility()`. Prefer
passing commands into the existing loop over a channel to sharing the object —
the loop stays the single owner, and the single-threaded runtime means a channel
costs nothing.
