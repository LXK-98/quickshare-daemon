# Bluetooth transport: what the investigation found

Investigated 2026-11-07, before writing any transport code, because the plan
assumed BLE discovery could produce a MAC to connect to. **It cannot, as
sketched.** Recorded here so the next attempt starts from the evidence rather
than the assumption.

## What works

**A MAC is available from a BLE advertisement.** `rqs_lib`'s `BleListener`
already receives it — `CentralEvent::ServiceDataAdvertisement { id, .. }`, where
`id` wraps the BlueZ device path and hence the address — and discards it,
sending only `()` to say "someone is sharing". Extracting it would be trivial.

## What does not

**The phone emits no Quick Share BLE advertisement while its share sheet is
open.** 60 seconds of scanning with the sheet open produced **zero** payloads
in the format `rqs_lib` itself advertises (26 bytes beginning `fc 12 8e 01`,
the constant in `blea.rs`).

What it did produce: four advertisements on Quick Share's service UUID
`0xFE2C`, all 19 bytes beginning `10 52`, from two addresses —
`AA:BB:CC:44:55:66` and `AA:BB:CC:44:55:67`. Both resolve to the same pair of
Sony earbuds (`LE_WF-1000XM5`), the second being a randomised address for the
same device. **Nothing from the phone.**

`0xFE2C` is Google's Fast Pair UUID, which Quick Share shares. So the UUID alone
does not identify Quick Share traffic.

**The phone has no classic Bluetooth presence either.** It is not paired, and
nothing resembling a Pixel appears in BlueZ's device list. The local adapter is
`Discoverable: no`, `Pairable: no`.

## Consequences

1. **Discovery cannot be built on advertisement parsing.** There is no
   advertisement from the peer to parse. Whatever real Quick Share does for
   Bluetooth discovery, it is not "listen for the peer's BLE beacon and read its
   MAC".
2. **`rqs_lib`'s `BleListener` reports false positives.** It treats any `0xFE2C`
   advertisement as "a device is sharing nearby", and in one 60-second window
   four such advertisements came from a pair of earbuds. Its only consumer is a
   wake-up hint, so the cost is wasted work rather than wrong behaviour — but it
   is wrong, and distinguishing the payloads is a four-byte check.
3. **The direction of the BLE beacon is worth re-reading.** `rqs_lib`
   advertises from `discovery()` — the *sender* beacons, and a *receiver*
   listens, to wake up receivers that are not otherwise visible. A phone with
   its share sheet open is a sender, so on that model it should beacon, and it
   did not. Either the model is wrong, or Android beacons only in states we did
   not reproduce — screen on, visibility set to everyone, Wi-Fi unavailable.

## The inverted experiment, and why it was inconclusive

Ran 2026-11-07: made the local adapter `Discoverable: yes` and `Pairable: yes`
(BlueZ allows both over D-Bus without root), turned Wi-Fi off on the phone, and
opened Quick Share.

**The machine did not appear in the phone's list.**

That was flagged as ambiguous before running it, and it is: we registered no
Quick Share RFCOMM service, because **the service UUID is not in any source
available here.** `rqs_lib` and Packet contain only `0xFE2C`, the BLE
service-data UUID, and the proto carries the RFCOMM identity inside the
*bandwidth upgrade* frame:

```proto
// Accompanies Medium.BLUETOOTH.
message BluetoothCredentials {
  optional string service_name = 1;
  optional string mac_address = 2;
}
```

So for a *bandwidth upgrade* the service name is negotiated per connection. For
an initial Bluetooth-medium connection there must be a well-known UUID, and we
do not have it. A device advertising no matching service is not expected to be
offered, so the negative tells us little.

The adapter was reverted to `Discoverable: no`, `Pairable: no` afterwards.

## Breakthrough: the Pixel's SDP record

2026-11-08. With the Pixel's Bluetooth settings page open (Android is only
discoverable while that screen shows), a classic inquiry from this laptop
returned its full SDP record:

```
Name: Pixel 6a   Class: 0x005a020c   Icon: phone
UUID: OBEX Object Push, Audio Source, A/V Remote Control Target,
      A/V Remote Control, Headset AG, PANU, NAP, Handsfree Audio Gateway,
      SIM Access, Phonebook Access Server, PnP Information,
      Message Access Server
UUID: Vendor specific          (a82efa21-ae5c-3dde-9bbc-f16da7b16c5a)
```

**`a82efa21-ae5c-3dde-9bbc-f16da7b16c5a` is the only vendor-specific entry**;
everything else is a standard Bluetooth profile. This is very likely the Quick
Share / Nearby Connections RFCOMM service — the value the transport work has
been blocked on.

**Stated as inference, not fact.** A web search did not corroborate the UUID,
and it could in principle belong to another Google service on the device. What
supports it: it is the sole non-standard UUID, and this same phone accepted a
Quick Share transfer over classic Bluetooth minutes earlier (below).

The decisive confirmation is cheap now: register an RFCOMM profile with this
UUID here, make the adapter discoverable, and see whether the phone offers this
machine as a Quick Share target with Wi-Fi off — the inverted experiment that
was inconclusive before only because we had no UUID to register.

## Confirmed: Quick Share really does use classic Bluetooth

A Z Fold 8 → Pixel transfer with the Pixel's Wi-Fi off, captured from the Z
Fold's HCI log, shows a classic ACL connection to the Pixel during the
transfer:

```
< HCI Command: Create Connection      Address: AA:BB:CC:11:22:33 (Google, Inc.)
> HCI Event: Role Change              Role: Peripheral
> HCI Event: Connect Complete         Handle: 7  Link type: ACL  Status: Success
```

So the `BLUETOOTH` medium is genuinely used when no network is shared, rather
than the devices falling back to a hotspot. `AA:BB:CC:11:22:33` is the Pixel's
classic BD address.

## How to capture an Android HCI log without root

Worth recording, because it took several wrong turns:

1. The **bug report contains no Bluetooth log at all** on One UI — no
   `/data/misc/bluetooth/logs/` entry. Do not bother with it.
2. The developer setting "Enable Bluetooth HCI snoop log socket" claims to
   expose the log on a local socket, but **nothing listens on tcp:8872** (or
   40165, or 6800) even after a Bluetooth restart.
3. `/data/log/bt/` exists but is `bluetooth:bluetooth`, so `adb shell` cannot
   read it.
4. **What works:** `adb shell dumpsys bluetooth_manager` embeds the log between
   `BEGIN:BTSNOOP_LOG_SUMMARY` and `END:BTSNOOP_LOG_SUMMARY` as
   `base64 { version:i8, last_timestamp_ms:u64, deflate { records } }`.
   Converter at `docs/btsnooz2btsnoop.py`, output readable by `btmon -r`.

**Its limitation is in the name: it is a *summary*.** Of 6990 records, 5742
were HCI events and 1242 commands — but only **6 were ACL**. L2CAP, SDP and
RFCOMM payloads are therefore absent, which is why the UUID had to come from an
SDP query instead. Connection setup and BLE activity are fully visible.

## The decisive experiment ran, and was negative

2026-11-08. Registered an RFCOMM profile with
`a82efa21-ae5c-3dde-9bbc-f16da7b16c5a`, advertised `0xFE2C` carrying the same
service data `rqs_lib`'s `BleAdvertiser` sends, made the adapter discoverable
and pairable, and turned Wi-Fi off on the phone.

**The phone did not offer this machine, in either direction.**

This time both halves were confirmed up before the test — an earlier attempt
had the advertisement silently rejected by BlueZ (`Failed to parse
advertisement`, caused by a `local_name` pushing it past the 31-byte limit)
while the run reported itself as testing both. That run was a repeat of the
RFCOMM-only one and should be disregarded.

So one of three things is wrong:

1. **The UUID is not Quick Share's.** Possible, though it is the only
   vendor-specific entry in the Pixel's SDP record.
2. **The BLE payload is not a valid receiver advertisement.** Most likely.
   `rqs_lib`'s `SERVICE_DATA` is a hardcoded 26-byte constant — a blob captured
   from some device at some point. Quick Share's advertisement near-certainly
   encodes device-specific content (a name hash, a salt, an account key), so a
   replayed stale one would be rejected. It is also what `rqs_lib` broadcasts
   while *sending*, not what a receiver advertises.
3. **Receiver discovery does not work this way at all.**

These cannot be distinguished with the hardware available. The next step is
desk research into the published reverse-engineering of the Nearby Connections
Bluetooth medium and its BLE advertisement format, not more probing.

## The research answers it: there is no Bluetooth receiver discovery

2026-11-08. From grishka/NearDrop's `PROTOCOL.md`, the best community
write-up of the protocol:

- **A receiver is discovered over mDNS.** Service type `_FC9F5ED42C8A._tcp.`,
  instance name carrying a `0x23` byte, the 4-byte endpoint id, the 3-byte
  service id `FC 9F 5E` and two zero bytes; TXT key `n` carrying base64
  endpoint info (bit field with version/visibility/device type, 16 bytes of
  identity data, the device name when visible). **This is exactly what we
  already do.**
- **The `0xFE2C` / `fc 12 8e 01 42 …` advertisement is the *sender's*.**
  *"Android sends these BLE advertisements periodically while searching for
  Nearby Share targets"*, and a receiver *"does not advertise the MDNS service
  all the time"* — it *"waits for a BLE advertisement with the following
  parameters"* and then starts. It is a wake-up trigger, not a presence
  announcement.

**So our experiment had the roles backwards.** We broadcast "I am looking for
targets" and then waited to be listed *as* a target. No RFCOMM registration
would have fixed that, and the UUID question was beside the point. Explanation
2 from the section above was right for a reason I had not identified: the
payload is not merely stale, it is the wrong role's advertisement entirely.

**Bluetooth is not a transport in any third-party implementation.** NearDrop,
rquickshare and NearShare are all mDNS-based with BLE as a trigger only. The
classic ACL connection captured between the two phones is real, but it is
something Google Play Services does Android-to-Android, outside the protocol
that has been reverse-engineered.

### Superseded — see "The full Bluetooth recipe" below

The conclusion in this section was **wrong**, and is kept only because the
reasoning that produced it is instructive. A full HCI capture from the Pixel
showed Quick Share using RFCOMM over classic Bluetooth, with a published SDP
record. NearDrop's protocol notes describe the mDNS path accurately but are not
exhaustive: they document what *that* client implements, not everything Android
does.

~~**Not achievable against stock Android with the documented protocol.** The
discovery mechanism is mDNS, which needs a shared link. Implementing a
Bluetooth medium on our side would produce something no Android device looks
for.~~

What remains possible, in descending order of plausibility:

1. **`WIFI_HOTSPOT` bandwidth upgrade.** `BandwidthUpgradeNegotiationFrame`
   carries `WifiHotspotCredentials { ssid, password }`, so a connection
   established over one medium can move to a hotspot. That still needs an
   initial connection, so it does not solve discovery.
2. **Linux-to-Linux.** Two instances of this daemon could agree any transport
   they like, since both ends are ours. Useful, but not the goal.
3. **Nothing else** without Google Play Services internals.

### Sources

- grishka/NearDrop `PROTOCOL.md` — https://github.com/grishka/NearDrop/blob/master/PROTOCOL.md
- Dhiva-Labs/NearShare — https://github.com/Dhiva-Labs/NearShare
- asulwer/rquickshare PR #39, on `0xFE2C` false positives — https://github.com/asulwer/rquickshare/pull/39
- google/nearby discussion #2446 — https://github.com/google/nearby/discussions/2446
- adityatr64/QShare-linux — https://github.com/adityatr64/QShare-linux (describes a
  different layout with `0xFEF3`; unverified, and the README says it is
  AI-assisted, so treated as unreliable)

These are unofficial and some are years old; Google publishes no spec.

## The full Bluetooth recipe, from a complete HCI capture

2026-11-08. A **Pixel** bug report does include
`FS/data/misc/bluetooth/logs/btsnoop_hci.log` — 1.4 MB, complete with ACL
payloads. (One UI's does not; that was the earlier dead end.) Decoded with
`btmon -r`, a Z Fold → Pixel transfer with the Pixel's Wi-Fi off shows the whole
exchange.

### The receiver publishes an SDP record

```
Service Class ID:   a82efa21-ae5c-3dde-9bbc-f16da7b16c5a
Protocol:           L2CAP -> RFCOMM, channel 0x0a (10)
Attribute 0x0100:   "NearbySharing"
```

The sender issues an SDP Service Search Attribute Request for that exact UUID,
gets this back, then opens RFCOMM (SABM/UA on PSM 3). **The UUID inferred from
the Pixel's SDP record was correct**, and the service name is `NearbySharing`.

### How the sender found the receiver

It did not search. The Pixel's log shows an **unsolicited `Connect Request`**
from the Z Fold — no inquiry, no scan preceding it. The Z Fold already knew the
Pixel's BD address, which is consistent with both devices being signed into the
same Google account (the transfer also needed no acceptance on the Pixel, the
"Your devices" flow).

Also present just before: a BLE advertisement the Pixel *received* carrying
service data under **`0xFEF3`** (Google), payload `11 01 d0 36 9d 5d` — not
`0xFE2C`. This matches the layout the QShare-linux README describes, which had
been set aside as unverified.

### Why our experiment failed, precisely

Two reasons, neither of which was the UUID:

1. The service record was named `Quick Share`, not `NearbySharing`.
2. More fundamentally, **nothing told the phone our address existed.** The
   sender connects to an address it already knows; it does not go looking. A
   Linux box that has never been paired is not in any such list.

### Pairing was tried, and does not help

2026-11-08. Published the SDP record exactly as captured — UUID, RFCOMM channel
10, service name `NearbySharing` — with a pairing agent. The Pixel paired
successfully:

```
Name: Pixel 6a   Paired: yes   Bonded: yes
UUID: Vendor specific  (a82efa21-ae5c-3dde-9bbc-f16da7b16c5a)
```

**Quick Share still listed nothing, in either direction.** No RFCOMM connection
was attempted.

### Conclusion: the Bluetooth path is gated on Google account linkage

Four pieces of evidence, which now agree:

1. The Z Fold connected to the Pixel's BD address with **no inquiry and no
   scan** — it already knew the address.
2. That transfer required **no acceptance** on the Pixel: the same-account
   "Your devices" flow.
3. **Bluetooth bonding does not substitute.** We are paired and still invisible,
   so the address list Quick Share draws on is not BlueZ's.
4. No third-party implementation uses Bluetooth, despite the protocol being
   otherwise well reverse-engineered.

The peer address appears to come from Google account sync — a Play Services
feature rather than a protocol one. A Linux client cannot enter that list
without being a Google-account-registered device.

**This is a conclusion with evidence behind it**, unlike the earlier one that a
capture disproved. It could still be wrong — a mechanism we have not observed
might exist — but every observation available here points the same way.

### What would still be worth trying, if someone returns to this

- **Linux-to-Linux.** Both ends ours, so any transport can be agreed. The
  groundwork already supports it: generic handshake (D-015), `Target::Bluetooth`
  slot, `Medium::Bluetooth` in the device model.
- **`0xFEF3`.** The BLE advert the Pixel received before the connection carried
  service data under `0xFEF3`, payload `11 01 d0 36 9d 5d`, not `0xFE2C`. What
  that encodes is unexamined, and it is the one observed signal not yet
  accounted for.
- **A capture with a non-account-linked Android** sending over Bluetooth, if
  such a thing even happens. That would show whether the account gate is real
  or whether we simply never produced the right advertisement.

## Where this leaves it: blocked on protocol knowledge, not on code

The groundwork is done and useful regardless — the handshake is transport-generic
(D-015), `Target` has a Bluetooth slot, `Medium::Bluetooth` is in the device
model, and `Device.addresses` can hold a MAC. What is missing is knowing **what
to advertise and what to connect to**, and that is not guessable.

Three ways to get it, in order of cost:

1. **Research the protocol.** Quick Share and Nearby Connections have been
   reverse-engineered publicly. The specific wanted facts are the RFCOMM service
   UUID for the `BLUETOOTH` medium, and whether discovery over that medium uses
   classic inquiry, BLE GATT, or something else. No hardware needed.
2. **`sudo btmon` while the phone attempts a Bluetooth transfer.** Needs root,
   and needs the phone to have something to attempt *to* — which is the problem,
   so this probably needs (3) as well.
3. **A second Android device.** Capture a working Android-to-Android transfer
   with Wi-Fi disabled on both. This is the one that definitively answers it,
   and the only one that shows a working exchange rather than a failed attempt.

Until one of those happens, further code would be guesswork.

## Method note

Two of this investigation's own measurements were spoiled by filtering before
reading:

- The first scan deduplicated advertisements on `(id, address, port)`, which
  collapses a stable advertiser into one line and hides repetition.
- The second piped the probe through `tail -70`, which discarded exactly the
  per-advertisement UUID lines the question turned on — and the "zero Quick
  Share payloads" count taken from that truncated output was meaningless.

Both were caught, but the pattern is the same one: when the question is whether
something appears, do not filter the output before looking at it. The probe is
kept at `docs/ble-probe.rs`.
