// SPDX-License-Identifier: GPL-3.0-or-later
//! Discovering nearby devices, and remembering them while they are around.
//!
//! Discovery is not free: it runs mDNS browsing and, on Linux with the
//! `experimental` feature, a BLE advertiser. PLAN.md therefore has it run only
//! while something needs it — the Nautilus menu asks for it when it is built,
//! and it stops once nothing has asked for a while.
//!
//! The lease mechanism here is why that works with more than one client. See
//! docs/EXTENSION_PLAN.md: a global on/off breaks the moment a second front-end
//! exists, because the first one closing its menu would stop discovery out from
//! under the second.

// The lease machinery and device expiry below are not reachable from the
// one-shot `devices`/`send` commands, which discover once and exit. They exist
// for the daemon's D-Bus API (PLAN.md Phase 3), which is the only thing that
// can hold a lease, and they are covered by tests. Refcounting is designed in
// now rather than retrofitted because a global on/off breaks as soon as a
// second front-end exists — see docs/EXTENSION_PLAN.md.
#![allow(dead_code)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rqs_lib::EndpointInfo;

/// How long discovery keeps running after the last lease is dropped.
///
/// PLAN.md says "stops discovery after ~60 s idle". The grace period matters
/// because the Nautilus menu is built, torn down and rebuilt as the user moves
/// around, and restarting mDNS browsing each time would be slower and would
/// lose the device list the user is looking at.
pub const IDLE_GRACE: Duration = Duration::from_secs(60);

/// How long a device is listed after it was last seen.
///
/// mDNS removals are not reliable — a phone that walks out of range sends
/// nothing — so entries also expire on their own.
const DEVICE_TTL: Duration = Duration::from_secs(120);

/// How long a withdrawn device stays listed before it is really dropped.
///
/// Observed 2026-10-01 against a Pixel 6a: `ServiceRemoved` arrived seconds
/// after `ServiceResolved` while the phone's Quick Share sheet was still open
/// and it was still reachable. mDNS churn like that is normal, and acting on it
/// instantly would make a device blink out of the picker exactly as the user
/// reached for it. A short grace absorbs the churn; a device that is genuinely
/// gone still disappears quickly.
const REMOVAL_GRACE: Duration = Duration::from_secs(10);

/// A medium a device can be reached over.
///
/// Mirrors the subset of Nearby Connections' `Medium` that we could plausibly
/// use. Only `WifiLan` works today; the rest are named because a device's
/// reachability is a list, and the daemon has to be able to say which entry is
/// which when it picks one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Medium {
    /// mDNS discovery plus a direct TCP connection. The only one implemented.
    WifiLan,
    /// Bluetooth RFCOMM. Reachable without a shared network, and slow.
    Bluetooth,
}

impl Medium {
    /// The name used on D-Bus, matching the protocol's own spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WifiLan => "wifi_lan",
            Self::Bluetooth => "bluetooth",
        }
    }
}

/// One way of reaching a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub medium: Medium,
    /// Medium-specific: `"ip:port"` for `WifiLan`, a MAC for `Bluetooth`.
    pub address: String,
}

/// A device we could send to.
///
/// Identity and reachability are separate: `id` says *which device*, and
/// `addresses` says *how to reach it*, possibly several ways at once. Before
/// 2026-11-07 these were one `"ip:port"` string doing both jobs, which no
/// device without an IP could satisfy. See docs/TRANSPORT_IDENTITY.md.
#[derive(Debug, Clone)]
pub struct Device {
    /// The device's advertised endpoint id.
    ///
    /// Stable across a device closing and reopening its share sheet, which is
    /// the lifetime that matters here. Not stable across days or networks, so
    /// it is never persisted.
    pub id: String,
    /// What the device calls itself. Absent until mDNS resolves the TXT
    /// record, which is why it is an `Option` rather than a placeholder.
    pub name: Option<String>,
    /// Every way this device is currently known to be reachable, best first.
    pub addresses: Vec<Address>,
    /// When this entry was last refreshed by a discovery event.
    pub last_seen: Instant,
    /// When mDNS said this device went away, if it has.
    ///
    /// Cleared the moment it is announced again. See [`REMOVAL_GRACE`].
    pub withdrawn_at: Option<Instant>,
}

impl Device {
    /// A name fit to show a person.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or("Unknown device")
    }

    /// Whether this device can actually be sent to.
    ///
    /// An entry can exist with no addresses: mDNS announces a service before
    /// resolving it. Offering such a device in a picker would produce a send
    /// that fails for reasons the user cannot act on.
    pub fn is_reachable(&self) -> bool {
        !self.addresses.is_empty()
    }

    /// The medium to use for a transfer, and its address.
    ///
    /// Prefers Wi-Fi LAN, which is both the only implemented medium and the
    /// fast one. This is the daemon's choice to make rather than a client's:
    /// only the daemon knows what is reachable and what each medium costs.
    pub fn best_address(&self) -> Option<&Address> {
        self.addresses.iter().min_by_key(|address| address.medium)
    }

    /// The address for one medium, if the device is reachable over it.
    pub fn address_for(&self, medium: Medium) -> Option<&Address> {
        self.addresses
            .iter()
            .find(|address| address.medium == medium)
    }
}

/// The set of currently-known devices, and who still wants discovery running.
#[derive(Debug, Default)]
pub struct Registry {
    devices: HashMap<String, Device>,
    /// Outstanding discovery leases, keyed by whatever the caller uses to
    /// identify itself — a D-Bus unique name, in practice.
    leases: HashMap<String, Instant>,
    /// When the last lease was released, for the idle grace period.
    idle_since: Option<Instant>,
}

impl Registry {
    /// Applies one discovery event.
    ///
    /// Returns whether the visible device list changed, so callers can avoid
    /// emitting a D-Bus signal for a refresh that changed nothing. mDNS
    /// re-announces the same device repeatedly, so most events change nothing.
    pub fn apply(&mut self, endpoint: EndpointInfo) -> bool {
        // Keyed on the device's own endpoint id, falling back to the address
        // when a peer does not advertise one.
        let key = Self::key_for(&endpoint);

        // A removal is anything that is not an explicit `Some(true)`.
        //
        // rqs_lib sends exactly two kinds of event: `ServiceResolved` builds an
        // `EndpointInfo` with `present: Some(true)` and every field filled in,
        // and `ServiceRemoved` sends `EndpointInfo { id, ..Default::default() }`
        // — so a removal carries `present: None` and *no other data*, which
        // includes no endpoint id. `Some(false)` is never sent at all.
        //
        // Reading `None` as "still here" was a bug: the empty removal event was
        // applied as an update and wiped the device's name, leaving "Unknown
        // device" in the picker. See docs/RQS_LIB_NOTES.md.

        if endpoint.present != Some(true) {
            // A removal carries no endpoint_id, so try the address key too.
            let withdrawn_key = if self.devices.contains_key(&key) {
                key
            } else {
                match self.key_by_address(&endpoint.id) {
                    Some(found) => found,
                    None => return false,
                }
            };

            return match self.devices.get_mut(&withdrawn_key) {
                // Withdrawn, not deleted: see REMOVAL_GRACE.
                Some(device) if device.withdrawn_at.is_none() => {
                    device.withdrawn_at = Some(Instant::now());
                    false
                }
                // Already withdrawn, or never known.
                _ => false,
            };
        }

        let addr = match (&endpoint.ip, &endpoint.port) {
            (Some(ip), Some(port)) => Some(Address {
                medium: Medium::WifiLan,
                address: format!("{ip}:{port}"),
            }),
            _ => None,
        };

        // No address-based deduplication any more. It existed because the key
        // *was* the address, so a device whose address changed arrived as a
        // second entry; keying on the endpoint id removes the problem rather
        // than compensating for it. See docs/TRANSPORT_IDENTITY.md.
        match self.devices.get_mut(&key) {
            Some(existing) => {
                existing.last_seen = Instant::now();
                // Announced again, so any pending withdrawal is void.
                let was_withdrawn = existing.withdrawn_at.take().is_some();
                // Never let an absent name replace a known one. Only the
                // resolve path reaches here, so a name should always be present
                // — but losing a device's name is the one failure that is
                // visible to the user, so it is guarded explicitly.
                let name_changed = endpoint.name.is_some() && existing.name != endpoint.name;
                // A new or changed address for a medium we already had
                // replaces that medium's entry; others are left alone, so a
                // Wi-Fi announcement cannot erase a Bluetooth address.
                let address_changed = match &addr {
                    Some(new) => {
                        let previous = existing.address_for(new.medium).cloned();
                        if previous.as_ref() != Some(new) {
                            existing
                                .addresses
                                .retain(|address| address.medium != new.medium);
                            existing.addresses.push(new.clone());
                            existing.addresses.sort_by_key(|address| address.medium);
                            true
                        } else {
                            false
                        }
                    }
                    // An unresolved announcement must not make a reachable
                    // device unreachable.
                    None => false,
                };

                let changed = was_withdrawn || name_changed || address_changed;
                if name_changed {
                    existing.name = endpoint.name;
                }
                changed
            }
            None => {
                self.devices.insert(
                    key.clone(),
                    Device {
                        id: key,
                        name: endpoint.name,
                        addresses: addr.into_iter().collect(),
                        last_seen: Instant::now(),
                        withdrawn_at: None,
                    },
                );
                true
            }
        }
    }

    /// The registry key for an endpoint.
    ///
    /// The device's advertised endpoint id when it has one, and otherwise its
    /// address. The fallback keeps peers that advertise no endpoint id usable
    /// rather than invisible, at the cost of the old instability for those
    /// devices only.
    fn key_for(endpoint: &EndpointInfo) -> String {
        endpoint
            .endpoint_id
            .clone()
            .unwrap_or_else(|| endpoint.id.clone())
    }

    /// Finds a device by one of its addresses.
    ///
    /// Needed because a removal event carries only `rqs_lib`'s `"ip:port"` id
    /// and no endpoint id, so a withdrawal cannot be matched by key alone.
    fn key_by_address(&self, address: &str) -> Option<String> {
        self.devices
            .iter()
            .find(|(_, device)| {
                device
                    .addresses
                    .iter()
                    .any(|candidate| candidate.address == address)
            })
            .map(|(key, _)| key.clone())
    }

    /// Drops devices that have aged out or whose withdrawal grace has passed.
    ///
    /// Returns whether anything was dropped.
    pub fn expire(&mut self) -> bool {
        let before = self.devices.len();
        self.devices.retain(|_, device| {
            let aged_out = device.last_seen.elapsed() >= DEVICE_TTL;
            let withdrawn = device
                .withdrawn_at
                .is_some_and(|at| at.elapsed() >= REMOVAL_GRACE);
            !aged_out && !withdrawn
        });
        self.devices.len() != before
    }

    /// Every currently-listable device, newest first.
    ///
    /// Most-recently-seen first because that is the most likely intended
    /// recipient — the phone the user just woke up to send something from.
    ///
    /// Devices withdrawn longer ago than [`REMOVAL_GRACE`] are left out even if
    /// [`Registry::expire`] has not run, so a caller that never expires (the
    /// one-shot CLI) still does not show a device that has gone.
    pub fn list(&self) -> Vec<Device> {
        let mut devices: Vec<Device> = self
            .devices
            .values()
            .filter(|device| {
                device
                    .withdrawn_at
                    .is_none_or(|at| at.elapsed() < REMOVAL_GRACE)
            })
            .cloned()
            .collect();
        devices.sort_by(|a, b| {
            b.last_seen
                .cmp(&a.last_seen)
                .then_with(|| a.display_name().cmp(b.display_name()))
        });
        devices
    }

    /// Looks a device up by id.
    pub fn get(&self, id: &str) -> Option<&Device> {
        self.devices.get(id)
    }

    /// Records that `holder` wants discovery running.
    ///
    /// Returns whether discovery needs starting as a result.
    pub fn acquire_lease(&mut self, holder: &str) -> bool {
        let was_idle = self.leases.is_empty();
        self.leases.insert(holder.to_owned(), Instant::now());
        self.idle_since = None;
        was_idle
    }

    /// Releases `holder`'s lease.
    ///
    /// Does not stop discovery: [`Registry::should_stop_discovery`] decides
    /// that once the grace period has elapsed.
    pub fn release_lease(&mut self, holder: &str) {
        if self.leases.remove(holder).is_some() && self.leases.is_empty() {
            self.idle_since = Some(Instant::now());
        }
    }

    /// Whether discovery has been idle long enough to stop.
    pub fn should_stop_discovery(&self) -> bool {
        self.leases.is_empty()
            && self
                .idle_since
                .is_some_and(|since| since.elapsed() >= IDLE_GRACE)
    }

    /// Whether anything currently holds a lease.
    pub fn has_leases(&self) -> bool {
        !self.leases.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an `EndpointInfo` the way `rqs_lib` would.
    ///
    /// `id` is `"ip:port"` in the real library, so an id containing a colon is
    /// taken apart accordingly. A bare label gets a synthesised *unique*
    /// address, because two entries sharing an address describe one device and
    /// are deduplicated — a fixture reusing an address would not describe what
    /// it claims to.
    fn endpoint(id: &str, name: Option<&str>, resolved: bool) -> EndpointInfo {
        endpoint_at(id, name, resolved.then(|| synthetic_address(id)))
    }

    /// A stable address derived from an endpoint id, so separate fixtures
    /// describe separate devices.
    fn synthetic_address(id: &str) -> String {
        let seed: u32 = id.bytes().map(u32::from).sum();
        format!("10.0.{}.{}:1234", seed / 256 % 256, seed % 256)
    }

    /// Builds an announcement for `id` at a specific address.
    ///
    /// `endpoint_id` is the identity and the address is separate, which is the
    /// whole point of the model: the same device can turn up at a new address
    /// without becoming a new device.
    fn endpoint_at(id: &str, name: Option<&str>, address: Option<String>) -> EndpointInfo {
        let (ip, port) = match address.as_deref().and_then(|a| a.rsplit_once(':')) {
            Some((ip, port)) => (Some(ip.to_owned()), Some(port.to_owned())),
            None => (None, None),
        };

        EndpointInfo {
            fullname: format!("{id}._FC9F5ED42C8A._tcp.local."),
            // rqs_lib synthesises this from the address.
            id: address.unwrap_or_else(|| id.to_owned()),
            endpoint_id: Some(id.to_owned()),
            name: name.map(str::to_owned),
            ip,
            port,
            rtype: None,
            present: Some(true),
        }
    }

    /// A removal exactly as `rqs_lib` sends one: the `"ip:port"` id and nothing
    /// else, with **no endpoint id**.
    fn removal(address: &str) -> EndpointInfo {
        EndpointInfo {
            id: address.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn adds_and_updates_devices() {
        let mut registry = Registry::default();

        assert!(registry.apply(endpoint("a", Some("Pixel"), true)));
        assert_eq!(registry.list().len(), 1);
        assert!(registry.get("a").unwrap().is_reachable());

        // A repeat announcement of the same thing is not a change.
        assert!(!registry.apply(endpoint("a", Some("Pixel"), true)));

        // A rename is.
        assert!(registry.apply(endpoint("a", Some("Pixel 9"), true)));
        assert_eq!(registry.get("a").unwrap().name.as_deref(), Some("Pixel 9"));
    }

    #[test]
    fn an_unresolved_announcement_does_not_erase_a_known_address() {
        let mut registry = Registry::default();
        registry.apply(endpoint("a", Some("Pixel"), true));

        let resolved = registry.get("a").unwrap().addresses.clone();

        // mDNS announces before resolving; this must not make a reachable
        // device unreachable.
        registry.apply(endpoint("a", Some("Pixel"), false));

        let device = registry.get("a").unwrap();
        assert_eq!(device.addresses, resolved);
        assert!(device.is_reachable());
    }

    #[test]
    fn unresolved_devices_are_listed_but_not_reachable() {
        let mut registry = Registry::default();
        registry.apply(endpoint("a", None, false));

        let device = &registry.list()[0];
        assert!(!device.is_reachable());
        assert_eq!(device.display_name(), "Unknown device");
    }

    #[test]
    fn a_device_that_moves_address_stays_one_device() {
        let mut registry = Registry::default();

        // The same phone on a new address -- a different network, or a new
        // port. Identity is the endpoint id, so this is an update rather than
        // a second device. Previously this needed address-based deduplication
        // to paper over the id *being* the address.
        registry.apply(endpoint_at(
            "pixel",
            Some("Pixel"),
            Some("192.168.1.5:1111".to_owned()),
        ));
        registry.apply(endpoint_at(
            "pixel",
            Some("Pixel"),
            Some("192.168.1.5:2222".to_owned()),
        ));

        let listed = registry.list();
        assert_eq!(
            listed.len(),
            1,
            "one phone must not be listed twice: {listed:?}"
        );
        assert_eq!(
            listed[0].best_address().unwrap().address,
            "192.168.1.5:2222",
            "the newer address must win"
        );
    }

    #[test]
    fn two_devices_sharing_an_address_are_still_two_devices() {
        let mut registry = Registry::default();

        // Would have collapsed into one under the old address-based dedup. It
        // should not happen on a LAN, but identity no longer depends on it not
        // happening.
        registry.apply(endpoint_at(
            "one",
            Some("Pixel"),
            Some("10.0.0.1:1".to_owned()),
        ));
        registry.apply(endpoint_at(
            "two",
            Some("Laptop"),
            Some("10.0.0.1:1".to_owned()),
        ));

        assert_eq!(registry.list().len(), 2);
    }

    #[test]
    fn an_address_for_one_medium_does_not_disturb_another() {
        let mut registry = Registry::default();
        registry.apply(endpoint("a", Some("Pixel"), true));

        // Stand in for a Bluetooth address arriving from another source.
        registry
            .devices
            .get_mut("a")
            .unwrap()
            .addresses
            .push(Address {
                medium: Medium::Bluetooth,
                address: "AA:BB:CC:DD:EE:FF".to_owned(),
            });

        // A fresh Wi-Fi announcement must replace only the Wi-Fi entry.
        registry.apply(endpoint_at(
            "a",
            Some("Pixel"),
            Some("10.9.9.9:9999".to_owned()),
        ));

        let device = registry.get("a").unwrap();
        assert_eq!(device.addresses.len(), 2, "{:?}", device.addresses);
        assert_eq!(
            device.address_for(Medium::WifiLan).unwrap().address,
            "10.9.9.9:9999"
        );
        assert_eq!(
            device.address_for(Medium::Bluetooth).unwrap().address,
            "AA:BB:CC:DD:EE:FF"
        );
        // Wi-Fi is preferred when both are available.
        assert_eq!(device.best_address().unwrap().medium, Medium::WifiLan);
    }

    #[test]
    fn an_explicit_absence_also_withdraws() {
        let mut registry = Registry::default();
        registry.apply(endpoint("a", Some("Pixel"), true));

        // rqs_lib never sends Some(false) today, but anything that is not an
        // explicit Some(true) must be treated as the device going away.
        let mut gone = endpoint("a", Some("Pixel"), true);
        gone.present = Some(false);
        registry.apply(gone);

        assert!(registry.get("a").unwrap().withdrawn_at.is_some());
    }

    /// This test previously asserted the opposite, and passed — it encoded my
    /// belief rather than rqs_lib's behaviour. `ServiceRemoved` sends
    /// `EndpointInfo { id, ..Default::default() }`, so `present: None` *is* the
    /// removal signal and `Some(false)` is never sent.
    #[test]
    fn a_missing_presence_flag_is_a_removal() {
        let mut registry = Registry::default();
        registry.apply(endpoint("a", Some("Pixel"), true));

        // Exactly what rqs_lib sends on ServiceRemoved: the id and nothing else.
        registry.apply(EndpointInfo {
            id: "a".to_owned(),
            ..Default::default()
        });

        // Withdrawn, so still listed during the grace period...
        let listed = registry.list();
        assert_eq!(listed.len(), 1);
        // ...but the name must survive. Wiping it to "Unknown device" was the
        // bug this test now guards.
        assert_eq!(listed[0].display_name(), "Pixel");
        assert!(registry.get("a").unwrap().withdrawn_at.is_some());
    }

    #[test]
    fn a_withdrawn_device_is_dropped_after_the_grace_period() {
        let mut registry = Registry::default();
        registry.apply(endpoint("a", Some("Pixel"), true));
        registry.apply(EndpointInfo {
            id: "a".to_owned(),
            ..Default::default()
        });

        registry.devices.get_mut("a").unwrap().withdrawn_at = Some(Instant::now() - REMOVAL_GRACE);

        assert!(registry.list().is_empty(), "must not list a gone device");
        assert!(registry.expire());
    }

    #[test]
    fn re_announcing_cancels_a_withdrawal() {
        let mut registry = Registry::default();
        registry.apply(endpoint("a", Some("Pixel"), true));
        registry.apply(EndpointInfo {
            id: "a".to_owned(),
            ..Default::default()
        });
        assert!(registry.get("a").unwrap().withdrawn_at.is_some());

        // The mDNS churn observed against a real phone: removed, then resolved
        // again moments later while it never actually went away.
        assert!(
            registry.apply(endpoint("a", Some("Pixel"), true)),
            "coming back is a change worth reporting"
        );
        assert!(registry.get("a").unwrap().withdrawn_at.is_none());
        assert_eq!(registry.list()[0].display_name(), "Pixel");
    }

    #[test]
    fn an_empty_update_never_wipes_a_name() {
        let mut registry = Registry::default();
        registry.apply(endpoint("a", Some("Pixel 6a"), true));

        // Belt and braces: even a present-flagged event with no name must not
        // erase one. This is the shape that produced "Unknown device".
        let mut nameless = endpoint("a", None, true);
        nameless.present = Some(true);
        registry.apply(nameless);

        assert_eq!(registry.list()[0].display_name(), "Pixel 6a");
    }

    #[test]
    fn leases_are_refcounted() {
        let mut registry = Registry::default();

        // The first lease starts discovery; the second must not restart it.
        assert!(registry.acquire_lease("nautilus"));
        assert!(!registry.acquire_lease("extension"));
        assert!(registry.has_leases());

        // One client going away must not stop discovery for the other -- the
        // bug this whole mechanism exists to prevent.
        registry.release_lease("nautilus");
        assert!(registry.has_leases());
        assert!(!registry.should_stop_discovery());

        registry.release_lease("extension");
        assert!(!registry.has_leases());
        // Still within the grace period.
        assert!(!registry.should_stop_discovery());
    }

    #[test]
    fn releasing_an_unknown_lease_is_harmless() {
        let mut registry = Registry::default();
        registry.acquire_lease("nautilus");
        // A client that crashed and is released twice, or was never leased.
        registry.release_lease("never-held");
        assert!(registry.has_leases(), "must not clear someone else's lease");
    }

    #[test]
    fn re_acquiring_cancels_a_pending_idle_stop() {
        let mut registry = Registry::default();
        registry.acquire_lease("nautilus");
        registry.release_lease("nautilus");
        assert!(registry.idle_since.is_some());

        assert!(registry.acquire_lease("nautilus"));
        assert!(registry.idle_since.is_none(), "idle timer must be cleared");
    }

    #[test]
    fn devices_sort_most_recently_seen_first() {
        let mut registry = Registry::default();
        registry.apply(endpoint("old", Some("Old"), true));
        registry.apply(endpoint("new", Some("New"), true));

        // Backdate the first so the ordering is unambiguous.
        registry.devices.get_mut("old").unwrap().last_seen =
            Instant::now() - Duration::from_secs(30);

        let listed = registry.list();
        assert_eq!(listed[0].id, "new");
        assert_eq!(listed[1].id, "old");
    }

    #[test]
    fn stale_devices_expire() {
        let mut registry = Registry::default();
        registry.apply(endpoint("a", Some("Pixel"), true));

        assert!(!registry.expire(), "a fresh device must survive");

        registry.devices.get_mut("a").unwrap().last_seen = Instant::now() - DEVICE_TTL;
        assert!(registry.expire());
        assert!(registry.list().is_empty());
    }
}
