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

---

## D-006 — Raw `org.freedesktop.Notifications`, not the XDG portal

**Date:** 2026-09-30

**Context.** Packet sends notifications through `ashpd`, the XDG portal client.
PLAN.md specifies the raw spec instead. Both work; the reasons for the raw spec
turn out to be stronger than the plan states.

**Decision.** Talk to `org.freedesktop.Notifications` directly over the session
bus, via `zbus`.

- The portal exists to give *sandboxed* applications a mediated path to the
  session. We are deliberately not sandboxed — avoiding Flatpak is a stated goal
  — so the portal adds a hop and a dependency to solve a problem we do not have.
- The raw interface gives us `ActionInvoked` and `NotificationClosed` as
  signals. Buttons that we can actually wait on are the whole user interface;
  anything that obscured those signals would be disqualifying.
- `ashpd` would pull in either `async-io` or a GTK backend. We want neither.

**Verified on 2026-09-30** against GNOME Shell 50.4: `GetCapabilities` returns
`actions`, `body`, `body-markup`, `icon-static`, `persistence`, `sound`.
`Notify` returns a server id, `CloseNotification` works, and a consent prompt
left unanswered returns `TimedOut` after exactly its timeout and closes itself.

**Consequence.** `actions` is optional in the spec, and a server without it
leaves the daemon unable to receive anything interactively. Rather than fail at
startup, [`Notifier::supports_actions`] is checked once and logged as an error
pointing at `auto_accept_from`, which still works.

---

## D-007 — `zbus` with `default-features = false, features = ["tokio"]`

**Date:** 2026-09-30

**Context.** `zbus` defaults to the `async-io` reactor. Taking that default in
a tokio program means two executors in one process: async-io spawns its own
polling thread, and every thread costs a stack and potentially a glibc malloc
arena — the exact mechanism behind Packet's overhead that D-003 is about.

**Decision.** `zbus = { version = "5.7", default-features = false, features = ["tokio"] }`.

Silently costly: nothing fails if the feature is forgotten, the daemon simply
grows a second event loop. Recorded here because the failure mode is invisible
and a future `cargo add zbus` would reintroduce it.

---

## D-008 — The consent prompt is spawned, the rest of the handling is not

**Date:** 2026-09-30

**Context.** A consent prompt waits up to `accept_timeout_secs` (default 60).
On a `current_thread` runtime, awaiting that inline in the event loop would
stall every other event for the duration — including a second sender's request,
and including progress on a transfer already running.

**Decision.** Spawn a task per consent prompt; handle everything else inline.

Completion and failure notifications are D-Bus method calls that return in
milliseconds, so they cost the loop nothing worth restructuring for. Only the
part that waits on a human is moved off it.

This works because the reply path is a `broadcast::Sender`, which is cloneable
and `Send`, and `rqs_lib` expects consent as an ordinary channel send with no
ordering guarantee relative to other events. Each transfer handler filters by
id, so a late or out-of-order decision reaches the right place or nothing at
all.

**Rejected.** *Awaiting the prompt inline.* Simpler, and wrong the first time
two devices send at once — which is exactly the case a shared family machine
hits.

---

## D-009 — NetworkManager's `StateChanged`, debounced, with the restart deferred

**Date:** 2026-09-30

**Context.** `rqs_lib` binds its listener and starts mDNS once, at `run()`.
Neither survives an interface going away and returning with a new address: the
daemon stays up, logs nothing, and is silently invisible to every phone on the
network. Packet handles this through GIO's `NetworkMonitor`; we have no GIO, so
PLAN.md specifies NetworkManager's D-Bus interface.

**Decision.** Watch `org.freedesktop.NetworkManager`'s `StateChanged`, react
only to a down → up edge, debounce 3 s, and defer the restart while transfers
are in flight.

- **`ConnectedLocal` (50) counts as up**, not just `ConnectedGlobal` (70). Quick
  Share is mDNS plus a direct TCP connection and never needs the internet.
  Requiring global connectivity would disable the daemon on exactly the
  isolated networks it is most useful on.
- **3 s debounce**, the value Packet arrived at, because NetworkManager reports
  several states while a connection comes up. Without it the service restarts
  into a network that is still settling.
- **Edge, not level:** a site → global transition means the link never dropped
  and the listener is still valid.
- **Deferred, not refused.** A restart while a transfer is running would drop
  it. The request is held in a flag and run when `active` next empties, which
  is the moment the last transfer ends. Packet retries on a timer instead; a
  flag needs no interval guess and cannot starve.

**Verified 2026-09-30** against the live system bus, read-only: the proxy
connects, the current state reads as `ConnectedGlobal`, and a stable link
produces no spurious reconnect over a 5 s window. The down → up path and the
debounce are **not** verified — that needs the network actually taken down.

**Gotcha worth keeping.** zbus derives `receive_state_changed` from both the
`State` property and the `StateChanged` signal, which collide. The signal is
bound as `nm_state_changed` with `#[zbus(signal, name = "StateChanged")]`. The
generated *stream type* is still named after the D-Bus signal
(`StateChangedStream`), not after the Rust method.

---

## D-010 — The "Open" button refuses files that would execute

**Date:** 2026-09-30

**Context.** A completed transfer's notification carries "Open" and "Show in
Folder" buttons. "Open" runs `xdg-open` on a file whose name was chosen
entirely by the sender.

`xdg-open` on a `.desktop` file **launches it**. So the naive version of this
feature turns "you accepted a file from a nearby device" into "you executed
code from a nearby device", with one click, on a button sitting under whatever
name the sender picked — `Invoice.pdf.desktop` renders as `Invoice.pdf` in many
file listings.

**Decision.** `open_file` refuses a small deny-list of extensions
(`.desktop`, `.sh`, `.bash`, `.run`, `.appimage`, `.bin`) and the caller falls
back to revealing the file in the file manager, which is safe for anything.

This is not real protection — the file is already written to `~/Downloads`, and
the user can run it from there. The point is narrower: **the notification
button should not be the thing that runs it.** A button offered by the daemon
carries an implicit claim that pressing it is safe, and for these files it is
not.

**Rejected.**

- *Checking the executable bit instead.* `rqs_lib` writes received files
  without it, so it would never fire, while `.desktop` handling does not depend
  on it anyway.
- *A MIME-type check via `xdg-mime`.* Another process spawn on every click, to
  answer a question the extension already answers for the cases that matter.
- *Not offering "Open" at all.* It is the single most useful thing to do with a
  photo someone just sent, and refusing the whole feature over a handful of
  extensions is the wrong trade.

**Consequence.** A legitimately received shell script cannot be opened from the
notification. "Show in Folder" still works, which is the right destination for
a file you need to look at before running.

---

## D-017 — Distribution: COPR and release tarballs, and no self-update

**Date:** 2026-11-08

**Context.** Alex wants installation and updating to be as simple as possible,
and asked whether a `quickshare update` command or auto-update made sense.

**Decision.** Two channels, and **no self-update**.

**Fedora: a COPR repository.** `dnf copr enable` then `dnf install`, after
which `dnf upgrade` keeps it current. This is simpler than any script we could
write *and* it is the update mechanism, so nothing further is needed. COPR
builds from `packaging/quickshare-daemon.spec`, which splits the Nautilus menu
into a subpackage because it is the only component requiring
`nautilus-python`.

**Everything else: GitHub releases plus `packaging/install.sh`.** CI builds on
**Ubuntu 22.04 rather than the newest runner**, because the binary links glibc
and building on a newer base raises the minimum version it will run against —
22.04 gives glibc 2.35 against 2.39 on 24.04, which would silently exclude
older distributions. The script verifies a published SHA-256 before unpacking
anything, and re-running it is the update path.

**Rejected: self-update or auto-update.** Three reasons:

1. **It would need an HTTP and TLS stack.** `reqwest` or `rustls` adds several
   megabytes to a 3.2 MB binary whose entire justification is a measured
   footprint. Paying that for something `dnf` already does is the wrong trade
   in a project that measures itself.
2. **A background daemon that overwrites its own binary is supply-chain
   surface.** Doing it responsibly means signature verification and key
   management, which is more machinery again.
3. **It fights the package manager** on any packaged install.

The honest form of "auto update" here is to let the distribution do it.

**On `curl | sh`.** The release notes show downloading the script and reading
it before running, rather than leading with a pipe into a shell. Piping remote
code into a shell is normal and also a thing people are right to dislike; the
checksum and a readable script are the least we can do.

---

## D-016 — Option B implemented: identity separate from address

**Date:** 2026-11-07

Implements the recommendation in docs/TRANSPORT_IDENTITY.md, which has the full
reasoning. What actually landed:

**In the fork** (`5b5a3e2`):

- `EndpointInfo` gains `endpoint_id`, from the first label of the mDNS
  `fullname`. `id` is left as the synthesised `"ip:port"`, since callers pass it
  back as `SendInfo::id` — adding a field rather than redefining one keeps the
  change additive for any other consumer.
- `SendInfo` carries a `Target` enum instead of a bare `addr`, with
  `WifiLan { addr }` as its only variant, and `connect` dispatches on it.
- The outbound error path reports `si.id` rather than `si.addr`, fixing the
  latent inconsistency the note flagged.

**Here:**

- `Device` holds `addresses: Vec<Address>` — medium plus address — and `id` is
  the endpoint id. `Medium` names `WifiLan` and `Bluetooth`; only the first
  works.
- `Registry` is keyed on the endpoint id, falling back to the address for peers
  that advertise none. **The IP-based deduplication is gone**: it existed
  because the key *was* the address, so keying on identity removes the problem
  rather than compensating for it.
- `best_address` picks the medium, preferring Wi-Fi LAN. This is the daemon's
  choice by design — only it knows what is reachable and what each costs.
- `ListDevices` dicts replace `address: s` with `addresses: a{ss}` (medium →
  address) and add `medium: s`, the one the daemon would use. `id` now means
  identity.

**One wrinkle worth knowing.** A removal event carries only `rqs_lib`'s
`"ip:port"` id and *no* endpoint id, so a withdrawal cannot be matched by key.
`Registry` therefore looks a withdrawal up by address as well. Both paths are
tested.

**Breaking the D-Bus contract was the point of doing it now.** `address` is
gone and `id` means something different. The only consumer is our own CLI;
once a Nautilus or Shell extension exists this would have been a migration.
The `the_dict_keys_are_stable` test caught the change, which is what it is for,
and a new test now checks `device_dict` and `RemoteDevice::from_dict` against
each other so the two halves of the contract cannot drift apart.

**Verified:** 50 tests including the new identity cases — a device moving
address stays one device, two devices sharing an address stay two, and a Wi-Fi
announcement does not disturb a Bluetooth address. Introspection confirms
`ListDevices` still returns `aa{sv}`.

**Confirmed against the real device**, 2026-11-07:

```
endpoint_id: Some("EXAMPLEENDPT02")   identity
id:          "192.0.2.129:53601"   address

NAME                 VIA        ADDRESS                 ID
Pixel 6a    wifi_lan   192.0.2.129:53601    EXAMPLEENDPT02
```

The endpoint id the fork extracts from the fullname is the same value the
controlled stability check measured. One gap remains, and it is narrow: that run
used the standalone path, so the `addresses` map going over D-Bus has been
exercised by tests rather than against a phone. The `Device` model underneath it
is the same code either way.

---

## D-015 — Genericise the handshake over its transport

**Date:** 2026-11-07

**Context.** Sending and receiving without a shared Wi-Fi network is the
direction Alex wants to take this. `rqs_lib` advertises `mediums:
vec![Medium::WifiLan]` and has no `BandwidthUpgradeNegotiation` handling, but
the deeper obstacle was structural: `InboundRequest` and `OutboundRequest` held
a `TcpStream` **concretely**, so no other medium could be plugged in at all.

Nearby Connections defines `BLUETOOTH`, `WIFI_HOTSPOT`, `WIFI_DIRECT`,
`WIFI_AWARE` and `WEB_RTC` alongside `WIFI_LAN`, and the frame format is
identical on every one. Only the byte pipe differs.

**Decision.** Add a `Transport` trait to the fork — `AsyncRead + AsyncWrite +
Unpin + Send`, with a blanket impl — and make both request types generic over
it. `utils::stream_read_exact` takes any `AsyncRead`.

That is the entire requirement: length-prefixed frames over an ordered,
reliable stream. A blanket impl rather than explicit opt-in because there is
nothing a transport could usefully add beyond the three standard traits, and
requiring registration would be ceremony.

**Cost: none measurable.** No behaviour changed, and `manager.rs` needed no
edits — `TcpStream` satisfies the bound and the types infer. Our 48 tests pass
unchanged and TCP discovery still works.

**Verified** by two tests in the fork that construct a connection over
`tokio::io::duplex`, an in-memory pipe. Them compiling is the demonstration:
the handshake no longer depends on TCP. They also make it possible to exercise
the protocol in tests without a network, which nothing could do before.

**Why this first.** `bluer::rfcomm::Stream` already implements `AsyncRead` and
`AsyncWrite`, so with this in place a Bluetooth connection is a matter of
obtaining the stream, not of touching the protocol code. Doing it the other way
round — writing the Bluetooth medium first — would have meant either duplicating
the handshake or doing this refactor under pressure from half-finished code.

---

## D-014 — `Finished` must mean the protocol finished, not the last byte

**Date:** 2026-11-07

**Context.** A 39-byte transfer to a Pixel 6a reported every byte acknowledged,
`All files have been transferred` and `TransferState::Finished` — and the phone
showed a transfer error. Nothing on our side looked wrong.

The cause is an ordering in `rqs_lib`'s outbound send loop:

```rust
self.update_state(|e| e.state = TransferState::Finished, true).await;
self.disconnection().await?;
```

`Finished` is announced *before* the graceful Disconnection frame is sent. A
client told a transfer has finished reasonably stops its service at once, and
`RQS::stop` cancels the task that would have sent that frame. The receiver then
loses the connection without a clean close and marks the transfer failed, having
taken every byte.

**Decision.** Two changes, belt and braces.

In the fork, swap the two statements, so `Finished` means the protocol is
complete rather than that the last byte was written. A failure to send the frame
is logged rather than propagated: the transfer did succeed, and the client still
needs to hear `Finished`.

In this project, wait a second after a successful transfer before stopping the
service — in the CLI's one-shot path and in the daemon's shutdown. That covers
the window regardless of which `rqs_lib` revision is underneath, which matters
because the pin will eventually move and this failure mode is silent on our side.

**Why the event could not simply be trusted.** This is the second time a
terminal state has meant less than it appears to. `Finished` is set by the
sender's own loop; nothing in it is a report from the receiving application
(RQS_LIB_NOTES.md says so). Treating it as proof of delivery is what produced
three separate claims of success in this project against a phone that disagreed.

**Status: confirmed.** With the fork's reordering in place and the daemon left
running, a file sent to the Pixel 6a arrived and its contents were readable.
The race was the cause, and it was the only one: nothing else changed between
the failing run and the succeeding one.

---

## D-013 — The D-Bus API shape

**Date:** 2026-11-07

`org.quickshare.Daemon1` at `/org/quickshare/Daemon1`. The three decisions
EXTENSION_PLAN.md said to make now, made:

**Signals, not polling.** `DevicesChanged` and `TransferFinished`, plus the
automatic `PropertiesChanged` on `Visible`. `DevicesChanged` deliberately
carries **no payload** — a client re-reads `ListDevices`. One source of truth,
and no client assembling a stale view from deltas it may have missed.

**`aa{sv}`, not fixed tuples.** Keys `id`, `name`, `address`, `reachable`.
`reachable` is included rather than left to be inferred from an empty
`address`: whether a device can be sent to is our judgement, not an artefact of
how we represent it. A test pins the key set, since clients index by name and
renaming one is a breaking change.

**Discovery is leased.** `StartDiscovery` takes a lease keyed by the caller's
bus unique name (`:1.42`), which the bus assigns, the client cannot spoof, and
which disappears exactly when the client does. A `NameOwnerChanged` watcher
turns a departure into `Command::ClientGone`. Without that, a front-end
crashing with a lease open would pin discovery — and its BLE advertiser — on
until the daemon restarted.

**The interface holds no state.** Handlers send a `Command` into the event loop
over a channel and await a `oneshot` reply. This settles the question D-005 left
open: `zbus` handlers are separate entry points that would each need
`&mut RQS`, and passing commands keeps the loop the single owner rather than
reintroducing the `Arc<Mutex<…>>` that D-005 removed. The `oneshot` is what
lets a handler report the real outcome instead of acknowledging blindly.

**The bus is optional.** Failing to claim the name is a warning, not a fatal
error: a second daemon, or a session without a bus, should still receive files.
It simply cannot be driven remotely.

**`TransferPin` is a signal, not a return value.** The PIN does not exist when
`SendFiles` returns — it falls out of the key exchange, which happens
afterwards. A client must show it: it is how the person at the other device
confirms the transfer is really from here, and nothing can be accepted until
they do. Added 2026-11-07 when the CLI became a client and would otherwise have
lost the PIN display it already had.

**The CLI prefers the daemon and falls back.** `devices` and `send` ask the bus
whether `org.quickshare.Daemon1` has an owner; if so they drive it, and
otherwise they stand up their own short-lived service as before. Asking the bus
rather than calling a method and interpreting the error is deliberate: "no
daemon running" and "the daemon returned an error" deserve different handling,
and only `NameHasOwner` distinguishes them.

Using the daemon avoids a second mDNS stack, a second TCP listener and a second
BLE advertiser standing up alongside the daemon's, and reuses discovery that may
already be warm. Keeping the fallback means the CLI still works on a machine
where the service is not enabled, which is how PLAN.md describes it being used
for scripting.

**Verified 2026-11-07** against the live session bus: introspection shows the
full interface, `Visible` reads, `ListDevices` returns `aa{sv}`,
`StartDiscovery` attributes a lease to `:1.1314`, that client's departure
releases it, and a second client **joins the running discovery** inside the
grace period rather than restarting mDNS.

**Cost measured.** Serving the API moves idle from 6.2 MB RSS / 3.5 MB PSS / 2
threads to **6.7 MB / 4.0 MB / 3** — `zbus` brings its own task and a bus
connection. Still inside PLAN.md's 8 MB ceiling.

---

## D-012 — Fork `rqs_lib` to fix the P-256 coordinate padding

**Date:** 2026-11-07

**Context.** The `elliptic curve point encoding error` documented in
RQS_LIB_NOTES.md breaks roughly 1 connection in 262, inbound and outbound
alike. D-001 pinned a rev partly so a fix could be picked up "deliberately"
when one landed. Re-checked on 2026-11-07:

- `nozwock/rquickshare` HEAD is still `c9ac93f`, last commit 2026-09-25 — the
  same HEAD as when we first looked on 2026-09-30. Six weeks, no movement.
- `Martichou/rquickshare`, the actual upstream, HEAD `378d8ae`, last commit
  **2025-06-16** — about 17 months stale, 105 open issues.
- Both still carry the bug, at all four call sites, byte-identical.
- No issue in either tracker matches "elliptic curve", "point encoding",
  "SentUkeyClientInit", "curve" or "handshake". Unreported.

So there was nothing to wait for. "Update deliberately when a fix lands" had
quietly become "never".

**Decision.** Fork to `LXK-98/rquickshare` and pin
`7b42f7d94869f2b4570f4fe1c97a989cbb377797`.

The fork is **branched at `17702ed`**, the rev Packet is tested against, and
carries exactly one commit. That preserves the property D-001 actually cared
about — Packet stays a valid API reference — while fixing the defect. Divergence
from the reference is one function, not a moving target.

The fix adds `utils::p256_coordinate`, which right-aligns the significant bytes
into a fixed `[u8; 32]`, and routes both call sites through it. Unit tests in
the fork cover short, sign-byte-prefixed, exact and empty coordinates plus the
property that any input length yields a 65-byte point.

**Verified.** Re-ran the 500,000-key reproduction against the fork's real
helper: 1991 keys carry a short coordinate (0.398%), **0 decode failures, 0
wrong or invalid points**. Every one of those 1991 failed before. The check
asserts the rebuilt point equals the original, not merely that it parses.

**Rejected.**

- *Keep waiting for upstream.* Both repos are dormant and the bug is
  unreported; this was waiting for something nobody was working on.
- *A local `[patch]` against a path dependency.* No URL to pin, nothing to
  share, and it would not survive a clone of this repo.
- *Work around it in our own code.* The failure is inside the library's
  handshake; there is no seam above it to retry from, and it breaks receiving,
  where we are not the one initiating.

**Cost accepted.** A fork is a maintenance commitment: picking up future
`rqs_lib` changes now means rebasing one commit rather than bumping a rev. One
commit is about as cheap as that gets, and the alternative was a known 1-in-262
failure in the daemon's primary function.

**Open:** the fix is worth sending upstream. Not yet offered — see
docs/JOURNAL.md.

---

## D-011 — Revealing files goes through `FileManager1`, not `xdg-open`

**Date:** 2026-09-30

**Context.** "Show in Folder" could just `xdg-open` the containing directory.

**Decision.** Call `org.freedesktop.FileManager1.ShowItems`, which opens the
folder **with the files selected**, and fall back to `xdg-open` on the
directory only when no implementation is running.

Selecting the file is what the phrase means to whoever asks for the feature —
especially when a transfer drops several files into a `~/Downloads` that
already has hundreds, or when `rqs_lib` names them things like
`tv75Zr9J92SObYbT.mp4`, which is exactly what it does. `ShowItems` also takes
several URIs at once, so a multi-file transfer selects all of them.

Verified present on this system: Nautilus implements the interface.

**Note.** URIs are percent-encoded by hand in `open.rs`. Received filenames
come from the sender and routinely contain spaces, `#` and non-ASCII, each of
which silently truncates or corrupts a naively formatted `file://` URI.
