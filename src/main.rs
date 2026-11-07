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

mod cli;
mod config;
mod dbus;
mod devices;
mod meminfo;
mod network;
mod notify;
mod open;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rqs_lib::channel::{ChannelMessage, Message, MessageClient, TransferAction, TransferKind};
use rqs_lib::hdl::info::TransferPayload;
use rqs_lib::{EndpointInfo, OutboundPayload, RQS, SendInfo, Target, TransferState, Visibility};
use tokio::sync::broadcast::{Sender, error::RecvError};

use crate::config::Config;
use crate::dbus::{Command, Daemon1};
use crate::devices::{IDLE_GRACE, Medium, Registry};
use crate::meminfo::MemSnapshot;
use crate::network::NetworkMonitor;
use crate::notify::{Consent, Notifier, Urgency, format_bytes};

/// What `--help` prints.
///
/// There are no options: everything is configured through the file named below,
/// which is the point of the design. The subcommands PLAN.md's Phase 3 calls for
/// (`send`, `devices`) do not exist yet, and saying so beats a bare usage line
/// that leaves someone guessing.
const USAGE: &str = "\
quickshare-daemon — send and receive files over Quick Share

Usage:
  quickshare-daemon                     run the receiving daemon
  quickshare-daemon devices [secs]      list nearby devices (default 8s)
  quickshare-daemon send <to> <files…>  send files to a device (waits 45s for it)
  quickshare-daemon --help | --version

<to> is a device name or id as shown by `devices`.

Configuration lives in
  $XDG_CONFIG_HOME/quickshare-daemon/config.toml
which is written with defaults on first run.

Logging goes to stderr; filter it with RUST_LOG, e.g.
  RUST_LOG=quickshare_daemon=debug,rqs_lib=debug

Note: the other device must have its Quick Share sheet open to be discoverable.
`devices` and `send` currently run their own short-lived service rather than
talking to a running daemon.";

/// How long `devices` spends collecting before printing the list.
///
/// mDNS answers trickle in over a second or two, and a phone's Quick Share
/// sheet re-advertises periodically rather than instantly, so a short window
/// finds nothing and looks broken.
const DEFAULT_DISCOVERY_SECS: u64 = 8;

/// How long `send` will wait for the named device to turn up.
///
/// Much longer than the `devices` window, and for a different reason: a phone
/// advertises only while its Quick Share sheet is open, and nobody can hold
/// that open while typing a command. This is long enough to start the send and
/// then pick up the phone. `send` returns as soon as the device appears, so the
/// wait costs nothing when it is already there.
const SEND_WAIT_SECS: u64 = 45;

/// What the process was asked to do.
enum Invocation {
    /// Run the receiving daemon.
    Daemon,
    /// List nearby devices.
    Devices { window: Duration },
    /// Send files to one device.
    Send {
        target: String,
        files: Vec<PathBuf>,
        window: Duration,
    },
    /// Print something and exit successfully.
    Print(String),
}

/// Parses argv.
///
/// Hand-rolled rather than pulling in `clap`: there are three verbs and one
/// optional number, and the binary's size is something this project measures.
fn parse_args(args: &[String]) -> Result<Invocation, String> {
    let window = Duration::from_secs(DEFAULT_DISCOVERY_SECS);

    match args {
        [] => Ok(Invocation::Daemon),
        [flag] if flag == "--help" || flag == "-h" => Ok(Invocation::Print(USAGE.to_owned())),
        [flag] if flag == "--version" || flag == "-V" => Ok(Invocation::Print(format!(
            "{} {}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        ))),
        [verb] if verb == "devices" => Ok(Invocation::Devices { window }),
        [verb, secs] if verb == "devices" => match secs.parse::<u64>() {
            Ok(secs) if secs > 0 => Ok(Invocation::Devices {
                window: Duration::from_secs(secs),
            }),
            _ => Err(format!("not a number of seconds: {secs:?}")),
        },
        [verb, ..] if verb == "send" => match &args[1..] {
            [] => Err("send needs a device and at least one file".to_owned()),
            [_target] => Err("send needs at least one file".to_owned()),
            [target, files @ ..] => Ok(Invocation::Send {
                target: target.clone(),
                files: files.iter().map(PathBuf::from).collect(),
                window: Duration::from_secs(SEND_WAIT_SECS),
            }),
        },
        [other, ..] => Err(format!("unexpected argument: {other}")),
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            // 2 is the conventional exit status for a usage error, and
            // distinguishes it from the daemon failing to start.
            std::process::exit(2);
        }
    };

    if let Invocation::Print(text) = command {
        println!("{text}");
        return Ok(());
    }

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

    runtime.block_on(async move {
        match command {
            Invocation::Daemon => run().await,
            Invocation::Devices { window } => cli::devices(window).await,
            Invocation::Send {
                target,
                files,
                window,
            } => cli::send(&target, files, window).await,
            // Handled before the runtime is built.
            Invocation::Print(_) => unreachable!("printed above"),
        }
    })
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
    let (file_sender, _ble_events) = rqs
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

    let mut registry = Registry::default();
    // Outbound transfers we started on a client's behalf.
    let mut outbound: HashMap<String, OutboundTransfer> = HashMap::new();
    // Discovery's event channel. Created once and kept for the daemon's life:
    // `rqs.discovery()` takes a sender each time it starts, and reusing one
    // keeps our receiver valid across stop/start cycles.
    let (endpoint_tx, mut endpoints) = tokio::sync::broadcast::channel::<EndpointInfo>(32);
    let mut discovering = false;

    // Claiming the name is not fatal: a second daemon, or a session without a
    // bus, should still receive files. It just cannot be driven remotely.
    let (commands_tx, mut commands) = tokio::sync::mpsc::channel::<Command>(16);
    let bus = match serve_dbus(commands_tx.clone()).await {
        Ok(bus) => {
            tracing::info!(name = dbus::BUS_NAME, "serving the D-Bus API");
            Some(bus)
        }
        Err(err) => {
            tracing::warn!(
                "not serving the D-Bus API: {err:#}. Receiving still works; `devices` and \
                 `send` will fall back to their own short-lived service"
            );
            None
        }
    };
    // Emits our signals. Built from the connection, so it outlives any handler.
    let emitter = match bus.as_ref() {
        Some(connection) => {
            zbus::object_server::SignalEmitter::new(connection, dbus::OBJECT_PATH).ok()
        }
        None => None,
    };
    if let Some(connection) = bus.as_ref() {
        watch_for_departing_clients(connection.clone(), commands_tx.clone());
    }

    // Expires stale devices and stops discovery once its leases have been gone
    // for the grace period. One timer for both: neither needs to be prompt, and
    // a second interval would be a second wakeup for no benefit.
    let mut housekeeping = tokio::time::interval(IDLE_GRACE / 4);
    housekeeping.reset();

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
                    handle_event(
                        &message,
                        &config,
                        &notifier,
                        &actions,
                        &mut active,
                        emitter.as_ref(),
                        &mut outbound,
                    )
                    .await;

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
            // Discovery events, while discovery is running. The channel keeps
            // a sender alive in `endpoint_tx`, so this never resolves to Closed
            // and never busy-loops when discovery is off.
            endpoint = endpoints.recv() => {
                match endpoint {
                    Ok(endpoint) => {
                        if registry.apply(endpoint) {
                            notify_devices_changed(emitter.as_ref()).await;
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "fell behind on discovery events");
                    }
                    Err(RecvError::Closed) => unreachable!("endpoint_tx is held"),
                }
            }
            Some(command) = commands.recv() => {
                handle_command(
                    command,
                    &mut rqs,
                    &mut config,
                    &mut registry,
                    &mut discovering,
                    &endpoint_tx,
                    &file_sender,
                    &mut restart_pending,
                    &active,
                    &mut outbound,
                )
                .await;
            }
            _ = housekeeping.tick() => {
                if registry.expire() {
                    notify_devices_changed(emitter.as_ref()).await;
                }
                if discovering && registry.should_stop_discovery() {
                    tracing::info!("stopping discovery; no client has wanted it for a while");
                    rqs.stop_discovery();
                    discovering = false;
                }
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

    // A transfer that just finished may still be sending its graceful
    // Disconnection frame: `rqs_lib` announces Finished before sending it, and
    // cancelling now would drop it, leaving the receiver to treat a complete
    // transfer as failed. See the note in cli.rs.
    if !outbound.is_empty() || !active.is_empty() {
        tracing::debug!(
            outbound = outbound.len(),
            inbound = active.len(),
            "letting in-flight transfers close cleanly before stopping"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
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

/// Claims [`dbus::BUS_NAME`] and serves the interface.
async fn serve_dbus(commands: tokio::sync::mpsc::Sender<Command>) -> Result<zbus::Connection> {
    let connection = zbus::connection::Builder::session()
        .context("connecting to the session bus")?
        .name(dbus::BUS_NAME)
        .context("claiming the bus name")?
        .serve_at(dbus::OBJECT_PATH, Daemon1::new(commands))
        .context("serving the object")?
        .build()
        .await
        .context("building the connection")?;
    Ok(connection)
}

/// Releases a client's discovery lease when it leaves the bus.
///
/// Without this, a front-end that crashed holding a lease would pin discovery
/// on, along with its BLE advertiser, until the daemon restarted. Watching
/// `NameOwnerChanged` is what makes a lease safe to hand out at all.
fn watch_for_departing_clients(
    connection: zbus::Connection,
    commands: tokio::sync::mpsc::Sender<Command>,
) {
    tokio::spawn(async move {
        use futures_util::StreamExt;

        let proxy = match zbus::fdo::DBusProxy::new(&connection).await {
            Ok(proxy) => proxy,
            Err(err) => {
                tracing::warn!("cannot watch for departing clients: {err}");
                return;
            }
        };
        let mut changes = match proxy.receive_name_owner_changed().await {
            Ok(changes) => changes,
            Err(err) => {
                tracing::warn!("cannot subscribe to NameOwnerChanged: {err}");
                return;
            }
        };

        while let Some(signal) = changes.next().await {
            let Ok(args) = signal.args() else { continue };
            // A name losing its owner and gaining none means that client is
            // gone. Only unique names interest us: a well-known name changing
            // owner says nothing about our leases.
            let departed = args.name.as_str();
            if args.new_owner.is_none() && departed.starts_with(':') {
                let _ = commands
                    .send(Command::ClientGone {
                        holder: departed.to_owned(),
                    })
                    .await;
            }
        }
    });
}

/// Emits `DevicesChanged`, if there is a bus to emit it on.
async fn notify_devices_changed(emitter: Option<&zbus::object_server::SignalEmitter<'_>>) {
    if let Some(emitter) = emitter {
        if let Err(err) = Daemon1::devices_changed(emitter).await {
            tracing::debug!("could not emit DevicesChanged: {err}");
        }
    }
}

/// Applies one [`Command`] from a D-Bus client.
#[allow(clippy::too_many_arguments)]
async fn handle_command(
    command: Command,
    rqs: &mut RQS,
    config: &mut Config,
    registry: &mut Registry,
    discovering: &mut bool,
    endpoint_tx: &Sender<EndpointInfo>,
    file_sender: &tokio::sync::mpsc::Sender<SendInfo>,
    restart_pending: &mut bool,
    active: &HashMap<String, ActiveTransfer>,
    outbound: &mut HashMap<String, OutboundTransfer>,
) {
    match command {
        Command::StartDiscovery { holder, reply } => {
            let first = registry.acquire_lease(&holder);
            let mut outcome = Ok(());

            // `acquire_lease` reports the first lease, but discovery may still
            // be running inside its grace period from a previous one -- in
            // which case there is nothing to start.
            if first && !*discovering {
                match rqs.discovery(endpoint_tx.clone()) {
                    Ok(()) => {
                        *discovering = true;
                        tracing::info!(%holder, "started discovery");
                    }
                    Err(err) => {
                        // Drop the lease again: holding one for discovery that
                        // never started would block a later attempt.
                        registry.release_lease(&holder);
                        outcome = Err(format!("{err:#}"));
                    }
                }
            } else {
                tracing::debug!(%holder, "joined running discovery");
            }

            let _ = reply.send(outcome);
        }

        Command::StopDiscovery { holder } => {
            registry.release_lease(&holder);
            // Not stopped here: the grace period is handled by housekeeping, so
            // a menu that closes and reopens does not restart mDNS.
            tracing::debug!(%holder, "released discovery lease");
        }

        Command::ClientGone { holder } => {
            // `release_lease` is already a no-op for a holder that had none, so
            // every departing client can be passed through unconditionally.
            if registry.has_leases() {
                registry.release_lease(&holder);
                tracing::debug!(%holder, "client left the bus; released its lease");
            }
        }

        Command::ListDevices { reply } => {
            let _ = reply.send(registry.list());
        }

        Command::GetVisible { reply } => {
            let _ = reply.send(config.visible);
        }

        Command::SetVisible { visible, reply } => {
            if config.visible == visible {
                let _ = reply.send(Ok(()));
                return;
            }
            config.visible = visible;
            // `change_visibility` alone would do it, but the service has to be
            // restarted anyway for the mDNS advertisement to match, and a
            // deferred restart is the only safe kind mid-transfer.
            rqs.change_visibility(if visible {
                Visibility::Visible
            } else {
                Visibility::Invisible
            });
            *restart_pending = !restart_service(rqs, config, active).await;
            tracing::info!(visible, deferred = *restart_pending, "visibility changed");
            let _ = reply.send(Ok(()));
        }

        Command::SendFiles {
            target,
            paths,
            reply,
        } => {
            let _ =
                reply.send(start_outbound(registry, file_sender, &target, &paths, outbound).await);
        }
    }
}

/// Resolves `target` and hands the files to `rqs_lib`.
///
/// Returns the transfer id, which is `rqs_lib`'s `"ip:port"` endpoint id -- the
/// only identifier it has. Note that it is not stable across the device
/// re-advertising; see docs/RQS_LIB_NOTES.md.
async fn start_outbound(
    registry: &Registry,
    file_sender: &tokio::sync::mpsc::Sender<SendInfo>,
    target: &str,
    paths: &[PathBuf],
    outbound: &mut HashMap<String, OutboundTransfer>,
) -> Result<String, String> {
    // Validated before anything is handed over, so a bad path fails as an
    // error to the caller rather than as a transfer that dies later.
    for path in paths {
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(format!(
                    "{} is not a regular file; Quick Share cannot send directories",
                    path.display()
                ));
            }
            Err(err) => return Err(format!("cannot send {}: {err}", path.display())),
        }
    }

    let device = registry
        .get(target)
        .cloned()
        .or_else(|| {
            let wanted = target.to_lowercase();
            let mut matches = registry.list().into_iter().filter(|device| {
                device
                    .name
                    .as_deref()
                    .is_some_and(|name| name.to_lowercase() == wanted)
            });
            // Exactly one, or nothing: sending a file to the wrong device is
            // not recoverable, so an ambiguous name is never resolved by
            // guessing.
            match (matches.next(), matches.next()) {
                (Some(only), None) => Some(only),
                _ => None,
            }
        })
        .ok_or_else(|| format!("no single device matches {target:?}"))?;

    // The daemon picks the medium, not the caller: it is the only component
    // that knows what is reachable and what each one costs.
    let address = device
        .best_address()
        .cloned()
        .ok_or_else(|| format!("{} has no address yet", device.display_name()))?;
    let target = match address.medium {
        Medium::WifiLan => Target::WifiLan {
            addr: address.address,
        },
        // Named in `Medium` so a device's reachability can be described, but
        // there is no transport behind it yet.
        Medium::Bluetooth => {
            return Err(format!(
                "{} is only reachable over Bluetooth, which is not implemented yet",
                device.display_name()
            ));
        }
    };

    file_sender
        .send(SendInfo {
            id: device.id.clone(),
            name: device.display_name().to_owned(),
            target,
            ob: OutboundPayload::Files(
                paths
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect(),
            ),
        })
        .await
        .map_err(|_| "the Quick Share service is not accepting transfers".to_owned())?;

    // Recorded before any event can arrive, so the notifications have a name
    // to use. The events themselves only echo back the name we supplied.
    outbound.insert(
        device.id.clone(),
        OutboundTransfer {
            device: device.display_name().to_owned(),
            ..Default::default()
        },
    );

    Ok(device.id)
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
#[allow(clippy::too_many_arguments)]
async fn handle_event(
    message: &ChannelMessage,
    config: &Config,
    notifier: &Arc<Notifier>,
    actions: &Sender<ChannelMessage>,
    active: &mut HashMap<String, ActiveTransfer>,
    emitter: Option<&zbus::object_server::SignalEmitter<'_>>,
    outbound: &mut HashMap<String, OutboundTransfer>,
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

    // Outbound transfers report over D-Bus instead of through a consent
    // prompt: the person being asked to accept is at the *other* device, so
    // there is nothing to ask here.
    if *kind == TransferKind::Outbound {
        handle_outbound_event(id, client, emitter, outbound, notifier).await;
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
                // Files get their own notification, with buttons.
                Some(TransferPayload::Files(files)) => {
                    let paths: Vec<PathBuf> = files
                        .iter()
                        // rqs_lib reports bare file names, not paths, and
                        // reports the *original* name even when a collision
                        // made it write `name (1).ext`. Joining naively would
                        // point the Open button at the pre-existing file.
                        .map(|name| open::resolve_received(&config.download_dir, name))
                        .collect();
                    spawn_received_files_notification(
                        Arc::clone(notifier),
                        source.to_owned(),
                        paths,
                    );
                    return;
                }
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

/// Posts the "files received" notification and acts on its buttons.
///
/// Spawned rather than awaited: the user may be away from the machine when a
/// file lands, and GNOME keeps the notification in its list long after it
/// leaves the screen. The event loop must not be held open for that.
///
/// The one-hour timeout is a backstop against tasks accumulating, not a
/// deadline anyone should notice. After it, the notification stays in GNOME's
/// list but its buttons stop doing anything.
fn spawn_received_files_notification(notifier: Arc<Notifier>, source: String, paths: Vec<PathBuf>) {
    const BUTTON_LIFETIME: Duration = Duration::from_secs(60 * 60);

    tokio::spawn(async move {
        let Some(first) = paths.first() else { return };

        let summary = match paths.len() {
            1 => format!("Received {}", file_name(&first.to_string_lossy())),
            n => format!("Received {n} files"),
        };
        let body = format!("From {source}");

        // "Open" only makes sense for a single file; for several, the folder is
        // the only sensible destination. `reveal` selects them all.
        let actions: &[&str] = if paths.len() == 1 {
            &["open", "Open", "folder", "Show in Folder"]
        } else {
            &["folder", "Show in Folder"]
        };

        let chosen = match notifier
            .show_with_actions(&summary, &body, actions, Urgency::Normal, BUTTON_LIFETIME)
            .await
        {
            Ok(chosen) => chosen,
            Err(err) => {
                tracing::warn!("could not post the completion notification: {err:#}");
                return;
            }
        };

        match chosen.as_deref() {
            Some("open") => {
                if let Err(err) = open::open_file(first).await {
                    // Refused (an executable kind) or xdg-open is missing.
                    // Showing the file is the useful thing to do instead of
                    // nothing.
                    tracing::warn!("{err:#}");
                    if let Err(err) = open::reveal(&paths).await {
                        tracing::warn!("could not reveal the file either: {err:#}");
                    }
                }
            }
            Some("folder") => {
                if let Err(err) = open::reveal(&paths).await {
                    tracing::warn!("could not show the files in the file manager: {err:#}");
                }
            }
            Some(other) => tracing::debug!(action = other, "unrecognised action"),
            None => {}
        }
    });
}

/// What the daemon tracks about an outbound transfer it started.
///
/// Only enough to report it: the PIN must be announced exactly once, and
/// whether any bytes moved is what distinguishes "the receiver never accepted"
/// from "the connection dropped" — both of which arrive as `Disconnected`. See
/// docs/RQS_LIB_NOTES.md.
#[derive(Debug, Default)]
struct OutboundTransfer {
    /// The recipient's name, recorded when the transfer was started.
    ///
    /// Taken from the device we resolved rather than from the events, which
    /// carry the name we supplied and would leave nothing to show if a
    /// transfer somehow arrived unrecorded.
    device: String,
    pin_announced: bool,
    started_sending: bool,
    /// The notification showing this transfer, replaced in place as it
    /// progresses so one transfer produces one notification.
    notification: Option<u32>,
}

/// Reports an outbound transfer's progress over D-Bus.
///
/// `rqs_lib`'s outbound path never emits `WaitingForUserConsent`, so the PIN
/// has to be picked out of whichever state change happens to carry it.
async fn handle_outbound_event(
    id: &str,
    client: &MessageClient,
    emitter: Option<&zbus::object_server::SignalEmitter<'_>>,
    outbound: &mut HashMap<String, OutboundTransfer>,
    notifier: &Arc<Notifier>,
) {
    let state = client.state.clone().unwrap_or_default();
    let entry = outbound.entry(id.to_owned()).or_default();
    // Falls back only if a transfer somehow arrives unrecorded; `start_outbound`
    // normally seeds this.
    let device = if entry.device.is_empty() {
        "the device".to_owned()
    } else {
        entry.device.clone()
    };

    if !entry.pin_announced {
        if let Some(pin) = client.metadata.as_ref().and_then(|m| m.pin_code.as_deref()) {
            entry.pin_announced = true;
            tracing::info!(%id, %pin, "outbound transfer awaiting acceptance");
            if let Some(emitter) = emitter {
                if let Err(err) = Daemon1::transfer_pin(emitter, id, pin).await {
                    tracing::debug!("could not emit TransferPin: {err}");
                }
            }

            // The PIN has to reach whoever is standing at the other device, and
            // a client driving this from a file manager has nowhere to print
            // it. Critical urgency so GNOME does not fade it before they look.
            match notifier
                .show(
                    &format!("Accept on {device}"),
                    &format!("PIN {pin} — check the device shows the same code"),
                    Urgency::Critical,
                )
                .await
            {
                Ok(notification) => entry.notification = Some(notification),
                Err(err) => tracing::warn!(%id, "could not post the PIN notification: {err:#}"),
            }
        }
    }

    let finish = |succeeded: bool, error: String| (succeeded, error);
    let outcome = match state {
        TransferState::SendingFiles => {
            entry.started_sending = true;
            None
        }
        TransferState::Finished => Some(finish(true, String::new())),
        TransferState::Rejected => Some(finish(false, "the device declined the transfer".into())),
        TransferState::Cancelled => Some(finish(false, "the transfer was cancelled".into())),
        // One state, two causes, told apart by how far it got. No PIN at all
        // means the key exchange failed rather than anyone refusing.
        TransferState::Disconnected if !entry.pin_announced => Some(finish(
            false,
            "the connection failed during the encrypted handshake".into(),
        )),
        TransferState::Disconnected if !entry.started_sending => Some(finish(
            false,
            "the device never accepted the transfer -- it timed out, or was declined".into(),
        )),
        TransferState::Disconnected => {
            Some(finish(false, "the device disconnected mid-transfer".into()))
        }
        _ => None,
    };

    let Some((succeeded, error)) = outcome else {
        return;
    };

    let notification = entry.notification;
    outbound.remove(id);
    tracing::info!(%id, succeeded, error = %error, "outbound transfer ended");

    if let Some(emitter) = emitter {
        if let Err(err) = Daemon1::transfer_finished(emitter, id, succeeded, &error).await {
            tracing::debug!("could not emit TransferFinished: {err}");
        }
    }

    let (summary, body) = if succeeded {
        (format!("Sent to {device}"), String::new())
    } else {
        // The reason is the whole value of a failure notification: "couldn't
        // send" on its own tells the user nothing they can act on.
        (format!("Couldn't send to {device}"), error.clone())
    };
    let urgency = if succeeded {
        Urgency::Low
    } else {
        Urgency::Normal
    };

    // Replaces the PIN notification where there was one, so a transfer leaves
    // one notification behind rather than two.
    let posted = match notification {
        Some(existing) => notifier.replace(existing, &summary, &body, urgency).await,
        None => notifier.show(&summary, &body, urgency).await,
    };
    if let Err(err) = posted {
        tracing::warn!(%id, "could not post the outbound result notification: {err:#}");
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
