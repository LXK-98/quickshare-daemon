// SPDX-License-Identifier: GPL-3.0-or-later
//! Self-measurement of the daemon's own memory footprint.
//!
//! The entire justification for this project is a memory number (see PLAN.md),
//! so the daemon measures itself rather than relying on an external tool being
//! run at the right moment. Numbers come from the same places `PLAN.md`
//! specifies: `VmRSS`/`Threads` from `/proc/self/status` and `Pss`/
//! `Private_Dirty` from `/proc/self/smaps_rollup`.
//!
//! `smaps_rollup` is cheap compared to walking `smaps` (the kernel aggregates
//! it internally), but it is not free: it still traverses the page tables. Call
//! it on boot and on an interval, not per transfer chunk.

use std::fmt;

use anyhow::{Context, Result};

/// A point-in-time view of this process's memory use, in kibibytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemSnapshot {
    /// Resident set size: pages this process has in RAM, shared ones included.
    pub rss_kb: u64,
    /// Proportional set size: private pages plus this process's share of any
    /// shared ones. The honest number to compare against other processes.
    pub pss_kb: u64,
    /// Private dirty pages: memory that exists only for this process and cannot
    /// be dropped or reloaded from disk. The part that is genuinely "ours".
    pub private_dirty_kb: u64,
    /// OS threads in this process. Each one costs a stack and, under glibc, can
    /// pull in another malloc arena, which is where Packet loses ~31 MB.
    pub threads: u64,
}

impl MemSnapshot {
    /// Reads the current snapshot for this process.
    ///
    /// Missing fields are left at zero rather than failing: `smaps_rollup`
    /// requires `CONFIG_PROC_PAGE_MONITOR` and is absent on some kernels, and a
    /// daemon should not refuse to run because it cannot measure itself.
    pub fn read() -> Result<Self> {
        let mut snap = Self::default();

        let status =
            std::fs::read_to_string("/proc/self/status").context("reading /proc/self/status")?;
        for line in status.lines() {
            match parse_kv(line) {
                Some(("VmRSS", v)) => snap.rss_kb = v,
                Some(("Threads", v)) => snap.threads = v,
                _ => {}
            }
        }

        // Absence is not an error; see the doc comment above.
        if let Ok(rollup) = std::fs::read_to_string("/proc/self/smaps_rollup") {
            for line in rollup.lines() {
                match parse_kv(line) {
                    Some(("Pss", v)) => snap.pss_kb = v,
                    Some(("Private_Dirty", v)) => snap.private_dirty_kb = v,
                    _ => {}
                }
            }
        }

        Ok(snap)
    }
}

impl fmt::Display for MemSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "rss={:.1}MB pss={:.1}MB private_dirty={:.1}MB threads={}",
            self.rss_kb as f64 / 1024.0,
            self.pss_kb as f64 / 1024.0,
            self.private_dirty_kb as f64 / 1024.0,
            self.threads,
        )
    }
}

/// Parses one `Key:   <value> kB` line from a procfs file.
///
/// Returns the key and the numeric value, ignoring the unit: every field we
/// read from these two files is reported in kB. `Threads:` carries no unit,
/// which is why the unit is not required to be present.
fn parse_kv(line: &str) -> Option<(&str, u64)> {
    let (key, rest) = line.split_once(':')?;
    let value = rest.split_whitespace().next()?.parse().ok()?;
    Some((key, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_procfs_lines() {
        assert_eq!(parse_kv("VmRSS:\t   44668 kB"), Some(("VmRSS", 44668)));
        assert_eq!(parse_kv("Threads:\t30"), Some(("Threads", 30)));
        assert_eq!(
            parse_kv("Pss:               42397 kB"),
            Some(("Pss", 42397))
        );
    }

    #[test]
    fn ignores_non_numeric_and_malformed_lines() {
        assert_eq!(parse_kv("Name:\tpacket"), None);
        assert_eq!(parse_kv("no colon here"), None);
        assert_eq!(parse_kv("Empty:"), None);
    }

    #[test]
    fn reads_own_process() {
        let snap = MemSnapshot::read().expect("/proc/self/status must be readable");
        // A running test process has resident pages and at least one thread.
        assert!(snap.rss_kb > 0, "got {snap}");
        assert!(snap.threads >= 1, "got {snap}");
    }
}
