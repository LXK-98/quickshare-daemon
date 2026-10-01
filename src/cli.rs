// SPDX-License-Identifier: GPL-3.0-or-later
//! One-shot `devices` and `send` commands.
//!
//! These run their own short-lived Quick Share service rather than talking to a
//! running daemon. That is deliberate for now: it exercises the outbound path
//! without the D-Bus API existing yet, and a one-shot send is independently
//! useful for scripting on a machine where the daemon is not enabled.
//!
//! PLAN.md's Phase 3 CLI is a thin D-Bus client over the running daemon. That
//! is still the goal — it avoids a second mDNS stack and a second TCP listener
//! — and these will grow a "talk to the daemon if one is running" path once the
//! API exists. Until then this is the only way to send anything.
//!
//! Both commands advertise as **invisible**: sending needs discovery, not
//! discoverability, and a one-shot should not briefly appear as a second
//! receiver beside a daemon that may already be running.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rqs_lib::channel::TransferKind;
use rqs_lib::{EndpointInfo, OutboundPayload, RQS, SendInfo, TransferState, Visibility};
use tokio::sync::broadcast;

use crate::config::Config;
use crate::devices::{Device, Registry};
use crate::notify::format_bytes;

/// A started service, plus the channels needed to drive it.
struct Session {
    rqs: RQS,
    file_sender: tokio::sync::mpsc::Sender<SendInfo>,
    endpoints: broadcast::Receiver<EndpointInfo>,
}

/// Starts a short-lived service with discovery running.
async fn start_session() -> Result<Session> {
    let (config, _) = Config::load_or_create()?;

    let mut rqs = RQS::new(
        // Invisible: see the module docs.
        Visibility::Invisible,
        // A random port, never the configured one, which a running daemon may
        // already hold.
        None,
        Some(config.download_dir.clone()),
        Some(config.device_name.clone()),
    );

    let (file_sender, _ble) = rqs
        .run()
        .await
        .context("starting the Quick Share service")?;

    // `discovery` requires `run` to have happened first, and takes the sender
    // half; we keep a receiver.
    let (endpoint_tx, endpoints) = broadcast::channel::<EndpointInfo>(32);
    rqs.discovery(endpoint_tx)
        .context("starting mDNS discovery")?;

    Ok(Session {
        rqs,
        file_sender,
        endpoints,
    })
}

/// Collects devices for `window`, reporting each new one as it appears.
///
/// Always waits the full window rather than returning on the first device:
/// mDNS answers trickle in, and a picker that showed only the fastest responder
/// would be worse than one that took two seconds.
async fn discover(session: &mut Session, window: Duration, verbose: bool) -> Registry {
    let mut registry = Registry::default();
    let deadline = tokio::time::Instant::now() + window;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, session.endpoints.recv()).await {
            // Window elapsed.
            Err(_) => break,
            Ok(Ok(endpoint)) => {
                let id = endpoint.id.clone();
                if registry.apply(endpoint) && verbose {
                    if let Some(device) = registry.get(&id) {
                        eprintln!(
                            "  found {} ({})",
                            device.display_name(),
                            device.addr.as_deref().unwrap_or("resolving…")
                        );
                    }
                }
            }
            // Lagged, or the sender went away; neither is worth aborting over.
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                tracing::warn!(skipped = n, "fell behind on discovery events");
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
        }
    }

    registry
}

/// `quickshare-daemon devices` — list what is nearby.
pub async fn devices(window: Duration) -> Result<()> {
    let mut session = start_session().await?;
    eprintln!("Looking for nearby devices for {}s…", window.as_secs());

    let registry = discover(&mut session, window, true).await;
    let found = registry.list();

    session.rqs.stop().await;

    if found.is_empty() {
        eprintln!(
            "No devices found. Quick Share needs the other device's sheet to be open: \
             on Android, share something and leave the Quick Share picker on screen."
        );
        return Ok(());
    }

    println!("{:<24}  {:<22}  ID", "NAME", "ADDRESS");
    for device in found {
        println!(
            "{:<24}  {:<22}  {}",
            device.display_name(),
            device.addr.as_deref().unwrap_or("-"),
            device.id,
        );
    }

    Ok(())
}

/// Resolves a user-typed target to exactly one reachable device.
///
/// Accepts an exact id or a case-insensitive name. Ambiguity is an error rather
/// than a guess: sending a file to the wrong device is not recoverable.
fn resolve_target(registry: &Registry, target: &str) -> Result<Device> {
    if let Some(device) = registry.get(target) {
        return Ok(device.clone());
    }

    let wanted = target.to_lowercase();
    let matches: Vec<Device> = registry
        .list()
        .into_iter()
        .filter(|d| {
            d.name
                .as_deref()
                .is_some_and(|n| n.to_lowercase() == wanted)
        })
        .collect();

    match matches.len() {
        0 => {
            let known = registry.list();
            if known.is_empty() {
                bail!("no devices found; run `quickshare-daemon devices` to look again");
            }
            bail!(
                "no device matching {target:?}. Found: {}",
                known
                    .iter()
                    .map(Device::display_name)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        1 => Ok(matches.into_iter().next().expect("length checked")),
        n => bail!(
            "{n} devices are called {target:?}; use an id instead:\n{}",
            matches
                .iter()
                .map(|d| format!("  {}  {}", d.id, d.display_name()))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

/// Waits until `target` appears and is reachable, or `deadline` passes.
///
/// Unlike [`devices`], this returns the moment the wanted device shows up
/// instead of collecting for a fixed window: there is nothing to gain from
/// waiting once the recipient is known.
///
/// The deadline is generous because a phone only advertises while its Quick
/// Share sheet is open, and nobody can hold that open while typing a command.
/// Waiting lets the user start the send first and then pick up the phone — the
/// failure that prompted this, on 2026-10-01, was a sheet closing between a
/// successful `devices` run and the `send` that followed it.
async fn find_target(session: &mut Session, target: &str, deadline: Duration) -> Result<Device> {
    let mut registry = Registry::default();
    let until = tokio::time::Instant::now() + deadline;
    let mut announced = false;

    loop {
        // Only a reachable match will do: an entry without an address cannot be
        // connected to, and reporting success here would fail a moment later.
        if let Ok(device) = resolve_target(&registry, target) {
            if device.is_reachable() {
                return Ok(device);
            }
        }

        let remaining = until.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            // Report what *was* found: "not found" is much less useful than
            // "found these instead", especially with a mistyped name.
            return resolve_target(&registry, target).and_then(|device| {
                bail!(
                    "{} was found but never became reachable",
                    device.display_name()
                )
            });
        }

        if !announced {
            eprintln!(
                "Waiting up to {}s for {target}. Open its Quick Share sheet now if it is not \
                 already open.",
                deadline.as_secs()
            );
            announced = true;
        }

        match tokio::time::timeout(remaining, session.endpoints.recv()).await {
            Ok(Ok(endpoint)) => {
                registry.apply(endpoint);
            }
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                tracing::warn!(skipped = n, "fell behind on discovery events");
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                bail!("discovery stopped unexpectedly")
            }
            // Deadline reached; the next loop turn reports it.
            Err(_) => {}
        }
    }
}

/// `quickshare-daemon send <target> <files…>` — send files to one device.
pub async fn send(target: &str, files: Vec<PathBuf>, window: Duration) -> Result<()> {
    if files.is_empty() {
        bail!("no files given");
    }

    // Checked before starting a service: a typo in a filename should fail
    // immediately, not after ten seconds of discovery.
    let mut total_bytes = 0u64;
    for file in &files {
        let metadata =
            std::fs::metadata(file).with_context(|| format!("cannot send {}", file.display()))?;
        if !metadata.is_file() {
            bail!(
                "{} is not a regular file; Quick Share cannot send directories",
                file.display()
            );
        }
        total_bytes += metadata.len();
    }

    let mut session = start_session().await?;

    let device = match find_target(&mut session, target, window).await {
        Ok(device) => device,
        Err(err) => {
            session.rqs.stop().await;
            return Err(err);
        }
    };

    // `find_target` only returns reachable devices.
    let addr = device
        .addr
        .clone()
        .expect("find_target returns only reachable devices");

    // Subscribe before sending, so no early state change is missed.
    let mut events = session.rqs.message_sender.subscribe();

    eprintln!(
        "Sending {} ({}) to {}…",
        describe_files(&files),
        format_bytes(total_bytes),
        device.display_name()
    );

    session
        .file_sender
        .send(SendInfo {
            id: device.id.clone(),
            name: device.display_name().to_owned(),
            addr,
            ob: OutboundPayload::Files(
                files
                    .iter()
                    .map(|f| f.to_string_lossy().into_owned())
                    .collect(),
            ),
        })
        .await
        .context("handing the files to the Quick Share service")?;

    let outcome = follow_transfer(&mut events, &device).await;
    session.rqs.stop().await;
    outcome
}

/// Reports progress until the outbound transfer ends.
///
/// The overall timeout is generous because the receiving side has to accept the
/// transfer by hand, and a person reaching for their phone is slow.
async fn follow_transfer(
    events: &mut broadcast::Receiver<rqs_lib::channel::ChannelMessage>,
    device: &Device,
) -> Result<()> {
    const OVERALL_TIMEOUT: Duration = Duration::from_secs(300);

    // The outbound path never emits `WaitingForUserConsent` — verified in the
    // pinned source, which has zero occurrences of it in hdl/outbound.rs. The
    // PIN arrives in the metadata of ordinary state changes instead, so it has
    // to be picked up from whichever event carries it first.
    let mut pin_shown = false;
    // Whether the receiver ever accepted. Used to tell "never accepted" from a
    // genuine mid-transfer drop, which are the same `Disconnected` state.
    let mut started_sending = false;

    let result = tokio::time::timeout(OVERALL_TIMEOUT, async {
        loop {
            let message = match events.recv().await {
                Ok(message) => message,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    bail!("the Quick Share service stopped unexpectedly")
                }
            };

            // Our own replies echo back on this channel; only library-to-client
            // messages are interesting. See docs/RQS_LIB_NOTES.md.
            let Some(client) = message.msg.as_client() else {
                continue;
            };
            if client.kind != TransferKind::Outbound {
                continue;
            }

            let state = client.state.clone().unwrap_or_default();
            tracing::debug!(id = %message.id, ?state, "outbound event");

            // Shown as soon as it exists, whatever state carried it: the
            // transfer cannot proceed until someone taps Accept on the other
            // device, and they need the PIN to know it is really us.
            if !pin_shown {
                if let Some(pin) = client.metadata.as_ref().and_then(|m| m.pin_code.as_deref()) {
                    eprintln!(
                        "\n  ACCEPT ON {} — PIN {pin}\n  (check the phone shows the same PIN)",
                        device.display_name().to_uppercase()
                    );
                    pin_shown = true;
                }
            }

            match state {
                TransferState::SendingFiles => {
                    started_sending = true;
                    if let Some(m) = &client.metadata {
                        if m.total_bytes > 0 {
                            let percent = m.ack_bytes * 100 / m.total_bytes;
                            eprint!(
                                "\r  {percent:>3}%  {} / {}   ",
                                format_bytes(m.ack_bytes),
                                format_bytes(m.total_bytes)
                            );
                        }
                    }
                }
                TransferState::Finished => {
                    eprintln!("\rSent to {}.                    ", device.display_name());
                    return Ok(());
                }
                TransferState::Rejected => {
                    bail!("{} declined the transfer", device.display_name())
                }
                TransferState::Cancelled => bail!("the transfer was cancelled"),
                // One state, three very different causes, told apart by how far
                // the handshake got.
                //
                // No PIN means the key exchange never completed, so the
                // receiver was never asked anything — blaming the user for not
                // accepting would be wrong, and is what this code did before.
                TransferState::Disconnected if !pin_shown => bail!(
                    "the connection to {} failed during the encrypted handshake, before it \
                     could ask anyone to accept anything. Look for an rqs_lib ERROR line \
                     above. If it mentions \"elliptic curve point encoding\", that is a known \
                     intermittent bug affecting roughly 1 connection in 262 — retrying \
                     usually works. See docs/RQS_LIB_NOTES.md.",
                    device.display_name()
                ),
                // A PIN was shown but no bytes moved: the receiver really was
                // asked and did not agree.
                TransferState::Disconnected if !started_sending => bail!(
                    "{} never accepted the transfer — it timed out, or was declined. \
                     Accept it on the device while this is running.",
                    device.display_name()
                ),
                TransferState::Disconnected => {
                    bail!("{} disconnected mid-transfer", device.display_name())
                }
                // Handshake states.
                _ => {}
            }
        }
    })
    .await;

    match result {
        Ok(outcome) => outcome,
        Err(_) => bail!(
            "gave up waiting after {}s; {} never completed the transfer",
            OVERALL_TIMEOUT.as_secs(),
            device.display_name()
        ),
    }
}

/// "3 files", or the single file's name.
fn describe_files(files: &[PathBuf]) -> String {
    match files {
        [one] => one
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| one.display().to_string()),
        many => format!("{} files", many.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each entry gets a distinct address: devices sharing one are deduplicated
    /// as a single phone, so a fixture reusing an address would not describe the
    /// several devices it claims to.
    fn registry_with(entries: &[(&str, Option<&str>)]) -> Registry {
        let mut registry = Registry::default();
        for (index, (id, name)) in entries.iter().enumerate() {
            registry.apply(EndpointInfo {
                fullname: String::new(),
                id: (*id).to_owned(),
                name: name.map(str::to_owned),
                ip: Some(format!("10.0.0.{}", index + 1)),
                port: Some("99".to_owned()),
                rtype: None,
                present: Some(true),
            });
        }
        registry
    }

    #[test]
    fn resolves_by_id_and_by_name() {
        let registry = registry_with(&[("abc123", Some("Pixel 9"))]);

        assert_eq!(resolve_target(&registry, "abc123").unwrap().id, "abc123");
        assert_eq!(resolve_target(&registry, "Pixel 9").unwrap().id, "abc123");
        // Case-insensitive, because nobody types a device name exactly.
        assert_eq!(resolve_target(&registry, "pixel 9").unwrap().id, "abc123");
    }

    #[test]
    fn an_ambiguous_name_is_an_error_not_a_guess() {
        let registry = registry_with(&[("a", Some("Pixel")), ("b", Some("Pixel"))]);

        let err = resolve_target(&registry, "Pixel").unwrap_err().to_string();
        assert!(err.contains("2 devices"), "{err}");
        // The ids must be offered, or the user cannot resolve the ambiguity.
        assert!(err.contains('a') && err.contains('b'), "{err}");
    }

    #[test]
    fn an_unknown_target_lists_what_was_found() {
        let registry = registry_with(&[("a", Some("Pixel"))]);

        let err = resolve_target(&registry, "Laptop").unwrap_err().to_string();
        assert!(err.contains("Pixel"), "{err}");
    }

    #[test]
    fn no_devices_at_all_says_so() {
        let err = resolve_target(&Registry::default(), "Pixel")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no devices found"), "{err}");
    }

    #[test]
    fn describes_what_is_being_sent() {
        assert_eq!(describe_files(&[PathBuf::from("/tmp/a.png")]), "a.png");
        assert_eq!(
            describe_files(&[PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.png")]),
            "2 files"
        );
    }
}
