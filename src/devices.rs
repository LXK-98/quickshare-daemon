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

/// A device we could send to.
#[derive(Debug, Clone)]
pub struct Device {
    /// `rqs_lib`'s endpoint id. Stable for as long as the device keeps
    /// advertising, and what [`crate::send`] needs to address it.
    pub id: String,
    /// What the device calls itself. Absent until mDNS resolves the TXT
    /// record, which is why it is an `Option` rather than a placeholder.
    pub name: Option<String>,
    /// `ip:port`, ready for `SendInfo::addr`. `None` until resolved.
    pub addr: Option<String>,
    /// The address alone, used to recognise the same device after it changes
    /// port. See the deduplication in [`Registry::apply`].
    pub ip: Option<String>,
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
    /// An entry can exist without an address: mDNS announces a service before
    /// resolving it. Offering such a device in a picker would produce a send
    /// that fails for reasons the user cannot act on.
    pub fn is_reachable(&self) -> bool {
        self.addr.is_some()
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
        // A removal is anything that is not an explicit `Some(true)`.
        //
        // rqs_lib sends exactly two kinds of event: `ServiceResolved` builds an
        // `EndpointInfo` with `present: Some(true)` and every field filled in,
        // and `ServiceRemoved` sends `EndpointInfo { id, ..Default::default() }`
        // — so a removal carries `present: None` and *no other data*.
        // `Some(false)` is never sent at all.
        //
        // Reading `None` as "still here" was a bug: the empty removal event was
        // applied as an update and wiped the device's name, leaving "Unknown
        // device" in the picker. See docs/RQS_LIB_NOTES.md.
        if endpoint.present != Some(true) {
            return match self.devices.get_mut(&endpoint.id) {
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
            (Some(ip), Some(port)) => Some(format!("{ip}:{port}")),
            _ => None,
        };

        // `rqs_lib` sets `id` to "ip:port", and a phone picks a fresh port for
        // every share session — so the same device reappears under a new id and
        // the picker would list it twice. One IP is one device on a LAN, so an
        // entry for the same address under a different id is the same phone
        // having moved port, and the old entry is dropped.
        if let Some(ip) = endpoint.ip.as_deref() {
            self.devices
                .retain(|id, device| id == &endpoint.id || device.ip.as_deref() != Some(ip));
        }

        match self.devices.get_mut(&endpoint.id) {
            Some(existing) => {
                existing.last_seen = Instant::now();
                // Announced again, so any pending withdrawal is void.
                let was_withdrawn = existing.withdrawn_at.take().is_some();
                // Never let an absent name replace a known one. Only the
                // resolve path reaches here, so a name should always be present
                // — but losing a device's name is the one failure that is
                // visible to the user, so it is guarded explicitly.
                let name_changed = endpoint.name.is_some() && existing.name != endpoint.name;
                let changed =
                    was_withdrawn || name_changed || (addr.is_some() && existing.addr != addr);
                if changed {
                    if name_changed {
                        existing.name = endpoint.name;
                    }
                    // Keep a known address rather than overwrite it with None:
                    // a later unresolved announcement must not make a reachable
                    // device unreachable.
                    if addr.is_some() {
                        existing.addr = addr;
                        existing.ip = endpoint.ip;
                    }
                }
                changed
            }
            None => {
                self.devices.insert(
                    endpoint.id.clone(),
                    Device {
                        id: endpoint.id,
                        name: endpoint.name,
                        addr,
                        ip: endpoint.ip,
                        last_seen: Instant::now(),
                        withdrawn_at: None,
                    },
                );
                true
            }
        }
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
        let (ip, port) = match id.rsplit_once(':') {
            Some((ip, port)) => (ip.to_owned(), port.to_owned()),
            None => {
                let seed: u32 = id.bytes().map(u32::from).sum();
                (
                    format!("10.0.{}.{}", seed / 256 % 256, seed % 256),
                    "1234".to_owned(),
                )
            }
        };

        EndpointInfo {
            fullname: format!("{id}._FC9F5ED42C8A._tcp.local."),
            id: id.to_owned(),
            name: name.map(str::to_owned),
            ip: resolved.then_some(ip),
            port: resolved.then_some(port),
            rtype: None,
            present: Some(true),
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

        let resolved_addr = registry.get("a").unwrap().addr.clone();

        // mDNS announces before resolving; this must not make a reachable
        // device unreachable.
        registry.apply(endpoint("a", Some("Pixel"), false));

        let device = registry.get("a").unwrap();
        assert_eq!(device.addr, resolved_addr);
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
    fn a_device_changing_port_does_not_appear_twice() {
        let mut registry = Registry::default();

        // The same phone, re-advertising on a new port for a second share --
        // which rqs_lib reports as a different `id`, since the id *is* ip:port.
        registry.apply(endpoint("192.168.1.5:1111", Some("Pixel"), true));
        registry.apply(endpoint("192.168.1.5:2222", Some("Pixel"), true));

        let listed = registry.list();
        assert_eq!(
            listed.len(),
            1,
            "one phone must not be listed twice: {listed:?}"
        );
        assert_eq!(listed[0].addr.as_deref(), Some("192.168.1.5:2222"));
    }

    #[test]
    fn different_devices_on_different_addresses_both_list() {
        let mut registry = Registry::default();

        registry.apply(endpoint("10.0.0.1:1", Some("Pixel"), true));
        registry.apply(endpoint("10.0.0.2:1", Some("Laptop"), true));

        assert_eq!(registry.list().len(), 2);
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
