//! Read what earthly's OTLP spans say about a build.
//!
//! earthly emits these through the standard autoexport and every run logged
//! `traces export: exporter export timeout` while throwing them away. They
//! name TARGETS, which nothing else here does: the proxy sees LLB ops and
//! digests, and reconstructing a target from those means decoding a base64
//! `llb.customname` and hoping.
//!
//! What this deliberately does NOT report is a serial fraction. The finding
//! that 71% of `+test-no-qemu` is a serial base chain came from READING the
//! timeline - `+earthly-docker` runs 0-187s, everything else waits, the
//! tests run 192-271s - and two attempts to compute it automatically both
//! failed. Span overlap says 0% serial, because earthly holds a target's
//! span open while it WAITS and 36 blocked spans look like 36 running ones.
//! Counting only leaf spans says 25-44%, over a third of the wall clock,
//! because it discards too much. A number that contradicts the analysis it
//! is meant to support is worse than no number.
//!
//! Split BY LEG, which is the whole point. Aggregated across a run, `+base`
//! looked like 696 resolutions costing 4232s - an obvious thing to memoise.
//! Per leg it says the opposite: the baseline resolves it 575 times at 3.0s
//! and the fleet 121 times at 20.5s, so the repetition is earthly's own and
//! the fleet's problem is the price of a unit. A correctly computed
//! aggregate pointed at a fix that would have done nothing.

use std::collections::BTreeMap;

/// One target's contribution to one leg.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tally {
    pub count: u64,
    pub total_ms: f64,
    /// Every duration. These spans include waiting, so a mean over hundreds
    /// of cache hits and nine multi-minute blocks describes neither - the
    /// distribution is the only sound reading.
    pub each: Vec<f64>,
}

/// A leg of a run: one earthly invocation, one trace tree.
#[derive(Debug, Default)]
pub struct Leg {
    pub label: String,
    pub wall_ms: f64,
    pub targets: BTreeMap<String, Tally>,
    /// Every target span as (start, end) nanoseconds, so concurrency over
    /// time can be recovered. Durations alone cannot say how much of the
    /// wall clock had only one thing running, which is the number that caps
    /// what any distribution can do.
    pub spans: Vec<(u128, u128)>,
}

/// Spans longer than this are the umbrella ones - `main`, the whole target -
/// and counting them as work double-counts everything beneath them.
const UMBRELLA_MS: f64 = 300_000.0;

/// Parse an OTLP JSON-lines file into one [`Leg`] per trace.
///
/// Grouped by trace id rather than by timestamp: two earthly invocations
/// overlap in a log and do not overlap in a trace tree, and guessing a
/// boundary from a gap is how the two legs get mixed.
pub fn parse(jsonl: &str) -> Vec<Leg> {
    let mut legs: BTreeMap<String, Leg> = BTreeMap::new();
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        for rs in v["resourceSpans"].as_array().into_iter().flatten() {
            // The leg tag if the run set one, else the trace id stands in.
            let tag = rs["resource"]["attributes"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|a| a["key"] == "rebuck2.leg")
                .and_then(|a| a["value"]["stringValue"].as_str())
                .map(str::to_owned);
            for ss in rs["scopeSpans"].as_array().into_iter().flatten() {
                for sp in ss["spans"].as_array().into_iter().flatten() {
                    let (Some(tid), Some(name)) = (sp["traceId"].as_str(), sp["name"].as_str())
                    else {
                        continue;
                    };
                    let (st, en) = match (
                        sp["startTimeUnixNano"]
                            .as_str()
                            .and_then(|s| s.parse::<u128>().ok()),
                        sp["endTimeUnixNano"]
                            .as_str()
                            .and_then(|s| s.parse::<u128>().ok()),
                    ) {
                        (Some(a), Some(b)) if b >= a => (a, b),
                        _ => continue,
                    };
                    let ms = (en - st) as f64 / 1e6;
                    let leg = legs.entry(tid.to_owned()).or_default();
                    if leg.label.is_empty() {
                        leg.label = tag
                            .clone()
                            .unwrap_or_else(|| tid[..12.min(tid.len())].to_owned());
                    }
                    if name == "main" {
                        leg.wall_ms = leg.wall_ms.max(ms);
                    }
                    // Targets only. earthly names them `+thing`; the rest are
                    // gRPC methods and say nothing about the build's shape.
                    if name.starts_with('+') && ms <= UMBRELLA_MS {
                        let t = leg.targets.entry(name.to_owned()).or_default();
                        t.count += 1;
                        t.total_ms += ms;
                        t.each.push(ms);
                        leg.spans.push((st, en));
                    }
                }
            }
        }
    }
    // A file holds one trace id per nested earthly, and most carry only gRPC
    // spans. Kept, they bury the legs that matter under empty tables and -
    // worse - stop the two-leg comparison firing, because there are then
    // forty legs rather than two.
    let mut out: Vec<Leg> = legs
        .into_values()
        .filter(|l| !l.targets.is_empty())
        .collect();
    out.sort_by(|a, b| b.wall_ms.total_cmp(&a.wall_ms));
    out
}

/// Print each leg's most expensive targets, and what a target cost in each.
pub fn report(legs: &[Leg], top: usize) {
    for leg in legs {
        let spans: u64 = leg.targets.values().map(|t| t.count).sum();
        let total: f64 = leg.targets.values().map(|t| t.total_ms).sum();
        println!(
            "\n== {} : {:.1}s wall, {} target spans, {:.0}s of target time",
            leg.label,
            leg.wall_ms / 1000.0,
            spans,
            total / 1000.0
        );
        let mut rows: Vec<(&String, &Tally)> = leg.targets.iter().collect();
        rows.sort_by(|a, b| b.1.total_ms.total_cmp(&a.1.total_ms));
        println!(
            "{:44} {:>5} {:>9} {:>9} {:>6}",
            "target", "n", "p50 ms", "max s", ">60s"
        );
        for (name, t) in rows.into_iter().take(top) {
            let mut d = t.each.clone();
            d.sort_by(f64::total_cmp);
            println!(
                "{:44} {:5} {:9.0} {:9.1} {:6}",
                &name[..44.min(name.len())],
                t.count,
                d[d.len() / 2],
                d[d.len() - 1] / 1000.0,
                d.iter().filter(|x| **x > 60_000.0).count()
            );
        }
    }
    // The comparison the split exists for: same target, two legs.
    if legs.len() == 2 {
        println!("\n== the same target in both legs (mean seconds)");
        // The ratio column NAMED. Unlabelled it printed 0.1x for a target
        // costing seven times more in the fleet, which reads as the fleet
        // being ten times faster - the same misreading this tool exists to
        // prevent.
        let a0 = &legs[0].label[..8.min(legs[0].label.len())];
        let b0 = &legs[1].label[..8.min(legs[1].label.len())];
        println!(
            "{:44} {:>10} {:>10} {:>11}",
            "target",
            &legs[0].label,
            &legs[1].label,
            format!("{a0}/{b0}")
        );
        let mut names: Vec<&String> = legs[0].targets.keys().collect();
        names.sort_by_key(|n| -(legs[0].targets[*n].total_ms as i64));
        for n in names.into_iter().take(top) {
            let a = &legs[0].targets[n];
            let Some(b) = legs[1].targets.get(n) else {
                continue;
            };
            let (ma, mb) = (a.total_ms / a.count as f64, b.total_ms / b.count as f64);
            println!(
                "{:44} {:10.1} {:10.1} {:10.1}x",
                &n[..44.min(n.len())],
                ma / 1000.0,
                mb / 1000.0,
                if mb > 0.0 { ma / mb } else { 0.0 }
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one OTLP line. `ns` pairs are (start, end) in nanoseconds.
    fn line(trace: &str, leg: Option<&str>, spans: &[(&str, u128, u128)]) -> String {
        let attrs = match leg {
            Some(l) => format!(
                r#"{{"attributes":[{{"key":"rebuck2.leg","value":{{"stringValue":"{l}"}}}}]}}"#
            ),
            None => "{}".to_owned(),
        };
        let s: Vec<String> = spans
            .iter()
            .map(|(n, a, b)| {
                format!(
                    r#"{{"traceId":"{trace}","name":"{n}","startTimeUnixNano":"{a}","endTimeUnixNano":"{b}"}}"#
                )
            })
            .collect();
        format!(
            r#"{{"resourceSpans":[{{"resource":{attrs},"scopeSpans":[{{"spans":[{}]}}]}}]}}"#,
            s.join(",")
        )
    }

    #[test]
    fn two_legs_are_two_legs_and_not_one_pile() {
        // THE bug this exists to prevent. Aggregated, `+base` is 4 spans
        // averaging 6.5s and looks like something to memoise. Split, the
        // baseline does it 3 times at 3s and the fleet once at 20s - the
        // repetition is the baseline's and the price is the fleet's, which
        // is the opposite conclusion and the correct one.
        let ms = 1_000_000u128;
        let jsonl = [
            line(
                "aaaa000000000000",
                Some("baseline"),
                &[
                    ("main", 0, 275_000 * ms),
                    ("+base", 0, 3_000 * ms),
                    ("+base", 0, 3_000 * ms),
                    ("+base", 0, 3_000 * ms),
                ],
            ),
            line(
                "bbbb000000000000",
                Some("fleet"),
                &[("main", 0, 365_000 * ms), ("+base", 0, 20_000 * ms)],
            ),
        ]
        .join("\n");

        let legs = parse(&jsonl);
        assert_eq!(legs.len(), 2, "one leg per trace");
        // Longest wall first, so the slower leg leads the report.
        assert_eq!(legs[0].label, "fleet");
        assert_eq!(legs[1].label, "baseline");

        let fleet = &legs[0].targets["+base"];
        let base = &legs[1].targets["+base"];
        assert_eq!((base.count, fleet.count), (3, 1), "counts do not merge");
        assert!(
            (fleet.total_ms / fleet.count as f64) / (base.total_ms / base.count as f64) > 6.0,
            "the fleet's unit is dearer, which is the finding"
        );
    }

    #[test]
    fn traces_with_no_targets_are_not_legs() {
        // A real file has dozens of trace ids: every nested earthly gets its
        // own, and most carry only gRPC spans. Reported as legs they bury
        // the two that matter under empty tables - and, worse, they stop the
        // two-leg comparison from firing, because there are then 40 legs
        // rather than 2.
        let ms = 1_000_000u128;
        let jsonl = [
            line(
                "aaaa000000000000",
                Some("baseline"),
                &[("main", 0, 100_000 * ms), ("+work", 0, 5_000 * ms)],
            ),
            line(
                "eeee000000000000",
                None,
                &[("moby.buildkit.v1.frontend.LLBBridge/ReadFile", 0, 5 * ms)],
            ),
            line(
                "ffff000000000000",
                None,
                &[("moby.filesync.v1.FileSync/DiffCopy", 0, 9 * ms)],
            ),
        ]
        .join("\n");
        let legs = parse(&jsonl);
        assert_eq!(legs.len(), 1, "only the invocation that built something");
        assert_eq!(legs[0].label, "baseline");
    }

    #[test]
    fn umbrella_spans_do_not_count_as_work() {
        // `+test-no-qemu` wraps the whole build, so counting it as target
        // time counts everything twice and the total exceeds the wall clock
        // by a factor nobody notices.
        let ms = 1_000_000u128;
        let jsonl = line(
            "cccc000000000000",
            Some("fleet"),
            &[
                ("main", 0, 400_000 * ms),
                ("+test-no-qemu", 0, 390_000 * ms), // umbrella
                ("+real-work", 0, 9_000 * ms),
            ],
        );
        let legs = parse(&jsonl);
        assert_eq!(legs[0].targets.len(), 1, "only the real target counts");
        assert_eq!(legs[0].targets["+real-work"].count, 1);
        assert_eq!(legs[0].wall_ms, 400_000.0, "wall still comes from main");
    }

    #[test]
    fn grpc_methods_are_not_targets_and_junk_is_skipped() {
        let ms = 1_000_000u128;
        let jsonl = [
            "not json at all".to_owned(),
            "{}".to_owned(),
            line(
                "dddd000000000000",
                None,
                &[
                    ("moby.buildkit.v1.Control/Solve", 0, 5_000 * ms),
                    ("+thing", 0, 5_000 * ms),
                ],
            ),
        ]
        .join("\n");
        let legs = parse(&jsonl);
        assert_eq!(legs.len(), 1, "junk lines are skipped, not fatal");
        assert_eq!(legs[0].targets.len(), 1);
        assert!(legs[0].targets.contains_key("+thing"));
        // With no leg tag the trace id stands in, so two untagged runs are
        // still told apart.
        assert_eq!(legs[0].label, "dddd00000000");
    }
}
