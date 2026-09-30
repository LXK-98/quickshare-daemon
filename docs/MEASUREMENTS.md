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

| Date       | Build | RSS    | PSS    | PrivDirty | Threads | Notes |
|------------|-------|--------|--------|-----------|---------|-------|
| 2026-09-30 | debug | 4.8 MB | 3.1 MB | 3.1 MB    | 1       | **`rqs_lib` stubbed out** — skeleton only, see below |
| _pending_  |       |        |        |           |         | real build blocked: `protoc` not installed |

### The stubbed figure, and what it does and does not tell us

4.8 MB RSS / 3.1 MB PSS / **1 thread**, debug build, with `rqs_lib` replaced by
a signature-only stub. So this is the cost of *our* code plus tokio, tracing and
`dirs` — and nothing of the protocol.

What it establishes:

- The `current_thread` runtime really does run on one thread (D-003). Packet
  needs 30. Since threads are the mechanism behind Packet's ~31 MB of
  heap/arena overhead, this is the single most important early signal.
- Our own overhead is ~3.1 MB PSS **before optimisation** — debug build, no
  LTO, no stripping, no `MALLOC_ARENA_MAX`.

What it does not establish: anything about the real footprint. The remaining
budget for `rqs_lib` + mDNS + both BLE stacks + `libdbus` + the `zbus` still to
come is roughly 8 − 3 = **5 MB**, and the release profile should hand a chunk of
the 3 MB back. Plausible, but D-002 stays the open question until measured.

Reproduced with: a stub crate at `scratchpad/typecheck/`, disposable — see
JOURNAL.md 2026-09-30.

Record for each entry: the commit, `debug` or `release`, whether the
`experimental` feature (both BLE stacks — see D-002) was on, and the runtime
flavour. Those four are the variables that move the number.

### Measurements worth taking at the Phase 1 checkpoint

1. Idle, debug build, `experimental` on — the honest starting point.
2. Idle, release build with the tuned profile (`lto`, `opt-level = "s"`,
   `panic = "abort"`, `strip`) — expected to be the big drop.
3. Idle, release, `experimental` **off** — prices what D-002 is buying. Only
   worth doing if (2) misses the 8 MB target.
4. Peak during a single large file transfer, plus idle again ~60 s after it
   finishes — shows whether the allocator gives pages back, i.e. whether
   PLAN.md's `malloc_trim` idea is needed.
5. Thread count in every case. Threads are the mechanism behind Packet's
   ~31 MB, so this is the leading indicator.
