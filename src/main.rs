// SPDX-License-Identifier: GPL-3.0-or-later
//
// quickshare-daemon — a headless Quick Share (Nearby Share) daemon for Linux.
// Copyright (C) 2026  Alex
//
// The RQS service lifecycle and the inbound-transfer handling here are adapted
// from Packet (https://github.com/nozwock/packet, GPL-3.0-or-later) — see
// `src/window.rs::setup_rqs_service` and `src/widgets/receive_transfer.rs` at
// commit f634d66 — and both projects build on rquickshare
// (https://github.com/Martichou/rquickshare, GPL-3.0-or-later).
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. See LICENSE for the full text.

//! Boots [`rqs_lib::RQS`] on a single-threaded tokio runtime and reports every
//! transfer event it produces.
//!
//! This is the Phase 1 skeleton from PLAN.md: it proves out the three things the
//! rest of the project rests on — that the build works, that a `current_thread`
//! runtime is sufficient for `rqs_lib`, and what the idle memory floor actually
//! is with both BLE stacks linked in. Config loading and desktop notifications
//! land on top of this loop; they are not here yet.
//!
//! Temporary environment overrides, replaced by `config.toml` in Phase 1:
//!
//! - `QSD_DEVICE_NAME` — name advertised to nearby devices (default: hostname)
//! - `QSD_DOWNLOAD_DIR` — where received files land (default: XDG download dir)
//! - `QSD_PORT` — fixed TCP port (default: a random one)
//! - `QSD_AUTO_ACCEPT=1` — accept every incoming transfer without asking.
//!   **Testing only.** Until notifications exist there is no way to consent, so
//!   without this the protocol cannot be exercised end to end.
//! - `QSD_MEASURE_SECS` — memory measurement interval (default: 60)

mod meminfo;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use rqs_lib::channel::{ChannelMessage, Message, MessageClient, TransferAction, TransferKind};
use rqs_lib::{RQS, TransferState, Visibility};
use tokio::sync::broadcast::error::RecvError;

use crate::meminfo::MemSnapshot;

fn main() -> Result<()> {
    init_tracing();

    // A current_thread runtime, not Packet's multi-threaded one. rqs_lib only
    // ever calls `tokio::spawn`, never `block_on`, `spawn_blocking` or
    // `std::thread::spawn` (verified in the pinned checkout), so it does not
    // need worker threads of its own. Each thread we avoid is a stack plus,
    // under glibc, potentially another malloc arena.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")?;

    runtime.block_on(run())
}

/// Installs the `tracing` subscriber for stderr.
///
/// `tracing_subscriber`'s default features include `tracing-log`, so `init()`
/// also installs a `log` bridge — which is the only reason `rqs_lib`'s output is
/// visible at all, since it logs through the `log` crate rather than `tracing`.
/// Filter it with e.g. `RUST_LOG=quickshare_daemon=debug,rqs_lib=debug`.
fn init_tracing() {
    let builder = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "quickshare_daemon=info,rqs_lib=info".into()),
        )
        .with_writer(std::io::stderr);

    // systemd sets JOURNAL_STREAM when our stderr is already the journal, which
    // timestamps every line itself. Printing our own would just duplicate it.
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.without_time().init();
    } else {
        builder.init();
    }
}

async fn run() -> Result<()> {
    let device_name = std::env::var("QSD_DEVICE_NAME").ok();
    let download_dir = download_dir()?;
    let port = match std::env::var("QSD_PORT") {
        Ok(raw) => Some(
            raw.parse::<u32>()
                .with_context(|| format!("QSD_PORT is not a port number: {raw:?}"))?,
        ),
        Err(_) => None,
    };
    let auto_accept = std::env::var("QSD_AUTO_ACCEPT").is_ok_and(|v| v == "1");
    let measure_interval = Duration::from_secs(
        std::env::var("QSD_MEASURE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60),
    );

    if auto_accept {
        tracing::warn!(
            "QSD_AUTO_ACCEPT=1: every incoming transfer will be accepted without asking"
        );
    }

    // `RQS::new` only stores configuration; nothing is bound or advertised
    // until `run()`. Visibility is hardcoded until config lands in Phase 1.
    let mut rqs = RQS::new(
        Visibility::Visible,
        port,
        Some(download_dir.clone()),
        device_name,
    );

    // Subscribe before `run()`: this is a broadcast channel, so a receiver
    // created afterwards would miss anything sent in between.
    let mut events = rqs.message_sender.subscribe();
    let actions = rqs.message_sender.clone();

    // Hold onto both of `run()`'s return values for the whole session.
    //
    // `_file_sender` is the outbound half, used in Phase 3. Keeping it alive is
    // not merely tidy: `TcpServer::run` selects on the matching receiver with a
    // `Some(i) = ...recv()` pattern, so dropping this sender would make that
    // branch resolve to `None` and be re-polled on every loop iteration.
    //
    // `_ble_events` fires when a nearby device starts sharing; Phase 3 can use
    // it to enable discovery on demand instead of running mDNS continuously.
    let (_file_sender, _ble_events) = rqs
        .run()
        .await
        .context("starting the Quick Share service")?;

    tracing::info!(
        device_name = %rqs.get_device_name(),
        port = ?rqs.port_number,
        download_dir = %download_dir.display(),
        "Quick Share service started"
    );

    // The first measurement, taken after mDNS and BLE have settled, is the
    // number PLAN.md's target is about. Recorded in docs/MEASUREMENTS.md.
    let mut settled = Box::pin(tokio::time::sleep(Duration::from_secs(5)));
    let mut measure = tokio::time::interval(measure_interval);
    measure.reset(); // do not fire immediately; the settle timer covers boot

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing the SIGTERM handler")?;

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(message) => handle_event(&message, &actions, auto_accept),
                // The channel holds 50 messages. Progress updates during a
                // large transfer are the plausible way to overrun it, and
                // losing those is survivable — but losing a consent request
                // is not, so this is worth seeing.
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "fell behind on transfer events");
                }
                // Only reachable if RQS itself is dropped, which it is not
                // while this loop owns it.
                Err(RecvError::Closed) => {
                    tracing::error!("transfer event channel closed; shutting down");
                    break;
                }
            },
            _ = &mut settled => {
                match MemSnapshot::read() {
                    Ok(snap) => tracing::info!(%snap, "idle footprint (settled)"),
                    Err(err) => tracing::warn!("could not measure memory: {err:#}"),
                }
                // Re-arm far in the future; the interval takes over from here.
                settled.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(86_400));
            }
            _ = measure.tick() => {
                if let Ok(snap) = MemSnapshot::read() {
                    tracing::debug!(%snap, "footprint");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("interrupted; shutting down");
                break;
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM; shutting down");
                break;
            }
        }
    }

    // Cancels the TCP server, mDNS and BLE tasks and waits for them to finish.
    rqs.stop().await;

    if let Ok(snap) = MemSnapshot::read() {
        tracing::info!(%snap, "footprint at shutdown");
    }

    Ok(())
}

/// Reacts to one message from `rqs_lib`.
///
/// Consent is delivered back through the *same* broadcast channel the events
/// arrive on ([`Message::Lib`]), which means our own replies are echoed back to
/// us. Handling only [`Message::Client`] is what keeps that from looping.
fn handle_event(
    message: &ChannelMessage,
    actions: &tokio::sync::broadcast::Sender<ChannelMessage>,
    auto_accept: bool,
) {
    let Some(client) = message.msg.as_client() else {
        // Our own `Message::Lib` reply, echoed back by the broadcast channel.
        return;
    };

    let id = &message.id;
    let MessageClient {
        kind,
        state,
        metadata,
    } = client;

    let state = state.clone().unwrap_or_default();
    tracing::debug!(%id, ?kind, ?state, "transfer event");

    match state {
        TransferState::WaitingForUserConsent => {
            let source = metadata
                .as_ref()
                .and_then(|m| m.source.as_ref())
                .map(|s| s.name.clone())
                .unwrap_or_else(|| "unknown device".into());
            let kind_of_payload = metadata.as_ref().map(|m| format!("{:?}", m.payload_kind));
            let bytes = metadata.as_ref().map(|m| m.total_bytes);
            let pin = metadata.as_ref().and_then(|m| m.pin_code.clone());

            tracing::info!(
                %id,
                %source,
                payload = ?kind_of_payload,
                ?bytes,
                ?pin,
                "incoming transfer awaiting consent"
            );

            // Phase 1 replaces this with a desktop notification carrying
            // accept/deny actions. Until then, consent is all-or-nothing.
            if auto_accept {
                let action = TransferAction::ConsentAccept;
                tracing::warn!(%id, ?action, "auto-accepting (QSD_AUTO_ACCEPT=1)");
                send_action(actions, id, action);
            } else {
                tracing::warn!(
                    %id,
                    "no way to ask the user yet: declining. Set QSD_AUTO_ACCEPT=1 to test transfers"
                );
                send_action(actions, id, TransferAction::ConsentDecline);
            }
        }
        TransferState::Finished => {
            let files = metadata.as_ref().and_then(|m| m.payload.clone());
            tracing::info!(%id, ?kind, payload = ?files, "transfer finished");
        }
        TransferState::Rejected | TransferState::Cancelled | TransferState::Disconnected => {
            tracing::info!(%id, ?kind, ?state, "transfer ended without completing");
        }
        // Progress. `ReceivingFiles`/`SendingFiles` repeat per chunk, so this
        // stays at trace level to keep a large transfer from flooding the log.
        TransferState::ReceivingFiles | TransferState::SendingFiles => {
            if let Some(m) = metadata {
                tracing::trace!(%id, ack = m.ack_bytes, total = m.total_bytes, "progress");
            }
        }
        // Protocol handshake states; noise for our purposes.
        _ => {}
    }

    // Silences the unused warning on `TransferKind` while only inbound is
    // implemented; Phase 3 branches on it.
    debug_assert!(matches!(
        kind,
        TransferKind::Inbound | TransferKind::Outbound
    ));
}

/// Sends a consent decision back to the transfer handler waiting on it.
fn send_action(
    actions: &tokio::sync::broadcast::Sender<ChannelMessage>,
    id: &str,
    action: TransferAction,
) {
    let message = ChannelMessage {
        id: id.to_owned(),
        msg: Message::Lib { action },
    };
    // Fails only when nothing is subscribed. The transfer handler subscribes
    // for the life of the transfer, so this means the transfer is already gone.
    if actions.send(message).is_err() {
        tracing::warn!(%id, "no transfer left to receive the decision");
    }
}

/// Resolves the directory received files are written to.
///
/// Prefers `QSD_DOWNLOAD_DIR`, then the XDG download directory, then
/// `$HOME/Downloads` — matching PLAN.md's `download_dir` default. The directory
/// is created if missing: `rqs_lib` writes into it without checking, so a fresh
/// machine without `~/Downloads` would otherwise fail mid-transfer.
fn download_dir() -> Result<PathBuf> {
    let dir = match std::env::var_os("QSD_DOWNLOAD_DIR") {
        Some(raw) => PathBuf::from(raw),
        None => dirs::download_dir()
            .or_else(|| dirs::home_dir().map(|home| home.join("Downloads")))
            .context("could not determine a download directory; set QSD_DOWNLOAD_DIR")?,
    };

    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating the download directory {}", dir.display()))?;

    Ok(dir)
}
