//! Publish-side decisions: when to re-pack in full, and whether a full
//! re-pack is safe to publish.
//!
//! The packing itself is [`super::pack`]; this is the judgement around it,
//! which is where the expensive mistakes have lived. Both were shell:
//! `needs_compaction` plus two `date` invocations whose flags differ
//! between GNU and BSD (`date -d @N` vs `date -r N`), and a `[ "$a" \< "$b" ]`
//! string comparison standing in for time arithmetic.

use anyhow::Result;

use super::manifest::{needs_compaction, CompactCfg, Manifest};

/// Why this lap is re-packing in full, or `None` to publish a delta.
#[derive(Debug, PartialEq, Eq)]
pub enum Compact {
    /// The 20% rule, the delta-segment cap, or a cold bank.
    Thresholds(String),
    /// MEASURED delta-container fetch time from this lap's restore. The
    /// static thresholds are proxies; this is the reclaimable cost itself,
    /// and it adapts to API latency and lap cadence for free.
    RestoreOverhead { secs: u64, budget: u64 },
    /// A referenced container is nearing the 90-day retention cliff.
    /// Re-uploading is the bank's only defiance of GC.
    Rewarm { oldest: String },
}

impl Compact {
    pub fn reason(&self) -> String {
        match self {
            Compact::Thresholds(s) => s.clone(),
            Compact::RestoreOverhead { secs, budget } => {
                format!("restore-overhead {secs}s>budget {budget}s")
            }
            Compact::Rewarm { oldest } => format!("rewarm oldest-container={oldest}"),
        }
    }
}

/// Seconds since the epoch for an RFC3339 UTC stamp as the artifacts API
/// writes them (`2026-07-30T06:22:11Z`).
///
/// Hand-rolled rather than pulling in a date crate for one comparison -
/// and the shell it replaces did this with two mutually incompatible
/// `date` invocations plus a string `<`, which happens to work only
/// because the format is fixed-width.
pub fn epoch_secs(stamp: &str) -> Option<i64> {
    let b = stamp.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |a: usize, z: usize| stamp.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (h, mi, s) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // days_from_civil (Howard Hinnant): shift the year so leap days land
    // at the end of the era, then count.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s)
}

/// Inputs the restore leaves behind for this decision.
#[derive(Default)]
pub struct RestoreEvidence {
    /// Wall seconds spent fetching DELTA containers (full packs are the
    /// post-compaction steady state, so their cost is not reclaimable).
    pub delta_restore_secs: Option<u64>,
    /// `created_at` of the oldest container this lineage still references.
    pub oldest_container: Option<String>,
}

pub struct CompactPolicy {
    pub cfg: CompactCfg,
    pub restore_budget_secs: u64,
    pub rewarm_days: i64,
}

impl Default for CompactPolicy {
    fn default() -> Self {
        Self {
            cfg: CompactCfg::default(),
            restore_budget_secs: 30,
            rewarm_days: 60,
        }
    }
}

impl CompactPolicy {
    pub fn from_env() -> Self {
        let get = |k: &str, or: u64| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(or)
        };
        let d = Self::default();
        Self {
            cfg: CompactCfg::from_env(),
            restore_budget_secs: get("COMPACT_RESTORE_BUDGET", d.restore_budget_secs),
            rewarm_days: get("REWARM_DAYS", d.rewarm_days as u64) as i64,
        }
    }

    /// Order matters only for which reason gets reported; any one of them
    /// is sufficient. `now` is injected so the rewarm rule is testable.
    pub fn decide(
        &self,
        head: Option<&Manifest>,
        ev: &RestoreEvidence,
        now: i64,
    ) -> Option<Compact> {
        // No head means a cold bank: there is nothing to re-pack.
        let head = head?;

        let verdict = needs_compaction(head, &self.cfg);
        if verdict.starts_with("yes") {
            return Some(Compact::Thresholds(verdict));
        }
        if let Some(secs) = ev.delta_restore_secs {
            if secs > self.restore_budget_secs {
                return Some(Compact::RestoreOverhead {
                    secs,
                    budget: self.restore_budget_secs,
                });
            }
        }
        if let Some(oldest) = &ev.oldest_container {
            let cutoff = now - self.rewarm_days * 86_400;
            // An unparseable stamp must NOT read as "ancient" - that would
            // re-pack the whole bank on a malformed field.
            if epoch_secs(oldest).is_some_and(|t| t < cutoff) {
                return Some(Compact::Rewarm {
                    oldest: oldest.clone(),
                });
            }
        }
        None
    }
}

/// Would publishing this full re-pack SHED content the head still names?
///
/// A referenced container that failed to restore leaves the store short,
/// and a full pack of a short store becomes the new HEAD under
/// newest-wins - silently losing history. When that happens the caller
/// falls back to a delta, which can only add.
pub fn would_shed(old_count: usize, new_count: usize) -> bool {
    new_count < old_count
}

/// Seconds since the epoch, now.
pub fn now_secs() -> Result<i64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bank::manifest::Segment;

    fn seg(bytes: u64, full: bool) -> Segment {
        Segment {
            name: "cas-seg-x".into(),
            bytes,
            blobs: 1,
            prefixes: "0".into(),
            artifact: None,
            run: None,
            role: None,
            full: full.then_some(true),
        }
    }

    fn head(segments: Vec<Segment>) -> Manifest {
        Manifest {
            version: 1,
            lineage: "lin".into(),
            generation: "g".into(),
            segments,
            ..Default::default()
        }
    }

    #[test]
    fn epoch_matches_known_stamps() {
        assert_eq!(epoch_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_secs("2000-01-01T00:00:00Z"), Some(946_684_800));
        assert_eq!(epoch_secs("2026-07-30T06:22:11Z"), Some(1_785_392_531));
        // A leap day must not slide the count.
        assert_eq!(
            epoch_secs("2024-03-01T00:00:00Z").unwrap()
                - epoch_secs("2024-02-29T00:00:00Z").unwrap(),
            86_400
        );
    }

    #[test]
    fn a_malformed_stamp_is_not_ancient() {
        // The failure mode this guards: an unparseable created_at reading
        // as "very old" would re-pack the entire bank every lap.
        for bad in ["", "not-a-date", "2026-07-30", "20260730T000000Z"] {
            assert_eq!(epoch_secs(bad), None, "{bad:?} must not parse");
        }
        let policy = CompactPolicy::default();
        let ev = RestoreEvidence {
            oldest_container: Some("not-a-date".into()),
            ..Default::default()
        };
        assert_eq!(
            policy.decide(Some(&head(vec![seg(1, true)])), &ev, 1_785_392_531),
            None,
            "a malformed stamp must not trigger a rewarm"
        );
    }

    #[test]
    fn rewarm_fires_only_past_the_cliff() {
        let policy = CompactPolicy::default(); // 60 days
        let now = epoch_secs("2026-07-30T00:00:00Z").unwrap();
        let fresh = RestoreEvidence {
            oldest_container: Some("2026-07-01T00:00:00Z".into()),
            ..Default::default()
        };
        assert_eq!(
            policy.decide(Some(&head(vec![seg(1, true)])), &fresh, now),
            None
        );

        let stale = RestoreEvidence {
            oldest_container: Some("2026-04-01T00:00:00Z".into()),
            ..Default::default()
        };
        assert!(matches!(
            policy.decide(Some(&head(vec![seg(1, true)])), &stale, now),
            Some(Compact::Rewarm { .. })
        ));
    }

    #[test]
    fn measured_overhead_beats_the_static_thresholds() {
        let policy = CompactPolicy::default();
        let now = epoch_secs("2026-07-30T00:00:00Z").unwrap();
        let over = RestoreEvidence {
            delta_restore_secs: Some(999),
            ..Default::default()
        };
        assert_eq!(
            policy.decide(Some(&head(vec![seg(1, true)])), &over, now),
            Some(Compact::RestoreOverhead {
                secs: 999,
                budget: 30
            }),
            "the reclaimable cost itself should fire even when bytes are quiet"
        );
        let under = RestoreEvidence {
            delta_restore_secs: Some(3),
            ..Default::default()
        };
        assert_eq!(
            policy.decide(Some(&head(vec![seg(1, true)])), &under, now),
            None
        );
    }

    #[test]
    fn a_cold_bank_never_compacts() {
        let policy = CompactPolicy::default();
        let ev = RestoreEvidence {
            delta_restore_secs: Some(9999),
            oldest_container: Some("1971-01-01T00:00:00Z".into()),
        };
        assert_eq!(
            policy.decide(None, &ev, 1_785_392_531),
            None,
            "with no head there is nothing to re-pack"
        );
    }

    #[test]
    fn shedding_is_detected_by_count() {
        assert!(
            would_shed(100, 99),
            "a short store must fall back to a delta"
        );
        assert!(!would_shed(100, 100));
        assert!(!would_shed(100, 101));
    }
}
