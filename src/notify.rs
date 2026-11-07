// SPDX-License-Identifier: GPL-3.0-or-later
//! Desktop notifications, and the accept/deny prompt built on them.
//!
//! This is the daemon's entire user interface. It speaks
//! `org.freedesktop.Notifications` directly over the session bus rather than
//! going through the XDG portal that Packet uses (`ashpd`): the portal's
//! notification API is aimed at sandboxed apps, and we are deliberately not
//! sandboxed. The raw spec also gives us `ActionInvoked` and
//! `NotificationClosed`, which is what makes buttons on a notification
//! workable.
//!
//! Verified against GNOME Shell 50.4, whose `GetCapabilities` reports
//! `actions` and `persistence`. Servers are not required to support `actions`
//! — see [`Notifier::supports_actions`] for what to do when one does not.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use zbus::zvariant::Value;

/// How the user answered a consent prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    Accept,
    Decline,
    /// Nobody answered within the timeout. Treated as a decline, but recorded
    /// separately so the log can tell "declined" from "ignored".
    TimedOut,
}

/// The reason a notification went away, from the spec's `NotificationClosed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseReason {
    Expired,
    Dismissed,
    ByCall,
    Undefined,
}

impl From<u32> for CloseReason {
    fn from(raw: u32) -> Self {
        match raw {
            1 => Self::Expired,
            2 => Self::Dismissed,
            3 => Self::ByCall,
            _ => Self::Undefined,
        }
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    /// The spec's `Notify`. Returns the server's id for the notification,
    /// which is what `ActionInvoked` and `CloseNotification` refer to.
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// Sends notifications and waits for the user to answer them.
pub struct Notifier {
    proxy: NotificationsProxy<'static>,
    capabilities: Vec<String>,
}

impl Notifier {
    /// Connects to the session bus and asks the server what it can do.
    pub async fn new() -> Result<Self> {
        let connection = zbus::Connection::session()
            .await
            .context("connecting to the session bus")?;
        let proxy = NotificationsProxy::new(&connection)
            .await
            .context("reaching org.freedesktop.Notifications")?;

        // Asked once at startup, not per notification: it is a round trip, and
        // a notification server does not grow new capabilities while running.
        let capabilities = proxy.get_capabilities().await.unwrap_or_else(|err| {
            tracing::warn!("could not read notification capabilities: {err}");
            Vec::new()
        });

        tracing::debug!(?capabilities, "notification server");

        Ok(Self {
            proxy,
            capabilities,
        })
    }

    /// Whether the server draws action buttons.
    ///
    /// When it does not, a consent prompt cannot be answered from the
    /// notification, and [`Notifier::ask_consent`] declines on timeout with
    /// nothing the user can click. Callers should check this at startup and
    /// say so loudly, because the daemon is then effectively unusable for
    /// receiving files.
    pub fn supports_actions(&self) -> bool {
        self.capabilities.iter().any(|c| c == "actions")
    }

    /// Shows a plain notification and returns its id.
    pub async fn show(&self, summary: &str, body: &str, urgency: Urgency) -> Result<u32> {
        self.notify_inner(summary, body, &[], urgency, Expiry::ServerDefault, 0)
            .await
    }

    /// Replaces an earlier notification in place, keeping its position.
    ///
    /// Used for outbound transfers: the "waiting to be accepted" message and
    /// the result that supersedes it are one event, and reusing the id avoids a
    /// stack of notifications about a single transfer.
    ///
    /// Removed once as unused when completion notifications turned out not to
    /// need it; back because outbound ones do.
    pub async fn replace(
        &self,
        id: u32,
        summary: &str,
        body: &str,
        urgency: Urgency,
    ) -> Result<u32> {
        self.notify_inner(summary, body, &[], urgency, Expiry::ServerDefault, id)
            .await
    }

    /// Closes a notification.
    pub async fn close(&self, id: u32) -> Result<()> {
        self.proxy
            .close_notification(id)
            .await
            .with_context(|| format!("closing notification {id}"))
    }

    /// Shows an accept/deny prompt and waits for an answer.
    ///
    /// Dismissing the notification counts as a decline: swiping away a request
    /// is an answer, and the safe reading of it is no.
    pub async fn ask_consent(
        &self,
        summary: &str,
        body: &str,
        timeout: Duration,
    ) -> Result<Consent> {
        let outcome = self
            .await_action(
                summary,
                body,
                &["accept", "Accept", "decline", "Decline"],
                Urgency::Critical,
                // Never expire on the server's schedule: `timeout` is ours, and
                // a prompt that vanished on its own would leave the sending
                // phone waiting with nothing on screen to explain it.
                Expiry::Never,
                timeout,
            )
            .await?;

        Ok(match outcome {
            Outcome::Chosen(key) if key == "accept" => Consent::Accept,
            Outcome::Chosen(_) | Outcome::Dismissed => Consent::Decline,
            Outcome::TimedOut => Consent::TimedOut,
        })
    }

    /// Shows a notification carrying action buttons and waits for one.
    ///
    /// `actions` is flat (key, label) pairs, as the spec wants them. Returns
    /// the key that was pressed, or `None` if the notification was dismissed or
    /// the timeout ran out.
    ///
    /// Used for the "Open" / "Show in Folder" buttons on a completed transfer,
    /// where a long timeout is appropriate — the user may not be at the machine
    /// when the file lands, and GNOME keeps the notification in its list.
    pub async fn show_with_actions(
        &self,
        summary: &str,
        body: &str,
        actions: &[&str],
        urgency: Urgency,
        timeout: Duration,
    ) -> Result<Option<String>> {
        let outcome = self
            .await_action(
                summary,
                body,
                actions,
                urgency,
                Expiry::ServerDefault,
                timeout,
            )
            .await?;

        Ok(match outcome {
            Outcome::Chosen(key) => Some(key),
            Outcome::Dismissed | Outcome::TimedOut => None,
        })
    }

    /// Posts a notification with buttons and resolves on the first of: a button
    /// press, a dismissal, or the timeout.
    ///
    /// The signal streams are subscribed **before** the notification is sent.
    /// The other order is a race: a fast answer on a notification that is
    /// already on screen can be signalled before the subscription exists, and
    /// the wait would then hang until it timed out.
    async fn await_action(
        &self,
        summary: &str,
        body: &str,
        actions: &[&str],
        urgency: Urgency,
        expiry: Expiry,
        timeout: Duration,
    ) -> Result<Outcome> {
        let mut invoked = self
            .proxy
            .receive_action_invoked()
            .await
            .context("subscribing to ActionInvoked")?;
        let mut closed = self
            .proxy
            .receive_notification_closed()
            .await
            .context("subscribing to NotificationClosed")?;

        let id = self
            .notify_inner(summary, body, actions, urgency, expiry, 0)
            .await?;

        let outcome = tokio::time::timeout(timeout, async {
            loop {
                tokio::select! {
                    Some(signal) = invoked.next() => {
                        let args = match signal.args() {
                            Ok(args) => args,
                            Err(err) => {
                                tracing::warn!("malformed ActionInvoked: {err}");
                                continue;
                            }
                        };
                        // Both streams carry every notification's signals, not
                        // just ours.
                        if args.id != id {
                            continue;
                        }
                        // `default` is the spec's key for clicking the body
                        // rather than a button, and is not one of ours.
                        if args.action_key == "default" {
                            continue;
                        }
                        return Outcome::Chosen(args.action_key.to_owned());
                    }
                    Some(signal) = closed.next() => {
                        let Ok(args) = signal.args() else { continue };
                        if args.id != id {
                            continue;
                        }
                        let reason = CloseReason::from(args.reason);
                        tracing::debug!(?reason, "notification closed");
                        match reason {
                            // Swiped away by the user: an answer.
                            CloseReason::Dismissed => return Outcome::Dismissed,
                            // We closed it, or it expired. Neither is the user
                            // answering, so let the timeout decide.
                            _ => continue,
                        }
                    }
                    else => {
                        // Both streams ended, so the bus connection is gone.
                        tracing::warn!("notification signal streams ended");
                        return Outcome::TimedOut;
                    }
                }
            }
        })
        .await
        .unwrap_or(Outcome::TimedOut);

        // Take it off screen either way: it has been answered, or it is stale
        // and no longer answerable.
        if let Err(err) = self.close(id).await {
            tracing::debug!("could not close notification {id}: {err:#}");
        }

        Ok(outcome)
    }

    /// The single place `Notify` is called, so the app name, icon and hints
    /// stay consistent across every notification the daemon sends.
    async fn notify_inner(
        &self,
        summary: &str,
        body: &str,
        actions: &[&str],
        urgency: Urgency,
        expiry: Expiry,
        replaces_id: u32,
    ) -> Result<u32> {
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("urgency", Value::U8(urgency as u8));
        // Groups the daemon's notifications under one heading in GNOME's
        // notification list, and is what the spec says to send.
        hints.insert("category", Value::Str("transfer".into()));
        hints.insert("desktop-entry", Value::Str(env!("CARGO_PKG_NAME").into()));

        self.proxy
            .notify(
                "Quick Share",
                replaces_id,
                // A themed icon name, not a path. This one ships with
                // adwaita-icon-theme, so it resolves without us installing
                // anything.
                "send-to-symbolic",
                summary,
                body,
                actions,
                hints,
                expiry.as_millis(),
            )
            .await
            .context("sending a notification")
    }
}

/// What ended a wait on a notification's buttons.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// A button was pressed; carries its key.
    Chosen(String),
    /// The user swiped the notification away.
    Dismissed,
    /// Nobody answered in time.
    TimedOut,
}

/// The spec's `urgency` hint.
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum Urgency {
    Low = 0,
    Normal = 1,
    /// Critical notifications are not auto-dismissed by GNOME, which is what a
    /// prompt waiting for an answer needs.
    Critical = 2,
}

/// The spec's `expire_timeout`, which overloads one integer with three
/// meanings.
#[derive(Debug, Clone, Copy)]
enum Expiry {
    /// `-1`: whatever the server thinks is right.
    ServerDefault,
    /// `0`: never expire on its own.
    Never,
}

impl Expiry {
    fn as_millis(self) -> i32 {
        match self {
            Self::ServerDefault => -1,
            Self::Never => 0,
        }
    }
}

/// Formats a byte count for a notification body.
///
/// Deliberately coarse: this goes in a one-line message a person glances at,
/// where "1.4 MB" is more use than "1,438,214 bytes". Uses SI units, matching
/// what Android shows on the sending side.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "kB", "MB", "GB", "TB", "PB", "EB"];

    if bytes < 1000 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }

    // One decimal below 10 (1.4 MB), none above (14 MB): the extra digit stops
    // mattering as the number grows.
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_byte_counts_for_humans() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1000), "1.0 kB");
        assert_eq!(format_bytes(1_438_214), "1.4 MB");
        assert_eq!(format_bytes(14_382_140), "14 MB");
        assert_eq!(format_bytes(5_000_000_000), "5.0 GB");
        // u64::MAX is ~18.4 exabytes. Absurd for a file transfer, but the
        // units table goes far enough that it reads as a number rather than
        // as seven digits of terabytes.
        assert_eq!(format_bytes(u64::MAX), "18 EB");
    }

    #[test]
    fn close_reasons_match_the_spec() {
        assert_eq!(CloseReason::from(1), CloseReason::Expired);
        assert_eq!(CloseReason::from(2), CloseReason::Dismissed);
        assert_eq!(CloseReason::from(3), CloseReason::ByCall);
        assert_eq!(CloseReason::from(4), CloseReason::Undefined);
        assert_eq!(CloseReason::from(99), CloseReason::Undefined);
    }

    #[test]
    fn expiry_encodes_the_specs_sentinels() {
        assert_eq!(Expiry::ServerDefault.as_millis(), -1);
        assert_eq!(Expiry::Never.as_millis(), 0);
    }
}
