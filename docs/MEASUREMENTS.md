# Measurements

Every number here is reproducible with the command shown. PLAN.md's target is
**≤ 8 MB idle RSS**, ideally 4–6 MB.

Read the columns as: `RSS` is what the process has resident, shared pages
included; `PSS` is its private pages plus its fair share of shared ones — the
figure to compare between processes; `PrivDirty` is memory that exists only for
this process and cannot be reclaimed.

## Method

The daemon measures itself ([`src/meminfo.rs`](../src/meminfo.rs)) and logs an
`idle footprint (settled)` line 5 s after startup, then every
`QSD_MEASURE_SECS`. For any other process:

```bash
pid=$(pgrep -f '<pattern>'); grep -E '^(VmRSS|Threads)' /proc/$pid/status; grep -E '^(Pss|Private_Dirty)' /proc/$pid/smaps_rollup
```

"Idle" means: service started, nothing transferred yet, and at least 5 s
elapsed so mDNS and BLE have settled.

## Baseline — Packet (the thing being replaced)

| Date       | Process            | RSS      | PSS      | PrivDirty | Threads |
|------------|--------------------|----------|----------|-----------|---------|
| 2026-09-30 | `/app/bin/packet`  | 44.7 MB  | 42.4 MB  | 34.6 MB   | 30      |
| 2026-09-30 | `bwrap` (2 procs)  | 4.0 MB   | 0.3 MB   | 0.2 MB    | 1 each  |

Packet 0.7.1 Flatpak (`io.github.nozwock.Packet`), GNOME 50.4, idle with the
window closed. Matches PLAN.md's figures, which were taken the same day.

The two `bwrap` wrappers cost ~4 MB RSS but only ~0.3 MB PSS — almost all of
their resident memory is shared with the rest of the system, so PLAN.md's
"+4 MB for bwrap" overstates the real cost. The 42.4 MB PSS of the main process
is the number to beat.

## quickshare-daemon

Release build, commit `465384c`+, idle 12 s after start, `experimental` feature
on (both BLE stacks), `current_thread` runtime. Measured externally from
`/proc/<pid>/`, which agreed with the daemon's own self-report to within 0.1 MB.

| Date       | Build   | RSS    | PSS    | PrivDirty | Threads |
|------------|---------|--------|--------|-----------|---------|
| 2026-09-30 | debug   | 15.9 MB| 13.1 MB| 13.1 MB   | 3       |
| 2026-09-30 | release | 6.2 MB | 3.5 MB | 0.8 MB    | 2       |
| 2026-11-07 | release | 6.7 MB | 4.0 MB | 4.0 MB    | 3       |

The 2026-11-07 row is with the D-Bus API served: `zbus` adds a task and a
session-bus connection, costing ~0.5 MB RSS and one thread. Still inside the
8 MB ceiling. Note its private-dirty figure is a single reading taken 5 s after
start, so it is subject to the start-up inflation warned about below; the
2026-09-30 row's 0.8 MB came from three settled runs.

**PLAN.md's target was ≤ 8 MB RSS, ideally 4–6 MB. The release build lands at
~6.3 MB.**

Against the baseline: **7× less RSS, 12× less PSS, 15× fewer threads** than
Packet. Only ~0.8 MB is private dirty — the rest of the RSS is file-backed
(the binary and shared libraries), which the kernel can drop under pressure.

### `MALLOC_ARENA_MAX=2` makes no measurable difference

PLAN.md Phase 4 proposes it. Three runs each, with the variable genuinely
unset versus set to 2:

| Config      | RSS (kB)           | PSS (kB)           | PrivDirty (kB)  |
|-------------|--------------------|--------------------|-----------------|
| unset       | 6196, 6172, 6396   | 3454, 3501, 3476   | 808, 860, 832   |
| `=2`        | 6176, 6576, 6532   | 3434, 3614, 3497   | 792, 784, 804   |

Every difference is inside the run-to-run spread. This is the expected result
rather than a disappointing one: the tuning exists to stop glibc creating one
malloc arena per thread, and `current_thread` already got the thread count to
2. D-003 removed the problem that D-004's checklist was going to tune.

Kept in the systemd unit anyway. It costs nothing, and it is insurance if a
future `rqs_lib` starts spawning threads.

**A caution about single readings.** The first release measurement reported
3592 kB private-dirty — 4× every later reading — and was taken while the
process still had 3 threads and was presumably mid-startup. Believing it would
have meant reporting a 4.5× improvement from `MALLOC_ARENA_MAX` that does not
exist. Take three readings after the thread count has settled.

## Discoverability

Confirmed 2026-09-30 with `avahi-browse -rpt _FC9F5ED42C8A._tcp`: the daemon
advertises on every interface, at the port it reports in its own log, with the
device name in the TXT record's `n=` field. An Android phone on the same
network was visible in the same browse, advertising the same service type.

This is not a substitute for actually sending a file from a phone, which
remains the open item in PLAN.md's Phase 1 checkpoint.
