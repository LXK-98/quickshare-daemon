# Journal

Per-session record: what was done, what is verified, what broke, what is next.
Newest first.

## 2026-11-08 — Nautilus extension works; the first failure was impatience

Confirmed in the UI: the submenu lists the phone, and the "Other device…"
picker works too. Both paths exercised against a real device.

**The first attempt reported "No devices found" while the daemon's own log
showed it had resolved the Pixel.** I suspected my D-Bus unpacking, tested it,
and was wrong — `result[0]` yields real Python dicts and `d.get("reachable")`
returns `True`. `ListDevices` answered in about six seconds once the phone was
actually advertising.

So it was timing: the poll gave up after 15s, before the phone had appeared.
Raised to 45s, matching the CLI's send wait and for the same reason — a phone
only advertises while its Quick Share screen is open, and nobody can open that
and click a menu simultaneously. The timeout message now says to retry rather
than implying failure, and a "Looking for devices" notification goes up when
the search starts, so the fallback no longer looks inert while it waits.

Worth noting the failure mode: an impatient timeout presented as a definitive
"no devices", which reads as a broken extension. The daemon was working the
whole time.

---

## 2026-11-08 — Nautilus extension

`data/quickshare_nautilus.py`, installed to
`~/.local/share/nautilus-python/extensions/`. Nautilus 50.4 wants typelib
**4.1**, not the 4.0 Packet's extension requires.

Structure is adapted from Packet's `packet_nautilus.py.in` but the menu differs:
Packet hands files to its GUI, which then shows a picker. There is no GUI here,
so the device list has to be in the menu itself.

- **Uses GDBus, not `python3-dbus`.** Nautilus already depends on GIO, so the
  extension adds no dependency.
- **Holds a discovery lease and releases it after 60 s.** Nautilus is
  long-lived and the daemon keys leases on the client's bus name, so a lease
  taken when a menu opens would otherwise run mDNS and the BLE advertiser all
  day. This is the first real consumer of the refcounted leases from D-013.
- **2-second call timeout**, because this runs while Nautilus builds a context
  menu and a stalled call would freeze the file manager.
- **The zenity fallback is the common path, not the exception.** The menu is
  built synchronously but discovery takes seconds, so the submenu is usually
  empty on first open — "Other device…" opens a picker that polls for ~15 s.
  PLAN.md anticipated exactly this.
- Directories, non-local files and a missing daemon all produce an empty menu
  rather than an item that fails.

**Verified:** imports cleanly against the real typelib, instantiates,
implements `MenuProvider`, returns `[]` with no daemon. The `Daemon` client
half was driven against a live daemon — proxy reachable, `StartDiscovery`
logged with a holder, `ListDevices` answered, `StopDiscovery` sent.

**Not verified:** the menu actually appearing in Nautilus, and a send completing
through it. Needs a human right-clicking a file.

Note: Packet's extension is still installed alongside, so both items appear.

---

## 2026-11-08 — Pairing does not help: the Bluetooth path is account-gated

Published the SDP record exactly as captured — UUID, RFCOMM channel 10, service
name `NearbySharing` — with a pairing agent. The Pixel paired (`Paired: yes`,
`Bonded: yes`, passkey auto-confirmed). **Quick Share still listed nothing in
either direction, and no RFCOMM connection was attempted.**

Four observations now agree:

1. The Z Fold connected to the Pixel's address with **no inquiry and no scan**.
2. That transfer needed **no acceptance** — the same-account "Your devices"
   flow.
3. **Bluetooth bonding does not substitute**: we are paired and still invisible,
   so the list Quick Share draws on is not BlueZ's.
4. No third-party implementation uses Bluetooth, despite the rest of the
   protocol being well reverse-engineered.

So the peer address comes from Google account sync — a Play Services feature,
not a protocol one — and a Linux client cannot enter that list.

Unlike the earlier "not achievable" claim, which a capture disproved within the
hour, this one has evidence behind it. It could still be wrong, but every
observation available here points the same way.

**Stopping the Bluetooth thread.** Left in the findings doc for anyone
returning to it: Linux-to-Linux (both ends ours, groundwork already in place),
and the unexamined `0xFEF3` advert the Pixel received just before the
connection — the one observed signal still unaccounted for.

---

## 2026-11-08 — A full HCI capture: Quick Share does use RFCOMM, and I was wrong

A **Pixel** bug report includes `FS/data/misc/bluetooth/logs/btsnoop_hci.log`
complete with ACL payloads — 1.4 MB. One UI's does not, which was the earlier
dead end. That gave the whole exchange.

**The receiver publishes an SDP record:**

```
Service Class ID:  a82efa21-ae5c-3dde-9bbc-f16da7b16c5a
Protocol:          L2CAP -> RFCOMM, channel 0x0a (10)
Attribute 0x0100:  "NearbySharing"
```

The sender does an SDP Service Search Attribute Request for exactly that UUID
and then opens RFCOMM. **The UUID inferred from the Pixel's SDP record was
right.**

**So the previous entry's conclusion was wrong.** I had written that Bluetooth
is not a transport and that transfer without a shared network is "not
achievable against stock Android". NearDrop's protocol notes describe the mDNS
path accurately, but they document what *that client* implements, not
everything Android does — and I treated one community document as exhaustive.
The capture contradicts it directly.

**Why our experiment actually failed:** not the UUID. The service record was
named `Quick Share` rather than `NearbySharing`, and more fundamentally the
sender never searches — the Pixel's log shows an **unsolicited Connect
Request** from the Z Fold, no inquiry at all. It connected to an address it
already knew, both phones being on the same Google account. A Linux box that
has never been paired is in no such list.

Also found: a BLE advert the Pixel received carrying service data under
**`0xFEF3`**, not `0xFE2C` — matching the QShare-linux layout I had set aside
as unverified.

**Next, and well-founded this time:** pair the laptop with the Pixel over
classic Bluetooth, then publish the SDP record exactly as captured. Pairing is
what puts our address in the phone's known-device list, which is the gap.

**Two lessons.** Treating one community write-up as the full protocol produced
a confident wrong conclusion; the primary capture settled in minutes what the
documentation got incomplete. And the earlier "research first" lesson still
holds — it was right that desk research beat guessing, just wrong that it was
sufficient.

---

## 2026-11-08 — Research closes the Bluetooth question: no receiver discovery

NearDrop's `PROTOCOL.md` answers it. **A Quick Share receiver is discovered over
mDNS** — `_FC9F5ED42C8A._tcp.` with the endpoint id in the instance name and
endpoint info in the `n` TXT record, which is exactly what we already do. **The
`0xFE2C` / `fc 12 8e …` advertisement is the *sender's*:** Android broadcasts it
while searching for targets, and a receiver that is not currently advertising
mDNS sees it, wakes, and starts. A trigger, not a presence announcement.

**So the failed experiment had the roles backwards** — we broadcast "I am
looking for targets" and waited to be listed as one. No RFCOMM registration
would have helped and the UUID was beside the point. I had guessed the payload
was "stale"; it is worse than that, it is the wrong role's advertisement.

**Bluetooth is not a transport in any third-party implementation.** NearDrop,
rquickshare and NearShare are all mDNS with BLE as a trigger. The classic ACL
connection captured between the two phones is real but is Play Services doing
something Android-to-Android that is outside the reverse-engineered protocol.

**Therefore "transfer without a shared Wi-Fi network" is not achievable against
stock Android** with the documented protocol: discovery needs a shared link. A
Bluetooth medium on our side would produce something no Android device looks
for. What remains is a `WIFI_HOTSPOT` bandwidth upgrade (which still needs an
initial connection, so it does not solve discovery) or Linux-to-Linux, where
both ends are ours.

Sources and the full reasoning in `docs/BLUETOOTH_FINDINGS.md`.

**Lesson worth keeping:** one search and one document read answered what five
hardware experiments could not, and would have prevented three of them. The
investigation order should have been protocol-first.

---

## 2026-11-08 — The decisive Bluetooth experiment ran, and was negative

Registered the candidate RFCOMM UUID, advertised `0xFE2C` with `rqs_lib`'s
service data, adapter discoverable and pairable, Wi-Fi off on the phone. **The
phone did not offer this machine, in either direction.**

Three candidate explanations, undistinguishable with the hardware here, in
order of likelihood: the BLE payload is not a valid *receiver* advertisement
(`rqs_lib`'s `SERVICE_DATA` is a hardcoded blob captured from some other
device, and is what it sends while *sending*, not receiving); the UUID is not
Quick Share's; or receiver discovery does not work this way at all.

**A false result I nearly reported as real.** The first combined run had its
advertisement silently rejected by BlueZ — `Failed to parse advertisement`,
from a `local_name` pushing it past the 31-byte limit — while the program
carried on and the run looked like a valid test of both halves. It was a repeat
of the RFCOMM-only run. The failure is now fatal rather than logged, and I
smoke-tested registration before asking for another phone-side attempt. Third
measurement this session spoiled by not checking the instrument first.

**Also mishandled:** killing that run with `pkill` left the adapter
`Discoverable: yes` / `Pairable: yes`, since the program never reached its
cleanup. Caught and reverted, but a background process that changes system
state should not be killed from outside its own teardown.

Probe kept at `docs/rfcomm-profile-probe.rs`.

**Stopping the hardware experiments here.** They are getting less informative
per attempt, and the open questions are protocol facts rather than
configuration. What this line of work did establish is worth keeping: Quick
Share genuinely uses classic Bluetooth with no shared network, the Pixel's BD
address, a candidate service UUID, a working no-root Android HCI capture
pipeline, and a handshake that is already transport-generic with `Target` and
`Medium` slots waiting.

---

## 2026-11-08 — Found the Quick Share Bluetooth UUID, and proved the medium is real

Two results that unblock the transport, from `docs/BLUETOOTH_FINDINGS.md`.

**Quick Share genuinely uses classic Bluetooth with no shared network.** A
Z Fold → Pixel transfer with the Pixel's Wi-Fi off, captured from the Z Fold's
HCI log, shows `Create Connection` → `Connect Complete` to
`AA:BB:CC:11:22:33` (the Pixel), handle 7, BR-ACL. Not a hotspot fallback —
the `BLUETOOTH` medium.

**The service UUID is almost certainly
`a82efa21-ae5c-3dde-9bbc-f16da7b16c5a`.** An SDP query to the Pixel, with its
Bluetooth settings page open so it was discoverable, returned twelve standard
profiles (OBEX, A2DP, HFP, PBAP, MAP, PAN…) and exactly one vendor-specific
entry. A web search did not corroborate it, so this is inference, not fact —
but it is the only non-standard UUID on a phone that had just accepted a Quick
Share transfer over classic Bluetooth.

That makes the previously-inconclusive inverted experiment decisive: register
an RFCOMM profile with this UUID, make the adapter discoverable, Wi-Fi off on
the phone, and see whether it offers us.

**Capturing an Android HCI log without root** took several wrong turns, now
recorded so they are not repeated: the bug report contains no Bluetooth log at
all; the "HCI snoop log socket" setting listens on no port we could find; and
`/data/log/bt/` is root-only. What works is
`adb shell dumpsys bluetooth_manager`, which embeds a base64+zlib **btsnooz v2**
blob. Converter kept at `docs/btsnooz2btsnoop.py`.

Its limit is in the name — a *summary*. Of 6990 records, 5742 events and 1242
commands but only **6 ACL**, so L2CAP/SDP/RFCOMM payloads are absent. That is
why the UUID had to come from an SDP query rather than the capture.

**Two process notes.** I spent three rounds guessing the btsnooz record layout
before reading the published format, which took one call and got it right
immediately — my guesses all assumed HCI packet types 1–4 when btsnooz uses its
own enum (`0x10` in-event, `0x20` out-command). And several timed experiments
were wasted because the capture window closed before Alex could see the
instruction; asking first and running after is the correct order for anything
needing a phone in hand.

---

## 2026-11-07 — Bluetooth discovery investigated: the plan was wrong

Full writeup in `docs/BLUETOOTH_FINDINGS.md`. Investigated before writing
transport code, which was the right call: **BLE discovery cannot produce a MAC
to connect to, as I had sketched it.**

**What works:** a MAC *is* available from a BLE advertisement, and `rqs_lib`'s
`BleListener` already receives it in `ServiceDataAdvertisement { id, .. }` and
throws it away. Extracting it would be trivial.

**What does not:** the phone emits no Quick Share BLE advertisement while its
share sheet is open. Sixty seconds of scanning produced zero payloads in the
format `rqs_lib` itself advertises (`fc 12 8e 01 …`). The four `0xFE2C`
advertisements seen were all 19 bytes beginning `10 52`, from two addresses that
both resolve to the same Sony earbuds — the second being a randomised address
for the same device. Nothing from the phone, and no classic Bluetooth presence
for it either.

`0xFE2C` is Google's Fast Pair UUID, which Quick Share shares, so the UUID alone
does not identify Quick Share traffic. **Which means `rqs_lib`'s `BleListener`
reports false positives** — it read a pair of earbuds as "a device is sharing
nearby" four times in a minute. Only a wake-up hint, so the cost is wasted work
rather than wrong behaviour, but distinguishing the payloads is a four-byte
check.

**The inverted experiment ran, and was inconclusive.** Adapter set
`Discoverable: yes` / `Pairable: yes` (BlueZ allows both over D-Bus without
root), Wi-Fi off on the phone, Quick Share opened: the machine did not appear in
the phone's list. Flagged as ambiguous before running and it is — **the RFCOMM
service UUID is in no source available here.** `rqs_lib` and Packet have only
`0xFE2C`, the BLE one, and the proto carries the RFCOMM identity inside the
*bandwidth upgrade* frame (`BluetoothCredentials { service_name, mac_address }`),
so it is negotiated per connection rather than well-known. A device advertising
no matching service is not expected to be offered, so the negative says little.
Adapter reverted afterwards.

**So Bluetooth is blocked on protocol knowledge, not on code.** The groundwork
holds and is useful regardless: the handshake is transport-generic, `Target` has
a Bluetooth slot, `Medium::Bluetooth` is in the device model, `Device.addresses`
can hold a MAC. What is missing is what to advertise and what to connect to, and
that is not guessable. Three routes, cheapest first: research the public
reverse-engineering of Nearby Connections' Bluetooth medium; `sudo btmon` (needs
root, and needs the phone to have something to attempt *to*); or a second
Android device to capture a working Wi-Fi-disabled transfer. Until one of those,
further code would be guesswork.

**Two method failures of mine, both self-inflicted and both caught.** The first
scan deduplicated on `(id, address, port)`, hiding repetition. The second piped
the probe through `tail -70`, which discarded exactly the UUID lines the
question turned on — and I briefly read a "zero Quick Share payloads" count off
that truncated output, which was meaningless. Same pattern as the earlier
endpoint-id capture: when the question is whether something appears, filtering
the output before looking at it destroys the measurement.

Probe kept at `docs/ble-probe.rs`.

---

## 2026-11-07 — Option B: identity separated from address

Implemented the design note's recommendation. D-016 has the detail; the shape
of it:

- **Fork** (`5b5a3e2`): `EndpointInfo.endpoint_id` from the mDNS fullname;
  `SendInfo` carries a `Target` enum and `connect` dispatches on it; the
  outbound error path reports the caller's id rather than the address.
- **Here**: `Device` has `addresses: Vec<Address>` with a `Medium`, `Registry`
  is keyed on the endpoint id, `best_address` picks the medium, and
  `ListDevices` swaps `address: s` for `addresses: a{ss}` plus `medium: s`.

**The IP-based deduplication is gone entirely.** It existed to paper over the
key being the address; keying on identity removes the problem rather than
compensating for it. Its replacement tests are better questions: a device that
moves address stays one device, two devices sharing an address stay two, and a
Wi-Fi announcement does not disturb a Bluetooth address.

**A wrinkle found while implementing.** A removal event carries only `rqs_lib`'s
`"ip:port"` id and *no* endpoint id, so a withdrawal cannot be matched by key at
all. `Registry` looks withdrawals up by address as well. Easy to have missed,
and it would have manifested as devices never disappearing.

**The contract break was the point of doing it now.** `address` is gone and `id`
means something different. Only our own CLI consumes it; after a Nautilus or
Shell extension existed this would have been a migration. The
`the_dict_keys_are_stable` test caught it, which is what it is for, and a new
test now checks `device_dict` against `RemoteDevice::from_dict` so the two
halves of the contract cannot drift.

`medium` is also surfaced in the CLI's device table rather than left unused —
worth seeing, since Bluetooth is roughly a thousand times slower than Wi-Fi.

50 tests, clippy clean.

**Confirmed against the phone.** `endpoint_id: Some("EXAMPLEENDPT02")` alongside
`id: "192.0.2.129:53601"`, and the table reads
`Pixel 6a | wifi_lan | 192.0.2.129:53601 | EXAMPLEENDPT02`. Identity
and address are visibly separate, and the id matches what the controlled
stability check measured earlier the same evening.

Narrow gap left: that run used the standalone path, so the `addresses` map going
over D-Bus is test-covered rather than phone-covered. Same `Device` code
underneath either way.

---

## 2026-11-07 — Endpoint id confirmed stable across a share session

The controlled check TRANSPORT_IDENTITY.md called for. `avahi-browse` running
continuously while the sheet was opened, closed and reopened; the close shows as
a 41-second gap in which avahi lost the service, so what follows is a fresh
resolution rather than cache.

**Unchanged either side: endpoint id `EXAMPLEENDPT02` *and* address
`192.0.2.129:53601`.** One distinct id and one distinct address in the entire
capture.

So option B has a principled identity — the first label of the mDNS `fullname`,
which `rqs_lib` discards in favour of synthesising `ip:port`. The claim is
narrow: one device, one network, one sitting, establishing only that the id
survives a session ending, which is the lifetime needed.

It also finishes off the "fresh port per session" assumption: the port was
identical across a deliberate close/reopen. That was wrong, and the IP-based
dedup it justified needs a different rationale — which option B replaces anyway.

**A method mistake worth keeping.** The first capture deduplicated sightings on
`(endpoint, address, port)`. If the id is stable, that collapses both sessions
into one line, indistinguishable from having seen the device once — the script
could not answer the question it was written for. Rerun logging every sighting,
where the gap is the signal. When testing whether something changes,
deduplicating is deleting the measurement.

---

## 2026-11-07 — Design note: device identity across transports

`docs/TRANSPORT_IDENTITY.md`. Written before any Bluetooth code because the
answer changes the D-Bus contract.

**The finding that shapes it:** `ip:port` is currently doing four separate jobs
— device identity, dial string, transfer id, and dedup key — which coincide only
because there is one medium. A Bluetooth-only device has no `ip:port` at all.

Recommendation is to separate identity from address: a stable id of our own,
plus a list of medium-tagged addresses, with the daemon choosing the medium
(it is the only component that knows what is reachable). Do it now: the only
consumer of `ListDevices` is our own CLI, so changing the meaning of `id` costs
one file today and a published contract later.

Also flagged a latent bug for the same pass: `start_outbound` returns
`device.id` as the transfer id while `manager.rs`'s outbound error path reports
`si.addr`. Identical strings today, divergent the moment identity and address
separate — a client would get a failure for a transfer id it never heard of.

**The research question turned out to be partly answered by our own logs.** The
mDNS `fullname` carries the real endpoint id, and the Pixel shows
`EXAMPLEENDPT01` across four share sessions on 2026-10-01 and `EXAMPLEENDPT02`
across three on 2026-11-07. So it is stable across sessions and changes over
longer periods — which is exactly the lifetime option B needs, since nothing
persists an id.

**And it corrected an assumption of mine.** I had written that a phone picks a
fresh port per share session, which is what justified the IP-based dedup in
`Registry::apply`. Seven sessions say otherwise: the port was `53601` every
time, and what changed was the IP, across networks. The dedup does something
real, just not the thing I claimed. Corrected in RQS_LIB_NOTES.md.

---

## 2026-11-07 — The handshake is no longer tied to TCP

Step zero of the Bluetooth work, and the thing that was actually blocking it:
`InboundRequest` and `OutboundRequest` held a `TcpStream` concretely, so no
other medium could be plugged in at all.

Both are now generic over a `Transport` trait in the fork (`b6699184`):
`AsyncRead + AsyncWrite + Unpin + Send`, with a blanket impl. That is the whole
requirement — length-prefixed frames over an ordered, reliable stream. D-015.

**Nothing else had to change.** `manager.rs` needed no edits, since `TcpStream`
satisfies the bound and the types infer. No behaviour changed: our 48 tests pass
and TCP discovery still works.

**Verified** by two tests constructing a connection over `tokio::io::duplex`, an
in-memory pipe. Them compiling is the demonstration. They also open a door that
was shut before — the protocol can now be exercised in tests with no network at
all, which is how the Bluetooth path should be tested before any hardware is
involved.

**What this buys.** `bluer::rfcomm::Stream` already implements `AsyncRead` and
`AsyncWrite`, so a Bluetooth connection is now a matter of *obtaining the
stream*, not of touching protocol code.

**What is still missing for Bluetooth**, in rough order:

1. A listener: `bluer::rfcomm::Listener::bind` plus a BlueZ profile
   registration, advertising Quick Share's service UUID so a phone will connect.
2. A connector: `bluer::rfcomm::Stream::connect` to a discovered device's MAC.
3. Discovery over BLE rather than mDNS — a phone with no shared network is not
   on any mDNS-reachable segment, so `EndpointInfo` would have to carry a MAC
   instead of an `ip:port`. This is the part that touches our `devices.rs` and
   the D-Bus API, since both currently assume an `ip:port` identity.
4. Advertising `Medium::Bluetooth` in the connection request, and ideally
   handling `BandwidthUpgradeNegotiation` so a hotspot can take over for
   throughput — RFCOMM is on the order of 1–2 Mbit/s.

Step 3 is the one with design consequences beyond the fork, and worth thinking
about before writing code.

---

## 2026-11-07 — Silenced the post-transfer ERROR line

`rqs_lib` logged `error while handling client: early eof (Finished)` at ERROR
after every *successful* transfer — the peer hanging up is simply how a transfer
ends, and the read that notices it reports EOF.

Both `manager.rs` sites already distinguished the `Finished` state, since they
suppress the synthetic `Disconnected` message for it; the fix extends that same
check to the log line. Applied to the inbound path as well as the outbound one,
because a receiver hits this when the *sender* hangs up. Fork rev `cf71b1b`.

**Verified** by repeating the confirmed-working send: transfer completed,
`succeeded=true`, and no ERROR line anywhere in the daemon log where the same
run previously produced one.

---

## 2026-11-07 — Sending works end to end, confirmed on the device

A file sent from the daemon arrived on the Pixel 6a, with the PIN notification
on screen and the contents readable. **First confirmed successful send**, and
the receiving end is the one saying so this time.

One run validated four things at once:

- **D-014's hypothesis.** The race between `Finished` and the Disconnection
  frame was the cause. With the fork's reordering the frame goes out first, the
  daemon no longer tears down underneath it, and the transfer lands. Promoted
  from "hypothesis with a mechanism" to confirmed.
- **The fork** (D-012, D-014) — both fixes in a real handshake.
- **The D-Bus chain** — `SendFiles` → `TransferPin` → `TransferFinished`,
  driven by the CLI as a client, with a discovery lease taken and released.
- **Outbound notifications** — the PIN notification reached the screen, which
  is the whole reason it exists: a client sending from a file manager has
  nowhere else to put it.

Daemon log for the record:

```
started discovery holder=:1.1366
outbound transfer awaiting acceptance id=192.0.2.129:53601 pin=4517
All files have been transferred
outbound transfer ended id=192.0.2.129:53601 succeeded=true error=
ERROR rqs_lib::manager: TcpServer: error while handling client: early eof (Finished)
```

That last line is `rqs_lib` reading the socket after the exchange and finding
the peer had hung up. Benign — the transfer had already succeeded — but it logs
at ERROR, so it will look alarming in a journal and is worth silencing in the
fork at some point.

Idle with the D-Bus API served: **7.1 MB RSS / 4.3 MB PSS / 1.0 MB private
dirty / 3 threads**.

---

## 2026-11-07 — A byte-complete transfer still failed on the phone

A 39-byte send reported the PIN, every byte acknowledged, `All files have been
transferred` and `Finished` — and the Pixel showed a transfer error. Third time
I have reported a send as successful on the strength of our own terminal state
and been wrong.

**Mechanism found.** `rqs_lib`'s outbound loop announces `Finished` *before*
sending its graceful Disconnection frame. The CLI sees `Finished`, returns, and
calls `rqs.stop()`, which cancels the very task that would send that frame. The
receiver loses the connection without a clean close and marks the transfer
failed, having taken every byte. Visible in the log: `All files have been
transferred` → `Sent to…` → `tracker cancelled`, within milliseconds.

**Fixed in two places.** The fork swaps the two statements, so `Finished` means
the protocol completed rather than that the last byte was written (`7725c0b`,
now pinned). And both the CLI's one-shot path and the daemon's shutdown wait a
second before stopping when a transfer has just completed — which covers the
window whatever revision is underneath, and matters because the pin will move
and this failure is silent on our side. D-014.

**This is a hypothesis with a mechanism, not a confirmed fix.** The race is real
and readable in the source, and it explains the symptom. Whether it is the only
cause is unknown until something actually lands on the phone.

**Also explained: no notifications.** That run went through the *standalone*
path — no daemon was up, so the CLI stood up its own service and printed to the
terminal. Only the daemon posts notifications. Not a bug, but it means the
outbound notification integration is still unexercised, and I should have said
so when suggesting the test rather than implying it would cover them.

**Correction to an earlier claim.** I called Phase 3 "complete except the
Nautilus extension". Sending has never once succeeded end to end, so that was
wrong.

---

## 2026-11-07 — Outbound notifications in the daemon

An outbound transfer now produces **one** desktop notification, updated in
place: "Accept on <device> — PIN nnnn" at critical urgency, replaced by "Sent
to <device>" or "Couldn't send to <device>" with the reason.

The PIN notification is the point of this. A client driving `SendFiles` from a
file manager has nowhere to print a PIN, and without it the person at the other
device has no way to confirm the transfer is really from here — so nothing can
be accepted. The CLI prints it too, which means a terminal send reports twice;
mild duplication, and preferable to a GUI send being unusable.

`Notifier::replace` is back. I deleted it on 2026-09-30 as unused once inbound
completions turned out to want fresh notifications; outbound genuinely needs it,
because the "waiting" message and its result are one event.

The recipient's name is recorded in `start_outbound` when the device is
resolved, not read from the transfer events — those only echo back the name we
supplied, which would leave nothing to show for a transfer that somehow arrived
unrecorded.

**Verified:** the exact two-step sequence against GNOME Shell. `replace` reused
notification id 367 rather than stacking a second one, and both the success and
failure shapes post.

**Not verified:** the integration itself. `SendFiles` resolves only against
discovered devices — it correctly refused a fabricated `127.0.0.1:1` target —
so the notification path cannot be reached without a real device. Needs one send
to the phone.

48/48 tests, clippy clean. Also dropped an unused `emitter` parameter from
`handle_command` rather than silencing the warning.

---

## 2026-11-07 — The CLI is now a D-Bus client

`devices` and `send` use the running daemon when there is one and fall back to
their own short-lived service otherwise. That removes the duplicate mDNS stack,
TCP listener and BLE advertiser that previously stood up beside the daemon's.

**Two gaps this exposed in the API I had just written.**

The daemon was **dropping outbound events entirely** — `handle_event` returned
early on `TransferKind::Outbound` with a "Phase 3" note — so `TransferFinished`
was declared but never emitted. The daemon now tracks outbound transfers and
emits it, with the three-way `Disconnected` distinction the CLI already had:
no PIN means the handshake failed, a PIN but no bytes means the receiver never
accepted, bytes in flight means a real drop.

And there was **no way for a client to learn the PIN**. It cannot be a
`SendFiles` return value — it does not exist yet when that call returns, since
it falls out of the key exchange afterwards. Added a `TransferPin(id, pin)`
signal. Without it, moving the CLI onto the API would have silently lost the PIN
display, which is the bug from 2026-10-01 all over again.

**Verified.** `devices` via the daemon and via the fallback; `send` rejecting a
missing file before touching the bus; `send` taking a lease and waiting for a
device that is not there. And the crash path: killing the CLI mid-wait had the
daemon's `NameOwnerChanged` watcher release its lease, which is precisely what
that watcher is for.

```
started discovery                        holder=:1.1324
client left the bus; released its lease  holder=:1.1324
```

File validation moved up into `send` so it happens once, before either path is
chosen, rather than being duplicated.

48/48 tests, clippy clean.

**Not verified:** an actual transfer through `SendFiles`, including the
`TransferPin` and `TransferFinished` signals. Needs the phone.

---

## 2026-11-07 — D-Bus API, and a look at transports without a shared LAN

**`org.quickshare.Daemon1` is live.** `StartDiscovery`, `StopDiscovery`,
`ListDevices` → `aa{sv}`, `SendFiles(target, paths)` → transfer id, a `Visible`
property, and `DevicesChanged` / `TransferFinished` signals. `src/dbus.rs` holds
the interface; the event loop gained a discovery branch, a command branch and a
housekeeping tick. D-013.

The interface holds no state: handlers push a `Command` into the event loop and
await a `oneshot`. That settles D-005's open question without reintroducing the
`Arc<Mutex<…>>` it removed.

**The lease machinery is now reachable and verified end to end.** It had sat
behind `#[allow(dead_code)]` since 2026-10-01 waiting for exactly this. Live
against the session bus:

```
started discovery          holder=:1.1314
client left the bus; released its lease  holder=:1.1314
joined running discovery   holder=:1.1315
client left the bus; released its lease  holder=:1.1315
```

The second client *joined* the running discovery instead of restarting mDNS —
the grace period working — and both departures were caught by the
`NameOwnerChanged` watcher. That watcher is what makes a lease safe to hand out:
without it a front-end crashing with one open would pin discovery and its BLE
advertiser on until the daemon restarted.

**Cost:** idle goes from 6.2 MB RSS / 3.5 MB PSS / 2 threads to 6.7 / 4.0 / 3.
`zbus` brings a task and a bus connection. Inside the 8 MB ceiling.

**Transports without a shared Wi-Fi network — researched, not started.** Alex
wants to head this way, so the API was kept transport-agnostic. Findings:

- `rqs_lib` advertises `mediums: vec![Medium::WifiLan]` and has **no**
  `BandwidthUpgradeNegotiation` handling — one `Medium::` reference in the whole
  crate.
- The protos already define `BLUETOOTH`, `WIFI_HOTSPOT`, `WIFI_DIRECT`,
  `WIFI_AWARE` and `WEB_RTC` with credential messages, so the wire format
  exists and the implementation does not.
- Both `InboundRequest` and `OutboundRequest` hold `socket: TcpStream`
  **concretely**, so step zero for any new transport is genericising the
  handshake over `AsyncRead + AsyncWrite`.

Most tractable route: a Bluetooth RFCOMM transport (`bluer` is already a
dependency) to get a connection with no shared LAN, then optionally a
`WIFI_HOTSPOT` upgrade for throughput. WebRTC needs Google's signalling and is
not viable. All of it lands in the fork from D-012.

48/48 tests, clippy clean.

**Not done:** the CLI still runs its own short-lived service rather than using
this API, and the daemon does not yet post notifications for outbound
transfers.

---

## 2026-11-07 — Forked `rqs_lib` and fixed the EC point bug

**Re-checked upstream first.** Nothing had been fixed. `nozwock/rquickshare` is
still at `c9ac93f` (2026-09-25) — the same HEAD as six weeks earlier — and
`Martichou/rquickshare`, the real upstream, is at `378d8ae` from **2025-06-16**,
roughly 17 months stale with 105 open issues. All four call sites across both
forks are byte-identical to what we analysed, and no issue in either tracker
mentions the error. D-001's "update deliberately when a fix lands" had nothing
to wait for.

**Forked and fixed.** `LXK-98/rquickshare`, branch `fix/ec-point-left-pad`, rev
`7b42f7d`, **branched at `17702ed`** so Packet remains a valid API reference and
the divergence is exactly one commit. `utils::p256_coordinate` right-aligns the
significant bytes into a fixed `[u8; 32]`; both `inbound.rs` and `outbound.rs`
go through it. D-012.

**Verified against the original reproduction.** 500,000 real P-256 keys through
the fork's actual helper: 1991 carry a short coordinate (0.398%), **0 decode
failures, 0 wrong or invalid points**. Every one of those 1991 failed before the
fix. The check now also asserts the rebuilt point *equals the original*, not
merely that it parses — a padding bug that produced a valid but different point
would be worse than one that fails loudly.

Fork: 6/6 tests. This project: rebuilt against it, 45/45 tests, clippy clean.

**Not done:**

- **The fix has not been offered upstream.** Both repos are dormant, but it is a
  small reviewable patch with a standalone reproduction and is worth filing
  regardless. Opening an issue or PR on someone else's repository is a
  publishing action, so it waits for Alex to ask.
- **Not retested end to end against the phone.** At 1 in 262 a successful
  transfer proves nothing about this fix either way; the 500k-key check is the
  real evidence. Worth one ordinary send to confirm the fork did not break the
  handshake in some other way.

---

## 2026-10-01 — Sending tested against the phone; an upstream crypto bug

**The send path reaches `Finished` and transfers every byte**, confirmed on a
57-byte text file: PIN shown, `SendingFiles`, 57/57 acknowledged, "All files
have been transferred". Whether the file is usable on the phone is **still
unconfirmed** — see the caution below.

**A caution I earned.** I reported an earlier run as "Sent" on the strength of
our side reaching `Finished`, and the phone showed an error. `Finished` means our
send loop completed after the receiver's Accept frame; it is not a report from
the receiving application. Do not call a transfer successful without the
receiving end agreeing.

**Bug fixed: the PIN was never shown when sending.** `rqs_lib`'s outbound path
never emits `WaitingForUserConsent` — zero occurrences in `hdl/outbound.rs` —
and that was the only branch where the CLI printed the PIN. So the PIN was
computed, logged by `rqs_lib` at INFO, and never surfaced, while the user watched
a terminal that gave no sign it was waiting for them. Result: `consent denied:
TimedOut`. The PIN now prints from whatever event first carries it.

**Upstream bug found and then confirmed empirically:
`elliptic curve point encoding error`.** A send failed during the handshake, and
the phone said *"Dateien können nicht übertragen werden"*. Both `inbound.rs` and
`outbound.rs` rebuild the peer's public key without left-padding a short
coordinate, producing 64 bytes where SEC1 requires 65.

Confirmed rather than assumed: `docs/ec-point-bug-repro.rs` replicates
`encode_point` and the rebuild logic against 500,000 real P-256 keys. It emits
the identical error string, and the failure count equals the short-coordinate
count exactly — 1910 of 500,000 — with no other failure mode. Left-padding
yields 65 bytes that decode to a valid point.

**The measurement corrected my own estimate.** I had said 1 in 128, reasoning
from 1/256 per coordinate. It is **1 in 262**: a short result needs the value
below 2^247, not 2^248, because a 31-byte value with its top bit set gets a 0x00
prepended and lands back at exactly 32 bytes. The measured 0.190% per coordinate
matches 1/512, not 1/256. A separate figure in the same run — 75.001% of keys
having at least one 33-byte coordinate, against a predicted 1−(1/2)² — confirms
the two's-complement model the whole analysis rests on.

Intermittent, unrelated to file type, and **it affects receiving too**. Full
analysis in RQS_LIB_NOTES.md; the fix is three lines in a fork, not yet taken.

One honest caveat: at 1 in 262 we were unlucky to hit this within a handful of
attempts (~1.5%). Consistent with chance, but a single observation cannot rule
out something else raising the real-world rate.

**Error messages corrected.** A failed handshake was being reported as
"{device} never accepted the transfer", which blames the user for something they
were never asked. `Disconnected` now resolves three ways, by how far the
handshake got: no PIN means the key exchange failed, a PIN but no bytes means
consent was really refused, and bytes in flight means a genuine drop.

**Testing constraint, from Alex:** only ever transfer to "Pixel 6a", and
only with files generated for the test. I had suggested sending one of their
received files from `~/Downloads`, which is someone else's content and not mine
to retransmit. Also disclosed: `MDnsDiscovery` TCP-probes every advertiser it
resolves, so `devices` is not passive.

---

## 2026-10-01 — Discovery verified against a real phone; two bugs from it

Testing `devices` against a Pixel 6a found two things no unit test would have.

**Device names were being wiped.** The phone resolved correctly as
`Pixel 6a`, then reappeared as `Unknown device`. Cause: a backwards
assumption about `rqs_lib`. Its `ServiceRemoved` branch sends
`EndpointInfo { id, ..Default::default() }` — the id and *nothing else*, so
`present` is `None` because that is `Option`'s default, and `Some(false)` is
never sent anywhere in the library. Reading `None` as "still present" applied
the empty removal as an update and erased the name.

**My test asserted the same wrong belief, which is why it passed.** It is now
inverted, named after the real behaviour, and carries a note saying what it used
to claim. The name is additionally guarded against being overwritten by an
absent one, because losing a device's name is the one failure of this class a
user actually sees.

Removals now *withdraw* with a 10 s grace rather than deleting. The same run
showed `ServiceRemoved` arriving seconds after `ServiceResolved` while the
phone's sheet was open and it was still reachable, so acting instantly would
blink a device out of the picker exactly as the user reaches for it. `rqs_lib`'s
own comment claims a threshold here; none is implemented.

**Confirmed fixed against the phone:** a fresh run (different random port and
endpoint id, which is how one run is told from another — both are randomised per
start) printed the name once, took the `ServiceRemoved` silently, and listed
`Pixel 6a`.

**`send` could not find a device that `devices` had just found.** The sheet had
closed in between, and `send` gave up after its 8 s window. That is not a bug so
much as a design mistake: a phone advertises only while its Quick Share sheet is
open, and nobody can hold that open while typing a command. `send` now waits up
to 45 s and returns the *moment* the named device appears, so the wait costs
nothing when it is already there and makes "start the send, then pick up the
phone" the normal way to use it.

Clippy clean, 45/45 tests.

---

## 2026-10-01 — Phase 3 started: discovery and sending

**Added.** `devices` and `send` subcommands, a device registry, and the
`--help`/argv handling they needed.

- `src/devices.rs` — the device registry: applies discovery events, expires
  entries not seen in 120 s, and hands out **refcounted discovery leases** so a
  second front-end closing its menu cannot stop discovery out from under the
  first. The leases are not reachable yet — only the D-Bus API can hold one —
  but refcounting is designed in now because retrofitting it is the bug
  docs/EXTENSION_PLAN.md exists to prevent.
- `src/cli.rs` — one-shot `devices` and `send`. Both run their own short-lived
  service and advertise as **invisible**: sending needs discovery, not
  discoverability, and a one-shot should not briefly appear as a second receiver
  beside a running daemon. They use a random port so they cannot collide with a
  daemon holding a configured one.
- `send` validates every file before starting a service, so a typo fails
  immediately rather than after ten seconds of discovery, and refuses
  directories, which Quick Share cannot send.
- Ambiguous device names are an error listing the ids, never a guess. Sending a
  file to the wrong device is not recoverable.

**Three things reading `rqs_lib` turned up**, all now in RQS_LIB_NOTES.md:

1. **`EndpointInfo.id` is not a device id — it is literally `"ip:port"`.** It
   changes whenever a phone re-advertises on a fresh port, so the same device
   arrives under a new id and a naive picker lists it twice. The registry now
   deduplicates by address. This also **corrects advice I had written into
   EXTENSION_PLAN.md** about exposing "stable device ids" over D-Bus: there are
   none to expose.
2. **Discovery filters out devices on this machine's own IPs**
   (`is_not_self_ip`). So Packet running here is invisible to us — correct, but
   it means "no devices found" is expected when the only advertiser is local.
3. **`OutboundPayload` has one variant, `Files`.** Sending text, a URL or Wi-Fi
   credentials is not supported by the library at all; those exist only inbound.

**Verified.** Clippy clean, 42/42 tests. `devices` runs end to end: mDNS
discovery, BLE advertiser on hci0, clean shutdown. It correctly found nothing —
`avahi-browse` confirmed the only Quick Share advertiser on the LAN at the time
was on this host, which `is_not_self_ip` filters by design.

**Not verified: the send path itself.** No file has been sent. It needs a second
device with its Quick Share sheet open, which needs a person holding a phone.

**Two test fixtures were wrong and the new deduplication caught them.** Both
built several "devices" sharing one IP, which is not a thing that happens on a
LAN; the dedup correctly collapsed them into one and two tests failed. The
fixtures now mirror `rqs_lib`'s real id format.

---

## 2026-10-01 — Local cleanup, and two bugs it turned up

A pass over local state after the GitHub remote was removed. Two of the six
items were real bugs, both found by reading `rqs_lib` rather than by testing.

**Bug: the "Open" button could open the wrong file.** `rqs_lib` resolves
filename collisions itself (`lorem.txt` → `lorem (1).txt`) but reports the
**original** name in the transfer metadata, not the one it wrote. Joining the
reported name with `download_dir` therefore pointed at the *pre-existing* file
whenever a collision happened — so receiving a second `IMG_1234.jpg` and
pressing Open would have shown the first one. `open::resolve_received` now picks
the newest of the candidates `rqs_lib` could have chosen. It is a heuristic,
reliable only because the file was written seconds earlier; the real fix belongs
upstream, in reporting the path actually written.

**Bug: any argument was silently ignored.** `quickshare-daemon --help` started
the daemon and bound a port. So would `--visible`, or any typo. Now `--help`
and `--version` do what they should and an unknown argument exits 2 without
starting a network service. Found by running `--help` to check the install,
which started a daemon instead.

**Also fixed.**

- Removed the dead `origin` remote, so git operations no longer point at a
  repository that is being deleted.
- `~/.config/quickshare-daemon/config.toml` still had
  `device_name = "dell.fritz.box"` from before the FQDN fix — the file is
  written once, so fixing the default did not fix the existing file. Now `dell`.
- `cargo install --path .` so the systemd unit's `ExecStart`
  (`%h/.cargo/bin/quickshare-daemon`) resolves. It previously pointed at
  nothing.
- Removed a stray `/tmp/qsd-final.log` left by an earlier test run.

**Resolved, with no work needed:** PLAN.md's Phase 2 item for filename
collisions. `rqs_lib` already handles it, in the conventional way, and anything
we added would race with it. Documented in RQS_LIB_NOTES.md.

**Corrected an earlier claim of mine.** I had flagged the `LICENSE` file's
`<year> <name of author>` placeholders as something to fill in for GitHub's
license detection. They are part of the GPL's own "How to Apply These Terms"
appendix, which is meant to ship verbatim — filling them in would be editing
the license text. Nothing to fix. GitHub's "not detected" on a freshly created
repository was almost certainly indexing lag.

Clippy clean, 25/25 tests.

---

## 2026-09-30 — Receiving confirmed from Android; Open / Show in Folder buttons

**A file was sent successfully from an Android phone.** That closes the last
open item in Phase 1's checkpoint. Reported by the user rather than observed
here — see the caveat at the end of this entry.

**Added, on request:** the notification for a completed file transfer now
carries **"Open"** and **"Show in Folder"** buttons.

- `src/open.rs` — `reveal()` via `org.freedesktop.FileManager1.ShowItems`,
  which opens the folder with the files *selected* and takes several URIs at
  once (D-011), falling back to `xdg-open` on the directory if no file manager
  implements it. `open_file()` via `xdg-open`, refusing extensions that would
  execute rather than display (D-010).
- `src/notify.rs` — the signal-waiting logic behind `ask_consent` extracted
  into `await_action`, so button-carrying notifications are one primitive
  rather than two copies. `ask_consent` is now a thin mapping over it.
- `src/main.rs` — completed file transfers post their notification from a
  spawned task with a one-hour button lifetime, since the user may be away from
  the machine when a file lands.

Two details that came out of reading the source rather than guessing:
`rqs_lib` reports **bare file names, not paths**, so they must be joined with
`download_dir`; and `file://` URIs are percent-encoded by hand because received
names routinely contain spaces, `#` and non-ASCII, any of which silently
corrupts a naively formatted URI.

**Verified.** Clippy clean, 21/21 tests. `org.freedesktop.FileManager1` is
present on this system (Nautilus), and `reveal()` was called against it for
real with two URIs at once — the multi-file selection path — and accepted, so
the D-Bus signature is right. That was worth checking directly: a wrong
signature is a runtime error that unit tests cannot see and that would make
the button fail silently.

The notification-with-buttons path itself is the same `await_action` code
already confirmed twice by hand on the consent prompt (Accept at 31.3 s,
Decline at 8.8 s). A later attempt to have the new buttons clicked timed out
unanswered after 90 s, so "Open" and "Show in Folder" have not been pressed by
a human — only the code underneath them has been exercised.

**Caveat on the Android test.** No file newer than 20:56 exists in
`~/Downloads`, and this daemon first ran at about 21:09 — so the successful
transfer cannot be attributed to it from the filesystem alone. Packet was also
running and advertising throughout. Worth redoing once with the daemon in the
foreground and Packet stopped, watching for the consent prompt and the
`transfer finished` log line, before treating end-to-end receiving as proven.

---

## 2026-09-30 (milestone) — It builds, it runs, it hits the memory target

`protoc` was installed, which unblocked everything.

**It compiled on the first attempt**, with no changes to code that had only
ever been checked against a hand-written stub. The stub approach in
RQS_LIB_NOTES.md was worth the time.

**It runs.** Config written on first start, notification server reached, TCP
listener bound, mDNS broadcasting, BLE listener started, NetworkManager
watched, clean SIGTERM shutdown that cancels every task and unregisters the
mDNS service.

**It is discoverable.** `avahi-browse -rpt _FC9F5ED42C8A._tcp` sees it on every
interface at the port it logged, device name in the TXT record. An Android
phone was advertising the same service type on the same network.

**It hits the target.** Release build, idle, measured from `/proc` externally:

| | RSS | PSS | PrivDirty | Threads |
|---|---|---|---|---|
| Packet | 44.7 MB | 42.4 MB | 34.6 MB | 30 |
| this | 6.2 MB | 3.5 MB | 0.8 MB | 2 |

PLAN.md asked for ≤ 8 MB. Clippy clean, 17/17 tests, now against the real
`rqs_lib`.

**`MALLOC_ARENA_MAX=2` measured as a no-op**, three runs each way. The first
comparison appeared to show a 4.5× private-dirty win; it was an outlier from a
process still starting, and an empty `MALLOC_ARENA_MAX=` is not the same as
unset. Both mistakes were mine and both would have gone into the docs as fact
if the run had not been repeated.

**Fixed:** the default device name was the FQDN (`dell.fritz.box`), which is
what a phone would have shown on its share sheet. Now the short hostname. An
existing `config.toml` keeps the old value — delete it to regenerate.

**The one thing left in Phase 1:** sending an actual file from a phone. It
needs a human holding one.

---

## 2026-09-30 (later still) — Network watcher and SIGHUP reload

**Done.** Both of Phase 2's items that do not depend on `rqs_lib` being
buildable.

- `src/network.rs` — NetworkManager `StateChanged` watcher: state mapping,
  down → up edge detection, 3 s debounce, cancel-safe across `select!`. D-009.
- `src/main.rs` — the reconnect branch and a SIGHUP branch that reloads the
  config, applies `download_dir` in place through `rqs_lib`'s setter, and
  restarts the service only when `needs_service_restart` says so.
- Restarts are **deferred** while transfers are in flight and run the moment
  the last one ends, via a `restart_pending` flag.

**Verified.** `src/network.rs` run against the live system bus, read-only:
connects, reads `ConnectedGlobal`, and reports no spurious reconnect on a
stable link over 5 s. Clippy clean, 16/16 tests.

**Not verified.** The down → up transition and the debounce, which would mean
taking the network down — not something to do to a working machine unasked.
The SIGHUP path cannot run at all until the daemon can start.

**Two bugs caught while writing this, both mine.** `settle()` had a
non-exhaustive inner match and lost a reconnect if `select!` dropped the future
mid-debounce (fixed with `pending_reconnect`). And the first cut of
`restart_service` *refused* a restart during a transfer rather than deferring
it, which is weaker than what PLAN.md asks for; rewritten to actually defer.

---

## 2026-09-30 (later) — Config and notifications, verified for real

**Done.**

- `src/config.rs` — the `config.toml` from PLAN.md: serde + toml, defaults on
  first run with a commented header, `deny_unknown_fields` so a typo is a
  startup error rather than a setting that silently does not apply, validation
  of the values that would otherwise fail somewhere less obvious, and
  `needs_service_restart` stating which keys need the service restarted (the
  SIGHUP path in Phase 2 consumes it).
- `src/notify.rs` — `org.freedesktop.Notifications` over `zbus`: the consent
  prompt with Accept/Decline buttons, the timeout, completion and failure
  notifications, and `format_bytes`.
- `src/main.rs` rewritten to use both. Consent prompts are spawned off the
  event loop (D-008); auto-accept from config skips the prompt but still posts
  a notification; text, URL and Wi-Fi payloads each get their own completion
  message; failures are announced only for transfers that had been accepted.
- Decisions D-006 (raw notification spec over the XDG portal), D-007 (`zbus`
  on the tokio reactor, not async-io), D-008 (what gets spawned and why).

**Verified — against the live session bus, no stub involved.** `src/notify.rs`
was compiled directly into a test harness and run against GNOME Shell 50.4:

- `GetCapabilities` → `actions`, `body`, `body-markup`, `icon-static`,
  `persistence`, `sound`. So `supports_actions()` is true here.
- `Notify` returns a server id; `CloseNotification` works.
- A consent prompt nobody answers returns `TimedOut` after exactly its timeout
  and closes itself.
- A prompt answered by a human resolved in 8.8 s. It returned `Decline`, which
  is correct for both the Decline button and a swipe-away, and logging was off
  in that run, so which of the two happened is unknown.

**Also verified, via the stub:** `cargo check --all-targets` and
`cargo clippy --all-targets` clean, 14/14 tests passing across all four
modules.

`format_bytes` had a bug the tests caught: the unit table stopped at TB, so
`u64::MAX` formatted as "18446744 TB". Extended to PB and EB.

**Still blocked.** `protoc`. Nothing above involves `rqs_lib`, which is why it
could be verified at all.

---

## 2026-09-30 — Project bootstrap, blocked on `protoc`

**Done.**

- Read PLAN.md and verified its claims against this machine and against the
  pinned `rqs_lib` source. Three factual errors found and corrected in PLAN.md;
  see "Plan corrections" below.
- `git init`; GPL-3.0 `LICENSE` (verbatim, copied from the Packet clone,
  md5 `1ebbd3e3…`, 674 lines); `Cargo.toml`; `README.md`.
- `src/meminfo.rs` — self-measurement of RSS/PSS/private-dirty/threads from
  procfs. Reused by the Phase 4 tuning work.
- `src/main.rs` — `current_thread` runtime, RQS boot, transfer event loop,
  consent plumbing, SIGTERM/SIGINT shutdown, boot + interval memory logging.
- `data/quickshare-daemon.service` — systemd user unit.
- `docs/` — DECISIONS (D-001…D-005), RQS_LIB_NOTES, MEASUREMENTS, this journal.

**Verified.**

- Rust 1.94.1; `dbus-1` 1.16.2; NetworkManager and bluetooth both active.
- GNOME Shell 50.4's notification server advertises `actions` and `persistence`
  (`GetCapabilities`), so PLAN.md's accept/deny-inside-a-notification design
  works. No fallback UI needed.
- The dependency tree resolves against `rqs_lib` rev `17702ed` and every
  dependency compiles — `bluer`, `btleplug`, `mdns-sd`, `p256`, `sys_metrics`,
  `tracing-subscriber` all built.
- `src/meminfo.rs` compiles clean under edition 2024 and its 3 unit tests pass,
  checked with `rustc --test` against the already-built `anyhow` rlib, since
  `cargo test` is blocked by the same build failure as `cargo build`.
- Packet baseline re-measured: 44.7 MB RSS / 42.4 MB PSS / 30 threads. In
  MEASUREMENTS.md.
- 154 unique crates in the resolved tree. Seven of them are Bluetooth/D-Bus:
  `bluer`, `btleplug`, `bluez-async`, `dbus`, `dbus-tokio`, `dbus-crossroads`,
  `libdbus-sys` — recorded as evidence in D-002.

- **`src/main.rs` type-checks, via a stub.** Since the real `rqs_lib` cannot be
  built here, its public API was transcribed from the pinned source into a
  signature-only stub crate (218 lines, no behaviour) and the project's real
  sources were checked against it:
  `cargo check --all-targets` clean, `cargo clippy --all-targets` clean with
  zero warnings, `cargo test` 3/3 passing.
- **The event loop runs.** Built against the stub and executed for 8 s: the
  boot log, the 5 s settle measurement, the `QSD_MEASURE_SECS` interval
  (correctly *not* firing immediately, because of the `measure.reset()`),
  SIGTERM handling and the shutdown measurement all behaved as intended, and it
  reported **4.8 MB RSS / 3.1 MB PSS / 1 thread**. Recorded in MEASUREMENTS.md
  with the caveats it deserves.

  The stub lives in the session scratchpad at `typecheck/` and is deliberately
  **not** committed: it would drift out of sync with the pin and start lying.
  RQS_LIB_NOTES.md holds everything needed to regenerate it.

**Still not verified.** The stub proves the code is type-correct against the API
*as documented in RQS_LIB_NOTES.md*. If a signature there was transcribed
wrongly, the stub reproduces the mistake faithfully and the check passes anyway.
Nothing about protocol behaviour, mDNS, BLE or the real footprint is verified.

**Blocked.**

`cargo build` fails in `rqs_lib`'s build script:

```
core_lib/build.rs:15:6: called `Result::unwrap()` on an `Err` value:
Custom { kind: NotFound, error: "Could not find `protoc`. …" }
```

`prost-build` 0.13.5 does not bundle a protobuf compiler (confirmed: no
`protoc` anywhere in the crate), `protoc` is not installed, `$PROTOC` is unset,
and `sudo` on this machine requires a password. Fedora 44 ships
`protobuf-compiler` 3.19.6, which is new enough for prost-build. Needs:

```
sudo dnf install protobuf-compiler
```

Nothing else is known to block the build. `src/main.rs` has therefore **never
been compiled** — expect ordinary type errors on the first successful build.

**Next.**

1. Install `protoc`; `cargo build`; fix whatever `src/main.rs` got wrong.
2. Run it, confirm the daemon appears on an Android device's Quick Share sheet,
   and record the idle footprint in MEASUREMENTS.md — the Phase 1 checkpoint.
3. Then Phase 1 proper, in PLAN.md's order: `config.toml` loading, then
   notifications with accept/deny actions.

**Plan corrections made to PLAN.md.**

- `rqs_lib` rev: the plan's `c9ac93f…` is upstream HEAD, not what Packet pins.
  Corrected to `17702ed…`, with reasoning in D-001.
- Packet reference commit: `e42c097` → `f634d66`, the local clone's HEAD, which
  already contains the network-restart logic Phase 2 needs.
- "The logic we need is tangled into `src/window.rs` (~2,900 lines)": the RQS
  lifecycle is there, but the transfer handling is in
  `src/widgets/receive_transfer.rs` (831 lines) and
  `src/widgets/recipient_card.rs` (567). Total Packet `src/` is 5,578 lines,
  not ~6,200.

**Risks found that PLAN.md did not name.** All now recorded: the `experimental`
feature pulling in two BLE stacks *and* a second D-Bus stack (D-002),
`rqs_lib` logging through `log` rather than `tracing` (RQS_LIB_NOTES), the
`protoc` build requirement (above), and `TransferPayload::Wifi` existing as a
payload kind Phase 1 should not mishandle (RQS_LIB_NOTES).
