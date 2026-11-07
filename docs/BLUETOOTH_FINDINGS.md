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
