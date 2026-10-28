//! Planning for ranged reads of pack objects: which byte ranges to request so that
//! requests are few but not wasteful, and big packs still fan out in parallel.

use std::ops::Range;

/// Merge ranges closer than this: an extra request costs more than reading the gap. Packs
/// written one commit at a time hold other files between the wanted ones, so a wide gap
/// downloads a fifth more than needed (1 MiB: +20% on firstlanding, 256 KiB: +10%).
pub const GAP: u64 = 256 << 10;
/// Never plan a single request longer than this (unless one wanted range is), so a large
/// pack is fetched by several parallel requests.
pub const MAX_LEN: u64 = 8 << 20;
/// Bytes of fetched-but-unwritten content allowed in flight.
pub const BYTES_IN_FLIGHT: u64 = 256 << 20;

/// One request and the wanted ranges (indices into the planner's input) it covers.
#[derive(Debug, PartialEq, Eq)]
pub struct Read {
    pub range: Range<u64>,
    pub members: Vec<usize>,
}

/// Group `wanted` (any order, may overlap) into requests: ranges within `gap` of each other
/// share a request while it stays within `max_len`. Requests come out in offset order, every
/// wanted range lies inside exactly one request.
pub fn plan_reads(wanted: &[Range<u64>], gap: u64, max_len: u64) -> Vec<Read> {
    let mut order: Vec<usize> = (0..wanted.len()).collect();
    order.sort_by_key(|&i| (wanted[i].start, wanted[i].end));
    let mut out: Vec<Read> = vec![];
    for i in order {
        let w = &wanted[i];
        if let Some(cur) = out.last_mut() {
            let end = cur.range.end.max(w.end);
            if w.start <= cur.range.end.saturating_add(gap) && end - cur.range.start <= max_len {
                cur.range.end = end;
                cur.members.push(i);
                continue;
            }
        }
        out.push(Read {
            range: w.clone(),
            members: vec![i],
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_close_ranges_and_splits_far_ones() {
        let w = vec![0..10, 20..30, 5000..5010];
        let p = plan_reads(&w, 100, 1 << 20);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].range, 0..30);
        assert_eq!(p[0].members, vec![0, 1]);
        assert_eq!(p[1].range, 5000..5010);
    }

    #[test]
    fn splits_long_runs_at_member_boundaries() {
        let w: Vec<Range<u64>> = (0..10).map(|i| i * 10..i * 10 + 10).collect();
        let p = plan_reads(&w, 0, 35);
        // 30 bytes per request: three members each
        assert_eq!(
            p.iter().map(|r| r.members.len()).collect::<Vec<_>>(),
            [3, 3, 3, 1]
        );
        assert_eq!(p[0].range, 0..30);
        assert_eq!(p[3].range, 90..100);
    }

    #[test]
    fn oversized_member_stands_alone() {
        let w = vec![0..100, 100..110];
        let p = plan_reads(&w, 0, 50);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].range, 0..100);
    }

    #[test]
    fn unsorted_and_overlapping_input() {
        let w = vec![50..60, 0..40, 30..55];
        let p = plan_reads(&w, 0, 1000);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].range, 0..60);
        assert_eq!(p[0].members, vec![1, 2, 0]);
    }

    #[test]
    fn every_wanted_range_is_covered_exactly_once() {
        let mut x = 12345u64;
        let w: Vec<Range<u64>> = (0..500)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let s = (x >> 33) % 100_000_000;
                s..s + 1 + (x >> 20) % 5000
            })
            .collect();
        let p = plan_reads(&w, GAP, MAX_LEN);
        let mut seen = vec![0; w.len()];
        for r in &p {
            for &m in &r.members {
                seen[m] += 1;
                assert!(r.range.start <= w[m].start && w[m].end <= r.range.end);
            }
        }
        assert!(seen.iter().all(|&n| n == 1));
        assert!(p.windows(2).all(|a| a[0].range.start <= a[1].range.start));
    }

    #[test]
    fn empty() {
        assert!(plan_reads(&[], GAP, MAX_LEN).is_empty());
    }
}
