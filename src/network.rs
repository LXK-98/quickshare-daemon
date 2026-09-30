// SPDX-License-Identifier: GPL-3.0-or-later
//! Watches NetworkManager so the Quick Share service can be restarted when the
//! network comes back.
//!
//! `rqs_lib` binds its TCP listener and starts mDNS once, at `run()`. Neither
//! survives the interface going away and coming back with a different address:
//! the daemon stays up, looks healthy, and is invisible to every phone on the
//! network. Restarting the service on reconnect is the fix, and Packet does the
//! same thing — this is the behaviour of its `schedule_rqs_restart_on_network_up`
//! (`src/window.rs`, commit f634d66), moved from GIO's `NetworkMonitor` onto
//! NetworkManager's D-Bus interface because we have no GIO.
//!
//! The debounce is the important part. NetworkManager reports several state
//! changes while a connection comes up, so reacting to the first one restarts
//! the service into a network that is still settling.

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;

/// How long the state must hold steady before a reconnect counts.
///
/// Three seconds is what Packet settled on. Long enough to swallow the flapping
/// of a Wi-Fi association, short enough that a user who just reconnected does
/// not notice the wait.
const DEBOUNCE: Duration = Duration::from_secs(3);

/// NetworkManager's `NMState`, from its D-Bus API.
///
/// Only the distinction between "has a usable local network" and "does not"
/// matters here, but the variants are kept whole so the logs name the state
/// rather than printing a bare integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NmState {
    Unknown,
    Asleep,
    Disconnected,
    Disconnecting,
    Connecting,
    /// Connected, but only to the local link.
    ConnectedLocal,
    /// Connected to a site network, no internet.
    ConnectedSite,
    /// Connected with internet access.
    ConnectedGlobal,
}

impl From<u32> for NmState {
    fn from(raw: u32) -> Self {
        match raw {
            10 => Self::Asleep,
            20 => Self::Disconnected,
            30 => Self::Disconnecting,
            40 => Self::Connecting,
            50 => Self::ConnectedLocal,
            60 => Self::ConnectedSite,
            70 => Self::ConnectedGlobal,
            _ => Self::Unknown,
        }
    }
}

impl NmState {
    /// Whether Quick Share can work in this state.
    ///
    /// `ConnectedLocal` is enough: Quick Share is a LAN protocol built on mDNS
    /// and a direct TCP connection, and it never needs to reach the internet.
    /// Treating only `ConnectedGlobal` as usable would leave the daemon dead on
    /// exactly the isolated networks it is most useful on.
    pub fn is_usable(self) -> bool {
        matches!(
            self,
            Self::ConnectedLocal | Self::ConnectedSite | Self::ConnectedGlobal
        )
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;

    // Renamed on the Rust side: zbus derives `receive_state_changed` from both
    // the `State` property and the `StateChanged` signal, and the two collide.
    #[zbus(signal, name = "StateChanged")]
    fn nm_state_changed(&self, state: u32) -> zbus::Result<()>;
}

/// Reports when the network has come back, debounced.
pub struct NetworkMonitor {
    changes: StateChangedStream,
    /// The last state seen, so a transition can be told from a repeat.
    last: NmState,
    /// Set when the signal stream ends, which happens if NetworkManager or the
    /// bus goes away. Nothing can be reported after that.
    ended: bool,
    /// A down → up edge that has been seen but not yet reported to the caller.
    /// Survives `wait_for_reconnect` being cancelled mid-debounce.
    pending_reconnect: bool,
}

impl NetworkMonitor {
    /// Connects to NetworkManager and reads the current state.
    ///
    /// Subscribes to `StateChanged` *before* reading `State`, so a change
    /// landing between the two is seen rather than missed.
    pub async fn new() -> Result<Self> {
        let connection = zbus::Connection::system()
            .await
            .context("connecting to the system bus")?;
        let proxy = NetworkManagerProxy::new(&connection)
            .await
            .context("reaching org.freedesktop.NetworkManager")?;

        let changes = proxy
            .receive_nm_state_changed()
            .await
            .context("subscribing to NetworkManager's StateChanged")?;

        let last = NmState::from(proxy.state().await.unwrap_or(0));
        tracing::debug!(?last, "NetworkManager state at startup");

        Ok(Self {
            changes,
            last,
            ended: false,
            pending_reconnect: false,
        })
    }

    /// The state as of the last signal seen.
    pub fn state(&self) -> NmState {
        self.last
    }

    /// Resolves once the network has become usable again after not being so.
    ///
    /// Cancel-safe, which matters because `select!` drops this future every
    /// time another branch fires. A down → up edge is recorded in
    /// `pending_reconnect` the moment it is seen and before any further await,
    /// so a cancellation during the debounce window resumes the wait rather
    /// than losing the reconnect entirely.
    ///
    /// Never resolves if NetworkManager is gone. That is deliberate: the
    /// alternative is resolving with an error, which in a `select!` loop would
    /// spin at full speed. The daemon keeps working without reconnect
    /// handling, having said so once.
    pub async fn wait_for_reconnect(&mut self) {
        if self.ended {
            return std::future::pending().await;
        }

        loop {
            // Resume an edge seen before a previous cancellation, or one just
            // recorded at the bottom of this loop.
            if self.pending_reconnect {
                let usable = self.settle().await;
                self.pending_reconnect = false;
                if usable {
                    return;
                }
                // It went back down while settling. Wait for the next edge.
            }

            let Some(signal) = self.changes.next().await else {
                tracing::error!(
                    "NetworkManager's signal stream ended; no longer restarting the \
                     Quick Share service on reconnect"
                );
                self.ended = true;
                return std::future::pending().await;
            };

            let Ok(args) = signal.args() else {
                tracing::warn!("malformed StateChanged signal");
                continue;
            };

            let state = NmState::from(args.state);
            let was_usable = self.last.is_usable();
            self.last = state;
            tracing::debug!(?state, "NetworkManager state changed");

            // Only a down → up edge is interesting. Transitions between two
            // usable states (site → global, say) mean the link never dropped
            // and rqs_lib's listener is still valid.
            if !was_usable && state.is_usable() {
                self.pending_reconnect = true;
            }
        }
    }

    /// Waits out [`DEBOUNCE`] of quiet, consuming any changes that arrive.
    ///
    /// Returns whether the network is still usable once things go quiet. Each
    /// new signal restarts the window, so a connection that flaps three times
    /// produces one reconnect, not three.
    async fn settle(&mut self) -> bool {
        loop {
            match tokio::time::timeout(DEBOUNCE, self.changes.next()).await {
                // Nothing for a whole window: the state has settled.
                Err(_elapsed) => return self.last.is_usable(),
                Ok(Some(signal)) => {
                    if let Ok(args) = signal.args() {
                        self.last = NmState::from(args.state);
                        tracing::debug!(state = ?self.last, "still settling");
                    }
                }
                Ok(None) => {
                    self.ended = true;
                    return false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_networkmanager_states() {
        assert_eq!(NmState::from(0), NmState::Unknown);
        assert_eq!(NmState::from(10), NmState::Asleep);
        assert_eq!(NmState::from(20), NmState::Disconnected);
        assert_eq!(NmState::from(30), NmState::Disconnecting);
        assert_eq!(NmState::from(40), NmState::Connecting);
        assert_eq!(NmState::from(50), NmState::ConnectedLocal);
        assert_eq!(NmState::from(60), NmState::ConnectedSite);
        assert_eq!(NmState::from(70), NmState::ConnectedGlobal);
        // NetworkManager is free to add states; an unknown one must not be
        // mistaken for a usable network.
        assert_eq!(NmState::from(80), NmState::Unknown);
        assert_eq!(NmState::from(u32::MAX), NmState::Unknown);
    }

    #[test]
    fn local_connectivity_is_enough_for_quick_share() {
        assert!(NmState::ConnectedLocal.is_usable());
        assert!(NmState::ConnectedSite.is_usable());
        assert!(NmState::ConnectedGlobal.is_usable());

        assert!(!NmState::Unknown.is_usable());
        assert!(!NmState::Asleep.is_usable());
        assert!(!NmState::Disconnected.is_usable());
        assert!(!NmState::Disconnecting.is_usable());
        // Mid-connection is not yet usable: this is precisely the state that
        // would trigger a premature restart without the debounce.
        assert!(!NmState::Connecting.is_usable());
    }
}
