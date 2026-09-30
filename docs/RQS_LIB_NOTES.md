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
