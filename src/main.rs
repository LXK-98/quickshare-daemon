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

//! Receives files over Quick Share, asking for consent through a desktop
//! notification.
//!
//! Structure: a single-threaded tokio runtime owns [`rqs_lib::RQS`] and a
//! `select!` loop over its transfer events. Everything that has to wait on a
//! human — the accept/deny prompt — is spawned off that loop so a pending
//! prompt cannot stall a concurrent transfer.
//!
//! Configuration lives in `config.toml`; see [`config::Config`]. One
//! environment variable remains, for development only:
//!
//! - `QSD_MEASURE_SECS` — memory measurement interval (default: 60)

mod config;
mod meminfo;
mod network;
mod notify;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rqs_lib::channel::{ChannelMessage, Message, MessageClient, TransferAction, TransferKind};
use rqs_lib::hdl::info::TransferPayload;
use rqs_lib::{RQS, TransferState, Visibility};
use tokio::sync::broadcast::{Sender, error::RecvError};

use crate::config::Config;
use crate::meminfo::MemSnapshot;
use crate::network::NetworkMonitor;
use crate::notify::{Consent, Notifier, Urgency, format_bytes};

fn main() -> Result<()> {
    init_tracing();

    // A current_thread runtime, not Packet's multi-threaded one. rqs_lib only
    // ever calls `tokio::spawn`, never `block_on`, `spawn_blocking` or
    // `std::thread::spawn` (verified in the pinned checkout), so it does not
    // need worker threads of its own. Each thread we avoid is a stack plus,
    // under glibc, potentially another malloc arena. See
    // docs/DECISIONS.md#d-003.
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

/// What the daemon remembers about a transfer while it is in flight.
///
/// Only what the *final* notification needs: by the time a transfer finishes,
/// its metadata no longer says who sent it, and "Received 3 files" without a
/// sender is a worse message than we can afford to send.
#[derive(Debug)]
struct ActiveTransfer {
    source: String,
    /// Whether consent was given. A transfer that ends without it needs no
    /// closing notification — the user already answered, and telling them that
    /// the thing they declined did not happen is noise.
    accepted: bool,
}

async fn run() -> Result<()> {
    let (config, config_path) = Config::load_or_create()?;
    config.ensure_download_dir()?;
    tracing::info!(path = %config_path.display(), "loaded configuration");

    if let Some(false) = config.port_is_available() {
        // Not fatal: rqs_lib does the real bind, and the port may free up.
        tracing::warn!(
            port = ?config.port,
            "the configured port is already in use; the service will probably fail to start"
        );
    }

    let notifier = Arc::new(Notifier::new().await?);
    if !notifier.supports_actions() {
        tracing::error!(
            "this notification server does not support action buttons, so incoming \
             transfers cannot be accepted. Every request will be declined on timeout. \
             Add senders to auto_accept_from in {} to work around it",
            config_path.display()
        );
    }

    // `RQS::new` only stores configuration; nothing is bound or advertised
    // until `run()`.
    let mut rqs = RQS::new(
        if config.visible {
            Visibility::Visible
        } else {
            Visibility::Invisible
        },
        config.port.map(u32::from),
        Some(config.download_dir.clone()),
        Some(config.device_name.clone()),
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
        visible = config.visible,
        download_dir = %config.download_dir.display(),
        "Quick Share service started"
    );

    let mut active: HashMap<String, ActiveTransfer> = HashMap::new();
    // A restart asked for while transfers were in flight, run as soon as the
    // last one ends.
    let mut restart_pending = false;

    // The first measurement, taken after mDNS and BLE have settled, is the
    // number PLAN.md's target is about. Recorded in docs/MEASUREMENTS.md.
    let mut settled = Box::pin(tokio::time::sleep(Duration::from_secs(5)));
    let mut measure = tokio::time::interval(measure_interval());
    measure.reset(); // do not fire immediately; the settle timer covers boot

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing the SIGTERM handler")?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .context("installing the SIGHUP handler")?;

    // Not fatal if NetworkManager is absent: plenty of systems use systemd-networkd
    // or nothing at all, and the daemon works fine until the network changes
    // under it.
    let mut network = match NetworkMonitor::new().await {
        Ok(monitor) => {
            tracing::debug!(state = ?monitor.state(), "watching NetworkManager");
            Some(monitor)
        }
        Err(err) => {
            tracing::warn!(
                "not watching for network changes: {err:#}. The service will not be \
                 restarted when the network reconnects; restart the daemon by hand if \
                 it stops being discoverable"
            );
            None
        }
    };

    // Held across a deferred restart. `rqs_lib` bakes these in at start time,
    // so a restart must use the same values the running service was given.
    let mut config = config;

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(message) => {
                    handle_event(&message, &config, &notifier, &actions, &mut active).await;

                    // The deferred restart from a reconnect or a SIGHUP that
                    // arrived mid-transfer. `active` empties only here, so this
                    // is the one place the restart can become due.
                    if restart_pending && active.is_empty() {
                        restart_pending = !restart_service(&mut rqs, &config, &active).await;
                    }
                }
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
                settled
                    .as_mut()
                    .reset(tokio::time::Instant::now() + Duration::from_secs(86_400));
            }
            _ = measure.tick() => {
                if let Ok(snap) = MemSnapshot::read() {
                    tracing::debug!(%snap, "footprint");
                }
            }
            // `wait_for_reconnect` never resolves when there is no monitor,
            // so this branch simply never fires in that case.
            _ = async { match network.as_mut() {
                Some(monitor) => monitor.wait_for_reconnect().await,
                None => std::future::pending().await,
            } } => {
                restart_pending = !restart_service(&mut rqs, &config, &active).await;
            }
            _ = sighup.recv() => {
                match Config::load_or_create() {
                    Ok((reloaded, _)) => {
                        let restart_needed = reloaded.needs_service_restart(&config);
                        if let Err(err) = reloaded.ensure_download_dir() {
                            tracing::warn!("reloaded config's download_dir is unusable: {err:#}");
                        }
                        // Applied without a restart; rqs_lib reads this per
                        // transfer through a global set by the setter.
                        rqs.set_download_path(Some(reloaded.download_dir.clone()));
                        config = reloaded;
                        tracing::info!(restart_needed, "reloaded configuration");

                        if restart_needed {
                            restart_pending =
                                !restart_service(&mut rqs, &config, &active).await;
                        }
                    }
                    Err(err) => {
                        tracing::error!("keeping the running configuration: {err:#}");
                    }
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

/// The memory measurement interval. Development knob; not in the config file.
fn measure_interval() -> Duration {
    Duration::from_secs(
        std::env::var("QSD_MEASURE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60),
    )
}

/// Stops and restarts the Quick Share service, unless a transfer is running.
///
/// Called on reconnect and after a config change that `rqs_lib` only reads at
/// start time. Returns `false` if it declined because transfers are in flight,
/// which tells the caller to remember the request and come back once the last
/// one ends — restarting mid-transfer would drop a transfer the user is
/// watching.
///
/// Deferring on a flag rather than retrying on a timer (which is what Packet
/// does) means the restart happens the moment the last transfer ends, with no
/// interval to guess at and no possibility of a retry loop starving.
#[must_use = "a declined restart has to be retried once the transfers end"]
async fn restart_service(
    rqs: &mut RQS,
    config: &Config,
    active: &HashMap<String, ActiveTransfer>,
) -> bool {
    if !active.is_empty() {
        tracing::info!(
            transfers = active.len(),
            "deferring the Quick Share service restart until transfers finish"
        );
        return false;
    }

    tracing::info!("restarting the Quick Share service");
    rqs.stop().await;

    // `rqs_lib` reuses the port it last bound, even one it chose at random, so
    // a restart keeps the same port whether or not the config fixes one.
    if let Err(err) = rqs.run().await {
        // The daemon is now running with no service. Nothing here can fix
        // that, but the next reconnect or SIGHUP is the retry.
        tracing::error!("could not restart the Quick Share service: {err:#}");
        return true;
    }

    rqs.change_visibility(if config.visible {
        Visibility::Visible
    } else {
        Visibility::Invisible
    });

    tracing::info!(port = ?rqs.port_number, "Quick Share service restarted");
    true
}

/// Reacts to one message from `rqs_lib`.
///
/// Consent is delivered back through the *same* broadcast channel the events
/// arrive on ([`Message::Lib`]), which means our own replies are echoed back to
/// us. Handling only [`Message::Client`] is what keeps that from looping.
async fn handle_event(
    message: &ChannelMessage,
    config: &Config,
    notifier: &Arc<Notifier>,
    actions: &Sender<ChannelMessage>,
    active: &mut HashMap<String, ActiveTransfer>,
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

    // Outbound transfers are Phase 3. Until then they cannot happen, but
    // reacting to them as if they were inbound would be worse than ignoring
    // them.
    if *kind == TransferKind::Outbound {
        tracing::debug!(%id, ?state, "ignoring outbound transfer event (Phase 3)");
        return;
    }

    let state = state.clone().unwrap_or_default();
    tracing::debug!(%id, ?state, "transfer event");

    match state {
        TransferState::WaitingForUserConsent => {
            let source = metadata
                .as_ref()
                .and_then(|m| m.source.as_ref())
                .map(|s| s.name.clone())
                .unwrap_or_else(|| "An unknown device".to_owned());
            let bytes = metadata.as_ref().map_or(0, |m| m.total_bytes);
            let description = metadata
                .as_ref()
                .map_or_else(|| "something".to_owned(), describe_payload);

            if config.is_auto_accepted(&source) {
                tracing::info!(%id, %source, "auto-accepting: sender is in auto_accept_from");
                active.insert(
                    id.clone(),
                    ActiveTransfer {
                        source: source.clone(),
                        accepted: true,
                    },
                );
                send_action(actions, id, TransferAction::ConsentAccept);

                // Say so anyway. An automatic transfer the user never hears
                // about is indistinguishable from one they did not authorise.
                let _ = notifier
                    .show(
                        &format!("Receiving {description}"),
                        &format!(
                            "From {source} · {} · accepted automatically",
                            format_bytes(bytes)
                        ),
                        Urgency::Low,
                    )
                    .await;
                return;
            }

            active.insert(
                id.clone(),
                ActiveTransfer {
                    source: source.clone(),
                    // Set when the answer comes back.
                    accepted: false,
                },
            );

            // The prompt can wait up to `accept_timeout_secs`, so it must not
            // run on the event loop: a second sender arriving meanwhile would
            // otherwise not be handled until the first prompt was answered.
            //
            // The consequence is that the answer arrives as a plain
            // `Message::Lib` send with no ordering guarantee relative to other
            // events, which is exactly how rqs_lib expects consent to arrive.
            let notifier = Arc::clone(notifier);
            let actions = actions.clone();
            let id = id.clone();
            let timeout = Duration::from_secs(config.accept_timeout_secs);
            let pin = metadata.as_ref().and_then(|m| m.pin_code.clone());

            tokio::spawn(async move {
                let summary = format!("{source} wants to send {description}");
                let mut body = format_bytes(bytes);
                // The PIN is how the user confirms the sender is the device in
                // front of them rather than a stranger on the same network.
                if let Some(pin) = pin {
                    body.push_str(" · PIN ");
                    body.push_str(&pin);
                }

                let consent = match notifier.ask_consent(&summary, &body, timeout).await {
                    Ok(consent) => consent,
                    Err(err) => {
                        // A broken notification path must not leave the sender
                        // hanging; refusing is the safe direction to fail.
                        tracing::error!(%id, "could not ask for consent, declining: {err:#}");
                        Consent::Decline
                    }
                };

                tracing::info!(%id, ?consent, "consent decision");
                send_action(
                    &actions,
                    &id,
                    match consent {
                        Consent::Accept => TransferAction::ConsentAccept,
                        Consent::Decline | Consent::TimedOut => TransferAction::ConsentDecline,
                    },
                );
            });
        }

        TransferState::ReceivingFiles => {
            // First progress event means consent was accepted and the transfer
            // is underway.
            if let Some(transfer) = active.get_mut(id) {
                transfer.accepted = true;
            }
            if let Some(m) = metadata {
                tracing::trace!(%id, ack = m.ack_bytes, total = m.total_bytes, "progress");
            }
        }

        TransferState::Finished => {
            let transfer = active.remove(id);
            let source = transfer
                .as_ref()
                .map_or("an unknown device", |t| t.source.as_str());
            let payload = metadata.as_ref().and_then(|m| m.payload.as_ref());

            tracing::info!(%id, %source, payload = ?payload, "transfer finished");

            let (summary, body) = match payload {
                Some(TransferPayload::Text(text)) => (
                    format!("Text from {source}"),
                    // The whole point is to read it, and GNOME truncates long
                    // bodies itself.
                    text.clone(),
                ),
                Some(TransferPayload::Url(url)) => (format!("Link from {source}"), url.clone()),
                // Not offered as a "connect" action: joining a network is a
                // system-level change, and the password is better read off the
                // notification than acted on automatically.
                Some(TransferPayload::Wifi { ssid, password, .. }) => (
                    format!("Wi-Fi details from {source}"),
                    format!("{ssid} · {password}"),
                ),
                Some(TransferPayload::Files(files)) => (
                    match files.len() {
                        1 => format!("Received {}", file_name(&files[0])),
                        n => format!("Received {n} files"),
                    },
                    format!("From {source} · in {}", config.download_dir.display()),
                ),
                None => (format!("Received something from {source}"), String::new()),
            };

            if let Err(err) = notifier.show(&summary, &body, Urgency::Normal).await {
                tracing::warn!(%id, "could not post the completion notification: {err:#}");
            }
        }

        TransferState::Rejected | TransferState::Cancelled | TransferState::Disconnected => {
            let transfer = active.remove(id);
            tracing::info!(%id, ?state, "transfer ended without completing");

            // Only worth a notification if it had been accepted and then
            // failed. A transfer the user declined ending is not news.
            if let Some(transfer) = transfer.filter(|t| t.accepted) {
                let reason = match state {
                    TransferState::Cancelled => "the sender cancelled it",
                    TransferState::Disconnected => "the connection dropped",
                    _ => "it was rejected",
                };
                if let Err(err) = notifier
                    .show(
                        &format!("Transfer from {} failed", transfer.source),
                        &format!("Incomplete: {reason}."),
                        Urgency::Normal,
                    )
                    .await
                {
                    tracing::warn!(%id, "could not post the failure notification: {err:#}");
                }
            }
        }

        // Protocol handshake states; noise for our purposes.
        _ => {}
    }
}

/// A short phrase for what is being sent, for the consent prompt.
///
/// Reads the *kind* rather than the payload, because at consent time the
/// payload has not arrived yet — only its description has.
fn describe_payload(metadata: &rqs_lib::hdl::info::TransferMetadata) -> String {
    use rqs_lib::hdl::info::TransferPayloadKind;

    match metadata.payload_kind {
        TransferPayloadKind::Text => "some text".to_owned(),
        TransferPayloadKind::Url => "a link".to_owned(),
        TransferPayloadKind::WiFi => "Wi-Fi details".to_owned(),
        // The count is not in the metadata at consent time, so this stays
        // vague on purpose rather than guessing at "1 file".
        TransferPayloadKind::Files => "files".to_owned(),
    }
}

/// The final component of a path `rqs_lib` reported, for display.
fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned())
}

/// Sends a consent decision back to the transfer handler waiting on it.
fn send_action(actions: &Sender<ChannelMessage>, id: &str, action: TransferAction) {
    let message = ChannelMessage {
        id: id.to_owned(),
        msg: Message::Lib { action },
    };
    // Fails only when nothing is subscribed. The transfer handler subscribes
    // for the life of the transfer, so this means the transfer is already gone
    // — the sender hung up, or it timed out on their side.
    if actions.send(message).is_err() {
        tracing::warn!(%id, "no transfer left to receive the decision");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_reduced_to_their_last_component() {
        assert_eq!(file_name("~/Downloads/photo.jpg"), "photo.jpg");
        assert_eq!(file_name("photo.jpg"), "photo.jpg");
        // Not a usable name, but better echoed back than turned into "".
        assert_eq!(file_name("/"), "/");
        assert_eq!(file_name(""), "");
    }
}
