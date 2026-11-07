# `rqs_lib` notes

`rqs_lib` has no published documentation and no stable API. Everything here was
read out of the pinned checkout on 2026-09-30 and is accurate for **rev
`17702ed`** (`rqs_lib` 0.11.5) only. Re-check it when the pin moves (D-001).

Source of truth, as cargo fetched it:

```
~/.cargo/git/checkouts/rquickshare-4d4c6a55ac9ed53a/17702ed/core_lib/src
```

## Lifecycle

```rust
let mut rqs = RQS::new(visibility, port_number, download_path, device_name);
let (file_sender, ble_events) = rqs.run().await?;   // binds, advertises, spawns
rqs.discovery(endpoint_tx)?;                        // outbound only; needs run() first
rqs.stop_discovery();
rqs.stop().await;                                   // cancels + joins every task
```

- `new` only stores configuration. Nothing binds or advertises until `run`.
- `new` writes `download_path` and `device_name` into **process-global statics**
  (`CUSTOM_DOWNLOAD`, `DEVICE_NAME`, both `Lazy<RwLock<…>>`). Two `RQS` instances
  in one process would fight over them. Equivalent setters exist for later
  changes: `set_download_path`, `set_device_name`, `get_device_name`.
- `run` binds `0.0.0.0:{port_number or 0}` and then **writes the resolved port
  back** into `self.port_number`. So after a `stop()`/`run()` cycle the same
  port is reused even when the caller originally asked for a random one. Packet
  works around this by comparing against `rqs.port_number` before restarting.
- `run` spawns, into a `TaskTracker`: the TCP server, the mDNS server, and —
  with `experimental` — a `BleListener`. `discovery` additionally spawns a
  `BleAdvertiser` (Linux + `experimental`) and `MDnsDiscovery`.
- `discovery` returns `Err` if called before `run` ("The service wasn't first
  started").
- Visibility changes go through `change_visibility(Visibility)`, which is a
  `watch` channel send — cheap, no restart needed. `Visibility` is `Visible`,
  `Invisible` or `Temporarily`.

### `run`'s return values are both load-bearing

`run` returns `(mpsc::Sender<SendInfo>, broadcast::Receiver<()>)`.

- The `Sender<SendInfo>` is how outbound transfers are started (Phase 3).
  **Keep it alive even while unused.** `TcpServer::run` selects on the paired
  receiver with a `Some(i) = self.connect_receiver.recv()` pattern; if every
  sender is dropped, `recv()` resolves to `None` immediately, the pattern fails
  and the branch is re-polled on each iteration of the server's loop. Not fatal,
  but pointless work forever.
- The `Receiver<()>` fires when a nearby device starts sharing (the BLE
  listener's signal). Phase 3 can use it to run discovery on demand.

## Events and consent share one channel

`rqs.message_sender` is a `broadcast::Sender<ChannelMessage>` with a **capacity
of 50**. Subscribe *before* `run`, or early messages are lost.

```rust
pub struct ChannelMessage { pub id: String, pub msg: Message }

pub enum Message {
    Lib { action: TransferAction },   // client → library: consent
    Client(MessageClient),            // library → client: state + metadata
}

pub struct MessageClient {
    pub kind: TransferKind,           // Inbound | Outbound
    pub state: Option<TransferState>,
    pub metadata: Option<TransferMetadata>,
}
```

The direction is encoded in the variant, not in a separate channel. Consent is
sent back into the *same* `message_sender`:

```rust
rqs.message_sender.send(ChannelMessage {
    id: transfer_id,
    msg: Message::Lib { action: TransferAction::ConsentAccept },
})?;
```

Two consequences:

1. **Our own replies are echoed back to our own subscriber.** Match on
   `Message::Client` only (`msg.as_client()` is the helper) or the handler feeds
   itself.
2. **Every consent message reaches every in-flight transfer handler.** Each
   handler filters by id: `InboundRequest::handle` starts with
   `if channel_msg.id != self.state.id { return Ok(()); }`. That early return is
   safe because `TcpServer` drives `handle()` in a `loop`, so the handler is
   re-entered rather than abandoned.

`TransferAction` is `ConsentAccept`, `ConsentDecline` or `TransferCancel` — the
same enum cancels an accepted transfer mid-flight.

### States worth reacting to

`TransferState` has 18 variants, most of them handshake steps
(`SentUkeyServerInit`, `ReceivedPairedKeyResult`, …). The ones that matter:

| State                    | Meaning                                            |
|--------------------------|----------------------------------------------------|
| `WaitingForUserConsent`  | ask the user; `metadata` has source, size, pin code |
| `ReceivingFiles` / `SendingFiles` | progress; repeats per chunk               |
| `Finished`               | done; `metadata.payload` has the result            |
| `Rejected` / `Cancelled` / `Disconnected` | ended without completing      |

`TransferMetadata` carries `id`, `source: Option<RemoteDeviceInfo>` (`name`,
`device_type`), `pin_code`, `payload_kind` (`Files`/`Text`/`Url`/`WiFi`),
`payload_preview` (text only), `payload: Option<TransferPayload>`, `total_bytes`
and `ack_bytes`. Note `TransferPayload::Wifi { ssid, password, security_type }`
— a sender can push Wi-Fi credentials, which PLAN.md does not mention and
Phase 1 should at least not mishandle.

## Received files

`rqs_lib` writes into the download directory itself and **resolves filename
collisions on its own**: `hdl/inbound.rs` checks `dest.exists()` and inserts
` (1)`, ` (2)`, … before the extension, so `lorem.txt` becomes `lorem (1).txt`.
Nothing is needed on our side, and anything we added would race with it.

It reports the result as **bare file names, not paths**
(`files_name.push(file.name())`), so a caller that wants a usable path must join
them with the download directory it configured. Note that the name it reports is
the *original* one when a collision occurred — not the ` (1)` variant actually
written. A notification naming the file can therefore name a file that is not
the one on disk; harmless for display, wrong for anything that opens it by name.

## Discovery (`MDnsDiscovery`)

Browsing `_FC9F5ED42C8A._tcp.local.`. Three behaviours that are not obvious and
that shape any device picker built on it:

1. **Devices on the local machine are filtered out.** `is_not_self_ip(ip)` skips
   any advertiser on one of this host's own addresses. So a second Quick Share
   implementation running on the same machine — Packet, or our own daemon — is
   invisible to our discovery. Correct (you cannot send to yourself), but it
   means "no devices found" is the expected result when the only advertiser on
   the LAN is local.
2. **`EndpointInfo.id` is not a device id — it is literally `"ip:port"`.**

   Correction (2026-11-07): the note below says a phone "picks a fresh port per
   share session". Seven observed sessions across two days do not support that
   — the port was `53601` every time. What changed was the **IP**, across
   networks. The id is still unsuitable as a device identity, for that reason
   rather than the one originally given. The real endpoint id is in the mDNS
   `fullname` and *is* stable across sessions — confirmed by a controlled
   open/close/reopen check on 2026-11-07, where both the id and the port were
   identical either side of a 41-second withdrawal. Discovery discards it. See
   docs/TRANSPORT_IDENTITY.md.
   (`id: ip_port` in the resolve branch.) It therefore **changes whenever the
   port changes**, and a phone picks a fresh port per share session. There is no
   stable per-device identifier anywhere in the API. Anything keyed on `id`
   treats the same phone as a new device after it re-advertises, so a picker
   needs to deduplicate by address instead.
3. **A removal is signalled by `present: None`, not `Some(false)`.** The
   `ServiceRemoved` branch sends `EndpointInfo { id, ..Default::default() }` —
   so the event carries the id and *nothing else*: no name, no ip, no port, and
   `present: None` because that is `Option`'s default. `Some(false)` is never
   sent anywhere in the library.

   Getting this backwards is costly and quiet: treating `None` as "still
   present" applies the empty removal as an *update*, which wipes the device's
   name and leaves `Unknown device` in the picker while the device is still
   listed. Observed against a Pixel 6a on 2026-10-01.

   Related: `ServiceRemoved` arrives readily while a device is still present and
   reachable — the phone's sheet was open throughout — so a picker should not act
   on it instantly. The comment in the source says "only remove if it has not
   been seen in the last cleanup_threshold", but no such threshold is
   implemented.
4. **Discovery probes before reporting.** It only emits an `EndpointInfo` if
   `TcpStream::connect(ip:port)` succeeds at that moment. Devices are therefore
   always resolved and reachable when reported — but a device that is merely
   advertising, without accepting connections, never appears at all.

`SendInfo.id` takes that same `"ip:port"` string, so an id from discovery can be
handed straight back when sending.

## Sending

`OutboundPayload` has exactly one variant: `Files(Vec<String>)`. **Sending text,
a URL or Wi-Fi credentials is not supported** — those exist only on the inbound
side. Anything presenting a "share text" option would have to fake it by writing
a file.

`SendInfo { id, name, addr, ob }` where `addr` is the `ip:port` string
`TcpStream::connect` is called on, and `name` is what the remote device will be
labelled as locally. Hand it to the `mpsc::Sender<SendInfo>` that `run()`
returned.

## The outbound state machine differs from the inbound one

**Outbound never emits `WaitingForUserConsent`.** Zero occurrences in
`hdl/outbound.rs`. The states it actually sets are `Initial`,
`SentUkeyClientInit`, `SentUkeyClientFinish`, `SentPairedKeyEncryption`,
`SentPairedKeyResult`, `SentIntroduction`, `SendingFiles`, `Finished`,
`Cancelled` and `Disconnected`.

So a sender cannot learn "the receiver is being asked" from a state. It must
instead watch for `pin_code` appearing in the metadata of whatever state change
happens to carry it, which is how the PIN reaches a client at all. Keying a
"show the PIN" branch off `WaitingForUserConsent`, as the inbound side does,
produces code that never runs — and a user who is never told to accept anything.

**A refused or unanswered transfer surfaces as `Disconnected`, not `Rejected`.**
`rqs_lib` logs `Cannot process: consent denied: TimedOut` and then sets
`Disconnected`. `Rejected` is an inbound-side state. A sender therefore cannot
distinguish "declined" from "timed out" from "connection dropped" by state
alone; the only usable signal is whether `SendingFiles` was ever reached. Before
it, a disconnect means the receiver never accepted.

**`Finished` means our side completed its send**, set after the receiver's
`Status::Accept`. It is not a report from the receiving application, so a
receiver that accepts and then fails to store the file still leaves us at
`Finished`.

**Upstream also announces `Finished` before sending the Disconnection frame**,
which makes acting on it immediately unsafe:

```rust
self.update_state(|e| e.state = TransferState::Finished, true).await;
self.disconnection().await?;
```

A client that stops its service on hearing `Finished` cancels the task that
would send that frame, and the receiver treats a connection lost without a clean
close as a failed transfer even having taken every byte. Observed against a
Pixel 6a on 2026-11-07. Fixed in our fork by swapping the two statements
(`7725c0b`); still present in both `nozwock` and `Martichou`. Any client on
unpatched `rqs_lib` must wait before tearing down.

## Upstream bug: short EC coordinates break the handshake

**Symptom:** `TcpServer: error while handling client: elliptic curve point
encoding error (SentUkeyClientInit)`, the connection drops before anyone is
asked to accept anything, and the Android side says *"Dateien können nicht
übertragen werden"* / "Can't transfer files". Observed 2026-10-01.

**Cause.** Both `hdl/outbound.rs` (~line 1009) and `hdl/inbound.rs` (~line 1140)
rebuild the peer's public key as an uncompressed SEC1 point:

```rust
let mut bytes = vec![0x04];
if peer_p256_key.x.len() > 32 {
    bytes.extend_from_slice(&peer_p256_key.x[peer_p256_key.x.len() - 32..]);
} else {
    bytes.extend_from_slice(&peer_p256_key.x);   // <-- no left-padding
}
// ...same for y
let encoded_point = EncodedPoint::from_bytes(bytes)?;
```

An over-long coordinate (33 bytes, from a sign byte) is truncated correctly. A
**short** one is appended as-is, so the buffer ends up 63 or 64 bytes where
`EncodedPoint::from_bytes` requires exactly 65 — and fails.

### Why a coordinate is ever short

A P-256 public key *is a point* on the curve: a pair (x, y) of 256-bit field
elements, 32 bytes each. The fixed-width SEC1 form is `0x04 || x(32) || y(32)` =
65 bytes, but `securemessage.proto` carries x and y as **variable-length**
integers and says so explicitly:

```proto
// x and y are encoded in big-endian two's complement (slightly wasteful)
required bytes x = 1;
required bytes y = 2;
```

Two's complement means the length varies with the value: 0x00 is prepended when
the top bit is set (giving 33 bytes), and leading zeros are stripped otherwise
(giving 31 or fewer). Short coordinates are therefore **spec-sanctioned**, and a
conforming decoder has to left-pad. Variable-length input into a fixed-width
format is the whole bug.

### Measured rate

Confirmed empirically on 2026-10-01 by replicating `encode_point` and the rebuild
logic against 500,000 real P-256 keys:

| | count | share |
|---|---|---|
| short x | 952 | 0.190% |
| short y | 959 | 0.192% |
| **either short** | **1910** | **0.382%** |
| at least one 33-byte coordinate | 375006 | 75.001% |
| rqs_lib decode failures | 1910 | 0.382% |

So **1 connection in 262**, and the causal link is exact: the failure count
equals the short-coordinate count, with no other failure mode appearing. The
reproduction emits the identical error string, and left-padding yields 65 bytes
that decode to a valid point.

The 75% figure is a useful check on the model — it is exactly 1−(1/2)², which is
what two's complement predicts for "at least one of two coordinates has its top
bit set".

**An earlier note here said 1 in 128, reasoning from 1/256 per coordinate. That
was wrong.** A short result requires the value below 2^247, not 2^248: a 31-byte
value whose top byte is ≥ 0x80 gets a 0x00 prepended and lands back at exactly
32 bytes. So it is 1/512 per coordinate, and the measurement (0.190%) matches
that rather than the estimate.

**Consequences for us.**

- It is intermittent and unrelated to file type, size, or anything the user did.
  Retrying usually succeeds.
- **It affects receiving as well as sending** — the same code is in
  `inbound.rs` — so it is not merely a `send` problem.
- A client cannot see the cause: the failure surfaces as `TransferState::
  Disconnected` with no error text. Our CLI infers it from a PIN never having
  appeared, since the PIN is derived after this decode.

The reproduction is kept at `docs/ec-point-bug-repro.rs` — a standalone program
with no dependency on this project, so it can be handed to upstream as-is.

**Fixed in our fork** as of 2026-11-07 — `LXK-98/rquickshare`, branch
`fix/ec-point-left-pad`, rev `7b42f7d`, branched at `17702ed` with this as its
only commit. See docs/DECISIONS.md#d-012. Still present in both
`nozwock/rquickshare` (`c9ac93f`) and `Martichou/rquickshare` (`378d8ae`).

**The fix is three lines** — left-pad a short coordinate to 32 bytes instead of
appending it raw. PLAN.md's standing guidance is to fork `rqs_lib` rather than
Packet if patching is needed, so this is the sanctioned path; it is not yet done,
because a fork is a maintenance commitment and the bug is rare enough to retry
around.

## A peer's post-transfer hangup is logged as an error

After a transfer finishes, the peer closing the connection is how it ends, and
the read that discovers it surfaces as `early eof`. Both `manager.rs` paths
logged that at ERROR, so every *successful* transfer left

```
ERROR TcpServer: error while handling client: early eof (Finished)
```

in the journal. Fixed in our fork (`cf71b1b`) by extending the `Finished` check
those sites already make — they suppress the synthetic `Disconnected` message
for it — to the log line too. Still present upstream.

## BleListener reports false positives

`hdl/ble.rs` treats any advertisement on service UUID `0xFE2C` as "a device is
sharing nearby". `0xFE2C` is Google's **Fast Pair** UUID, which Quick Share
shares, so Fast Pair beacons from unrelated hardware trigger it — observed four
times in one 60-second window, all from a pair of Sony earbuds.

The payloads are distinguishable: Quick Share's is 26 bytes beginning
`fc 12 8e 01` (the constant in `hdl/blea.rs`), Fast Pair's were 19 bytes
beginning `10 52`. A four-byte check would separate them.

The signal is only used as a wake-up hint, so the cost is wasted work rather
than wrong behaviour. Not yet fixed in our fork. See
docs/BLUETOOTH_FINDINGS.md.

## Discovery contacts every device it finds

Worth stating plainly, because "discovery" sounds passive: `MDnsDiscovery` only
emits an `EndpointInfo` after `TcpStream::connect(ip:port)` **succeeds**. So
browsing opens a TCP connection to every Quick Share advertiser on the network,
not just to one we intend to send to. There is no way to disable the probe
without patching the library.

## Logging

`core_lib` does `#[macro_use] extern crate log` and logs through the **`log`
crate**, not `tracing`. Its records only appear if a `log` → `tracing` bridge is
installed. `tracing_subscriber`'s default features include `tracing-log`, and
`fmt().init()` installs the bridge for us — so this works by default, and would
silently stop working if `default-features = false` were ever set on
`tracing-subscriber`. Filter with `RUST_LOG=rqs_lib=debug`.

## Build requirements

- **`protoc` must be on `PATH`** (or `$PROTOC` set). `core_lib/build.rs` calls
  `prost_build` and `.unwrap()`s the result; prost-build 0.13.5 does not bundle
  a compiler, so a missing `protoc` is a build-script panic, not a clear error.
- `bluer` needs `libdbus` development files (`pkg-config dbus-1`).

## Threading

No `Runtime::new`, no `runtime::Builder`, no `block_on`, no `spawn_blocking` and
no `std::thread::spawn` anywhere in `core_lib` — only `tokio::spawn`. A
`current_thread` runtime is therefore sufficient (D-003). Its *dependencies*
still spawn threads of their own (`mdns-sd` runs a daemon thread; `bluer` and
`btleplug` bring D-Bus machinery), so the thread count at runtime will not be 1.
