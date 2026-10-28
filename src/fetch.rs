//! Planning for ranged reads of pack objects: which byte ranges to request so that
//! requests are few but not wasteful, and big packs still fan out in parallel.

use std::ops::Range;

/// Merge ranges closer than this. A gap is worth reading when it takes less time than the
/// request it saves; with hundreds of requests in flight that request is nearly free (latency
/// overlaps) and the gap's bytes are not (bandwidth is shared), so the break-even is small:
/// round-trip time x per-stream throughput is ~100 ms x ~1 MB/s once the link is saturated.
/// Packs written one commit at a time hold other files between the wanted ones, so a wide gap
/// downloads much more than needed (firstlanding, whole tree: 1 MiB +20% bytes, 256 KiB +10%,
/// 16 KiB +5%). `PAIL_FETCH_GAP` (bytes) overrides.
pub const GAP: u64 = 64 << 10;

/// The merge gap in use.
pub fn gap() -> u64 {
    std::env::var("PAIL_FETCH_GAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(GAP)
}

/// How long one ranged read of `len` bytes may take before it is abandoned and asked for
/// again: a request that stalls (seen on GCS with large ranges) must not hold up the whole
/// checkout. Generous: a healthy 8 MiB read takes a couple of seconds.
pub fn stall_after(len: u64) -> std::time::Duration {
    std::time::Duration::from_secs(8 + len / (2 << 20))
}
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
    fn stall_timeout_grows_with_length() {
        assert!(stall_after(8 << 20) > stall_after(1 << 10));
        assert!(stall_after(0) >= std::time::Duration::from_secs(8));
    }

    #[test]
    fn narrower_gaps_fetch_fewer_bytes_with_more_requests() {
        // 100 wanted 1 KiB ranges, 100 KiB apart
        let w: Vec<Range<u64>> = (0..100).map(|i| i * 102_400..i * 102_400 + 1024).collect();
        let bytes = |g| {
            plan_reads(&w, g, MAX_LEN)
                .iter()
                .map(|r| r.range.end - r.range.start)
                .sum::<u64>()
        };
        let reqs = |g| plan_reads(&w, g, MAX_LEN).len();
        assert!(bytes(0) < bytes(1 << 20));
        assert!(reqs(0) > reqs(1 << 20));
        assert_eq!(reqs(0), 100);
    }

    #[test]
    fn empty() {
        assert!(plan_reads(&[], GAP, MAX_LEN).is_empty());
    }
}
