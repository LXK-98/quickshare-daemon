# Device identity across transports

A design note, written 2026-11-07 before any Bluetooth code exists, because the
answer changes the D-Bus contract and is expensive to retrofit.

**The question.** Today a device *is* its `ip:port`. A phone reachable only over
Bluetooth has no `ip:port`. What identifies it instead, and who decides?

## What `ip:port` is doing today

Four separate jobs, all collapsed into one string:

| Job | Where |
|---|---|
| **Device identity** — which device is this | `EndpointInfo.id`, `Device.id`, the `id` key in `ListDevices` |
| **Dial string** — how to reach it | `SendInfo.addr` → `TcpStream::connect(si.addr)` |
| **Transfer id** — which transfer an event belongs to | `TransferPin`/`TransferFinished`, and inbound uses `remote_addr.to_string()` |
| **Dedup key** — is this the same device re-advertising | `Registry::apply` matches on `Device.ip` |

They coincide only because there is one medium. Two consequences already:

- **The id is not stable.** A phone picks a fresh port per share session, so the
  same device arrives under a new id. `Registry::apply` already papers over this
  by deduplicating on IP (RQS_LIB_NOTES.md), which is the dedup job quietly
  diverging from the identity job.
- **A latent inconsistency.** `start_outbound` returns `device.id` as the
  transfer id, but `manager.rs`'s outbound error path reports `si.addr`. Equal
  strings today, different the moment identity and dial string separate — a
  client would see a failure for a transfer id it never heard of.

Nothing persists an id across runs (`auto_accept_from` matches on *name*), so
there is no stored data to migrate. That is the one piece of luck here.

## What Bluetooth needs

A device reachable only over Bluetooth is identified by its **MAC address**, and
dialled with a MAC plus an RFCOMM channel. It is not on any mDNS-reachable
segment, so it is discovered over BLE, not mDNS.

So the dial string becomes medium-specific, and a device may be reachable over
*several* media at once — the same phone on the same Wi-Fi *and* in Bluetooth
range. Quick Share's own model expects this: `BandwidthUpgradeNegotiation`
exists precisely to move a live connection from one medium to a faster one.

## Options

### A. Keep one opaque string, make it medium-tagged

`"wifi:192.168.1.5:53601"`, `"bt:AA:BB:CC:DD:EE:FF"`. Parsed where it is dialled.

Cheapest change: `ListDevices` keys stay, `SendFiles(target)` still takes a
string. But it keeps all four jobs fused, so the id still changes when the port
does, a device on two media appears twice with no way to tell it is one device,
and every consumer has to learn the string grammar. It defers the problem rather
than answering it.

### B. Separate identity from address (recommended)

A device gets a stable `id` of our own; its reachability is a *list* of
medium-tagged addresses.

```
id         "pixel-6a-von-alex"   stable for as long as we can tell it is the same device
name       "Pixel 6a"  what it calls itself
addresses  [ {medium: "wifi_lan", address: "192.168.1.5:53601"},
             {medium: "bluetooth", address: "AA:BB:CC:DD:EE:FF"} ]
```

`SendFiles` then takes our id and the daemon picks a medium — which is the right
place for that choice, since only the daemon knows what is reachable and what it
costs. Wi-Fi LAN first, Bluetooth as fallback, matching what Quick Share itself
does.

The hard part is **what makes the id stable**. Candidates, in order of how much
they can be trusted:

1. **The BLE/mDNS endpoint id.** Quick Share devices advertise a 4-byte endpoint
   id. `rqs_lib` currently throws it away in discovery (it synthesises
   `ip:port` instead) but it is in the TXT record and the BLE advertisement.
   Needs checking whether a phone keeps it stable across share sessions — it may
   well be per-session, in which case it is no better than the port.
2. **The advertised name**, which `auto_accept_from` already relies on. Stable in
   practice, user-visible, and not unique — two phones can share a name, which
   we already refuse to guess between.
3. **A synthetic id keyed on (name, MAC-or-IP)**, which is what `Registry` is
   informally doing now by deduplicating on IP.

**Partly answered already**, from the discovery logs of this project's own test
runs. The mDNS `fullname` carries the endpoint id, and the same Pixel 6a shows:

| Date | fullname | address |
|---|---|---|
| 2026-10-01, four separate share sessions | `EXAMPLEENDPT01` | `192.0.2.25:53601` |
| 2026-11-07, three separate share sessions | `EXAMPLEENDPT02` | `192.0.2.129:53601` |

So the endpoint id is **stable across share sessions** — four in a row on one
day, three on another — and **changes over longer periods or across networks**.
That is good enough for option B: identity needs to survive a device closing and
reopening its sheet, which it does. It does not need to survive a week, because
nothing persists it.

**It also corrects an assumption of mine.** I wrote in RQS_LIB_NOTES.md that "a
phone picks a fresh port per share session", which is what the IP-based dedup in
`Registry::apply` was justified by. The observations do not support it: the port
was `53601` on both days, across seven sessions. What actually changed was the
**IP**, between networks. So the dedup is doing something real — it just is not
the thing I said it was, and under option B it is replaced by identity matching
anyway.

### The controlled check, 2026-11-07 23:07–23:10

Ran `avahi-browse` continuously while the sheet was opened, closed and reopened,
logging every sighting. The sheet being closed shows up as a 41-second gap in
which avahi lost the service entirely — so the sightings after it are a fresh
resolution, not cache:

```
23:07:38  EXAMPLEENDPT02  192.0.2.129:53601
   … through 23:08:09
            (41s gap — sheet closed, service withdrawn)
23:08:50  EXAMPLEENDPT02  192.0.2.129:53601
   … through 23:09:38
```

**The endpoint id is unchanged across the cycle. So is the address, port
included.** Exactly one distinct endpoint id and one distinct address in the
whole capture.

So option B has a principled identity: the first label of the mDNS `fullname`,
which `rqs_lib` currently discards in favour of synthesising `ip:port`.

**Scope of the claim.** One device, one network, one sitting. It establishes that
the id survives a share session ending — the lifetime option B needs — and
nothing more. The earlier observation that it differs between days and networks
stands, which is why nothing should persist it.

It also puts the "fresh port per session" assumption to rest for good: the port
was identical either side of a deliberate close. That claim was wrong, and the
IP-based dedup it justified needs its own rationale, which option B supersedes.

### A note on method

The first attempt at this check deduplicated sightings on
`(endpoint, address, port)`. If the id is stable, that collapses both sessions
into a single line — indistinguishable from having caught the device only once.
The capture had to be rerun logging every sighting, where the gap carries the
information. Worth remembering: when testing whether something *changes*,
deduplicating is deleting the measurement.

### C. Expose the medium, let clients choose

`SendFiles(target, medium)`. Rejected: a Nautilus menu has no basis for picking
a medium, and the daemon would still need a default for when the client does not
care. It pushes a decision outward to the component with the least information.

## Recommendation

**Option B**, with the id question settled empirically first.

Do it now rather than after the Bluetooth transport exists. The D-Bus API is
young — its only consumer is our own CLI, there is no Nautilus extension yet and
no GNOME extension — so changing `ListDevices`'s shape costs one file today and
would cost a published contract later. D-013 chose `a{sv}` dicts precisely so
fields could be added; an `addresses` array is exactly that case, but replacing
the *meaning* of `id` is not additive and should happen before anyone depends on
it.

And fix the transfer-id inconsistency in the same pass, since a transfer id that
is sometimes an address and sometimes a device id becomes a real bug the moment
the two differ.

## What changes where

**In the fork:**

- `SendInfo` carries a medium-tagged target instead of a bare `addr` string, and
  `TcpServer::connect` dispatches on it. The handshake itself needs nothing —
  D-015 already made it transport-generic.
- `MDnsDiscovery` stops synthesising `ip:port` as the id and reports the real
  endpoint id alongside the address, assuming research question 1 goes well.
- The outbound error path reports the same id as the success path.

**Here:**

- `Device` gains `addresses: Vec<(Medium, String)>`; `ip`-based dedup is replaced
  by identity matching. The withdrawal grace and TTL logic are unaffected.
- `ListDevices` dicts gain `addresses`; `id` changes meaning. `reachable`
  becomes "has at least one usable address".
- `SendFiles` takes our id; medium selection moves into the daemon.
- The CLI's `resolve_remote` is unaffected in shape — it already matches id or
  name and refuses ambiguity.

**Not affected:** `auto_accept_from` (matches names), the config file, the
notification layer, and the inbound consent path.

## Next step

Answer research question 1 before writing any of it: run two share sessions from
the Pixel and compare the endpoint id in the mDNS TXT record. If it is stable,
Option B has a principled id. If it is per-session, identity has to be synthetic
and the note's option-B sketch needs revisiting — better to learn that from
`avahi-browse` than from a half-built transport.
