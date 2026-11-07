// SPDX-License-Identifier: GPL-3.0-or-later
//! The daemon's D-Bus API, `org.quickshare.Daemon1`.
//!
//! Every front-end goes through this: the CLI, and later a Nautilus extension
//! or a GNOME Shell extension (docs/EXTENSION_PLAN.md). It is the only public
//! surface, so the three decisions it was designed around are worth restating:
//!
//! - **Signals, not polling.** A client that polls `ListDevices` is wasteful
//!   anywhere and unacceptable inside `gnome-shell`.
//! - **`a{sv}` dicts, not fixed tuples.** Fields can be added without breaking
//!   existing clients.
//! - **Discovery is leased, not toggled.** A global on/off breaks the moment
//!   two front-ends exist: one closing its menu would stop discovery out from
//!   under the other.
//!
//! The interface holds no state. Handlers send a [`Command`] into the daemon's
//! event loop and await a reply, which keeps the loop the single owner of
//! [`rqs_lib::RQS`] — `zbus` handlers are separate entry points that would each
//! otherwise need `&mut RQS`. See docs/DECISIONS.md#d-005.

use std::collections::HashMap;
use std::path::PathBuf;

use tokio::sync::{mpsc, oneshot};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

use crate::devices::Device;

/// The well-known name the daemon claims.
pub const BUS_NAME: &str = "org.quickshare.Daemon1";

/// The object implementing [`BUS_NAME`].
pub const OBJECT_PATH: &str = "/org/quickshare/Daemon1";

/// A request from a D-Bus client to the daemon's event loop.
///
/// Each variant carries a `oneshot` for its reply, so a handler can report the
/// real outcome rather than acknowledging blindly.
#[derive(Debug)]
pub enum Command {
    /// Take a discovery lease for `holder`, starting discovery if it is the
    /// first.
    StartDiscovery {
        holder: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Release `holder`'s lease. Discovery keeps running for a grace period.
    StopDiscovery { holder: String },
    /// The currently known devices.
    ListDevices { reply: oneshot::Sender<Vec<Device>> },
    /// Start an outbound transfer. Replies with the transfer id.
    SendFiles {
        target: String,
        paths: Vec<PathBuf>,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Read the current visibility.
    GetVisible { reply: oneshot::Sender<bool> },
    /// Change visibility. Restarts the service, since `rqs_lib` takes it at
    /// start time.
    SetVisible {
        visible: bool,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// A client dropped off the bus; release anything it held.
    ///
    /// This is what makes leases safe. Without it, a front-end that crashes
    /// with a lease open would pin discovery on — and its BLE advertiser —
    /// until the daemon restarted.
    ClientGone { holder: String },
}

/// Renders a device as the `a{sv}` dict clients receive.
///
/// `reachable` is included rather than left for the client to infer from
/// `address` being empty: whether a device can be sent to is our judgement, not
/// a detail of how we happen to represent it.
fn device_dict(device: &Device) -> HashMap<String, OwnedValue> {
    let mut dict = HashMap::new();
    let mut insert = |key: &str, value: Value<'_>| {
        if let Ok(owned) = OwnedValue::try_from(value) {
            dict.insert(key.to_owned(), owned);
        }
    };

    insert("id", Value::from(device.id.clone()));
    insert("name", Value::from(device.display_name().to_owned()));
    insert(
        "address",
        Value::from(device.addr.clone().unwrap_or_default()),
    );
    insert("reachable", Value::from(device.is_reachable()));
    dict
}

/// The `org.quickshare.Daemon1` implementation.
pub struct Daemon1 {
    commands: mpsc::Sender<Command>,
}

impl Daemon1 {
    pub fn new(commands: mpsc::Sender<Command>) -> Self {
        Self { commands }
    }

    /// Sends a command and awaits its reply.
    ///
    /// A closed channel or a dropped reply means the event loop is gone, which
    /// from a client's point of view is the daemon shutting down.
    async fn request<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> zbus::fdo::Result<T> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(build(tx))
            .await
            .map_err(|_| zbus::fdo::Error::Failed("the daemon is shutting down".into()))?;
        rx.await
            .map_err(|_| zbus::fdo::Error::Failed("the daemon did not answer".into()))
    }

    /// Identifies the caller, for lease bookkeeping.
    ///
    /// The unique name (`:1.42`) is the right key: it is assigned by the bus,
    /// cannot be spoofed by the client, and disappears when the client does,
    /// which is exactly the lifetime a lease should have.
    fn caller(header: &zbus::message::Header<'_>) -> zbus::fdo::Result<String> {
        header
            .sender()
            .map(|name| name.to_string())
            .ok_or_else(|| zbus::fdo::Error::Failed("caller has no unique name".into()))
    }
}

#[zbus::interface(name = "org.quickshare.Daemon1")]
impl Daemon1 {
    /// Starts discovering nearby devices, and keeps it running until this
    /// client calls `StopDiscovery` or leaves the bus.
    ///
    /// Safe to call repeatedly; a client holds at most one lease.
    async fn start_discovery(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let holder = Self::caller(&header)?;
        self.request(|reply| Command::StartDiscovery {
            holder: holder.clone(),
            reply,
        })
        .await?
        .map_err(zbus::fdo::Error::Failed)
    }

    /// Releases this client's discovery lease.
    ///
    /// Discovery continues if another client still holds one, and for a short
    /// grace period afterwards so a reopened menu does not restart mDNS.
    async fn stop_discovery(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let holder = Self::caller(&header)?;
        self.commands
            .send(Command::StopDiscovery { holder })
            .await
            .map_err(|_| zbus::fdo::Error::Failed("the daemon is shutting down".into()))
    }

    /// The devices currently known, most recently seen first.
    ///
    /// Returns an empty list rather than an error when discovery is not
    /// running: "nothing found" and "not looking" are the same thing to a
    /// client that is about to call `StartDiscovery`.
    async fn list_devices(&self) -> zbus::fdo::Result<Vec<HashMap<String, OwnedValue>>> {
        let devices = self.request(|reply| Command::ListDevices { reply }).await?;
        Ok(devices.iter().map(device_dict).collect())
    }

    /// Sends files to a device, named by id or by name.
    ///
    /// Returns the transfer id, which later appears in
    /// [`Daemon1::transfer_finished`]. Returns promptly: the transfer itself is
    /// asynchronous, and a receiver has to accept it by hand.
    async fn send_files(&self, target: &str, paths: Vec<String>) -> zbus::fdo::Result<String> {
        if paths.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs("no files given".into()));
        }

        let target = target.to_owned();
        let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
        self.request(|reply| Command::SendFiles {
            target: target.clone(),
            paths: paths.clone(),
            reply,
        })
        .await?
        .map_err(zbus::fdo::Error::Failed)
    }

    /// Whether this machine is discoverable by nearby devices.
    ///
    /// Writing it restarts the Quick Share service, because `rqs_lib` takes
    /// visibility at start time. The write is deferred while a transfer is in
    /// flight, so it may not take effect immediately.
    #[zbus(property)]
    async fn visible(&self) -> zbus::fdo::Result<bool> {
        self.request(|reply| Command::GetVisible { reply }).await
    }

    #[zbus(property)]
    async fn set_visible(&self, visible: bool) -> zbus::Result<()> {
        self.request(|reply| Command::SetVisible { visible, reply })
            .await
            .map_err(zbus::Error::from)?
            .map_err(zbus::Error::Failure)
    }

    /// The set of known devices changed.
    ///
    /// Deliberately carries no payload: a client re-reads `ListDevices`, which
    /// keeps one source of truth and avoids a client assembling a stale view
    /// out of deltas it may have missed.
    #[zbus(signal)]
    pub async fn devices_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    /// A transfer ended. `error` is empty when it succeeded.
    #[zbus(signal)]
    pub async fn transfer_finished(
        emitter: &SignalEmitter<'_>,
        id: &str,
        succeeded: bool,
        error: &str,
    ) -> zbus::Result<()>;

    /// The PIN for a transfer, as soon as the handshake derives one.
    ///
    /// A separate signal rather than a `SendFiles` return value, because the
    /// PIN does not exist yet when that call returns: it falls out of the key
    /// exchange, which happens afterwards. A client must show it — it is how
    /// the person at the other device confirms the transfer is really from
    /// here, and nothing can be accepted until they do.
    #[zbus(signal)]
    pub async fn transfer_pin(emitter: &SignalEmitter<'_>, id: &str, pin: &str)
    -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.quickshare.Daemon1",
    default_service = "org.quickshare.Daemon1",
    default_path = "/org/quickshare/Daemon1"
)]
pub trait Daemon {
    fn start_discovery(&self) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
    fn list_devices(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
    fn send_files(&self, target: &str, paths: &[&str]) -> zbus::Result<String>;

    #[zbus(property)]
    fn visible(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_visible(&self, visible: bool) -> zbus::Result<()>;

    #[zbus(signal)]
    fn devices_changed(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn transfer_finished(&self, id: String, succeeded: bool, error: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn transfer_pin(&self, id: String, pin: String) -> zbus::Result<()>;
}

/// One device as seen by a client, read back out of a [`device_dict`].
#[derive(Debug, Clone)]
pub struct RemoteDevice {
    pub id: String,
    pub name: String,
    pub reachable: bool,
}

impl RemoteDevice {
    /// Reads one dict from `ListDevices`.
    ///
    /// Missing or mistyped fields are tolerated rather than fatal: the contract
    /// allows adding fields, so a client that hard-failed on anything
    /// unexpected would break on the next extension.
    pub fn from_dict(dict: &HashMap<String, OwnedValue>) -> Option<Self> {
        let string = |key: &str| -> Option<String> {
            dict.get(key)
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| String::try_from(value).ok())
        };

        Some(Self {
            id: string("id")?,
            name: string("name").unwrap_or_else(|| "Unknown device".to_owned()),
            reachable: dict
                .get("reachable")
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| bool::try_from(value).ok())
                .unwrap_or(false),
        })
    }
}

/// Picks the one device matching `target`, by id or by name.
///
/// Mirrors the daemon's own resolution, and like it refuses to guess: sending a
/// file to the wrong device cannot be undone. Returns `Ok(None)` when nothing
/// matches yet, so a caller can keep waiting, and `Err` only when the answer is
/// ambiguous and waiting longer cannot help.
pub fn resolve_remote(
    devices: &[RemoteDevice],
    target: &str,
) -> Result<Option<RemoteDevice>, String> {
    if let Some(found) = devices.iter().find(|device| device.id == target) {
        return Ok(Some(found.clone()));
    }

    let wanted = target.to_lowercase();
    let matches: Vec<&RemoteDevice> = devices
        .iter()
        .filter(|device| device.name.to_lowercase() == wanted)
        .collect();

    match matches.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some((*only).clone())),
        many => Err(format!(
            "{} devices are called {target:?}; use an id instead:\n{}",
            many.len(),
            many.iter()
                .map(|device| format!("  {}  {}", device.id, device.name))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rqs_lib::EndpointInfo;

    use crate::devices::Registry;

    fn device(id: &str, name: Option<&str>, resolved: bool) -> Device {
        let mut registry = Registry::default();
        registry.apply(EndpointInfo {
            fullname: String::new(),
            id: id.to_owned(),
            name: name.map(str::to_owned),
            ip: resolved.then(|| "10.0.0.7".to_owned()),
            port: resolved.then(|| "4321".to_owned()),
            rtype: None,
            present: Some(true),
        });
        registry.list().into_iter().next().expect("just inserted")
    }

    fn as_string(dict: &HashMap<String, OwnedValue>, key: &str) -> String {
        String::try_from(dict[key].try_clone().unwrap()).unwrap()
    }

    #[test]
    fn a_reachable_device_renders_every_field() {
        let dict = device_dict(&device("10.0.0.7:4321", Some("Pixel 6a"), true));

        assert_eq!(as_string(&dict, "id"), "10.0.0.7:4321");
        assert_eq!(as_string(&dict, "name"), "Pixel 6a");
        assert_eq!(as_string(&dict, "address"), "10.0.0.7:4321");
        assert!(bool::try_from(dict["reachable"].try_clone().unwrap()).unwrap());
    }

    #[test]
    fn an_unresolved_device_is_reported_as_unreachable() {
        let dict = device_dict(&device("pending", None, false));

        // Empty rather than absent: a client indexing the dict must not have to
        // handle a missing key for a device that is merely not resolved yet.
        assert_eq!(as_string(&dict, "address"), "");
        assert!(!bool::try_from(dict["reachable"].try_clone().unwrap()).unwrap());
        // display_name()'s fallback, so a picker never shows an empty row.
        assert_eq!(as_string(&dict, "name"), "Unknown device");
    }

    #[test]
    fn the_dict_keys_are_stable() {
        // Clients index these by name; renaming one is a breaking change.
        let dict = device_dict(&device("a", Some("x"), true));
        let mut keys: Vec<&str> = dict.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["address", "id", "name", "reachable"]);
    }
}
