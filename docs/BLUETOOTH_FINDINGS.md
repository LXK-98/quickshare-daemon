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

## The cheapest next experiment

Invert it: make **us** the discoverable one.

Register an RFCOMM profile with Quick Share's service UUID via BlueZ, make the
adapter discoverable, turn Wi-Fi off on the phone, and see whether the phone
offers this machine as a Quick Share target. That tests the listener half — the
half we would have to implement anyway — and it needs one phone rather than two.

If the phone finds us, discovery is its problem and ours is accepting the
connection. If it does not, the Bluetooth medium needs protocol work that cannot
be guessed at: a packet capture of a working Android-to-Android transfer with
Wi-Fi disabled, using `btmon`, to see what actually happens on the wire.

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
