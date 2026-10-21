//! A port of Git's xdiff Myers implementation (xprepare.c, xdiffi.c, xemit.c) so line diffs,
//! diffstats and patches come out exactly as `git diff` prints them: same edit script, same
//! slider placement (indent heuristic), same hunks and function-name headers.

use std::collections::HashMap;

/// One change: lines `[i1, i1+chg1)` of the old file replaced by `[i2, i2+chg2)` of the new.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub i1: usize,
    pub i2: usize,
    pub chg1: usize,
    pub chg2: usize,
}

const XDL_MAX_COST_MIN: i64 = 256;
const XDL_HEUR_MIN_COST: i64 = 256;
const XDL_SNAKE_CNT: i64 = 20;
const XDL_K_HEUR: i64 = 4;
const XDL_MAX_EQLIMIT: i64 = 1024;
const XDL_SIMSCAN_WINDOW: i64 = 100;
const XDL_KPDIS_RUN: i64 = 4;
const XDL_LINE_MAX: i64 = i64::MAX;

/// Split into records (lines including their '\n'; the last may lack one).
pub fn records(d: &[u8]) -> Vec<&[u8]> {
    let mut out = vec![];
    let mut s = 0;
    for (i, &b) in d.iter().enumerate() {
        if b == b'\n' {
            out.push(&d[s..=i]);
            s = i + 1;
        }
    }
    if s < d.len() {
        out.push(&d[s..]);
    }
    out
}

fn bogosqrt(mut n: i64) -> i64 {
    let mut i = 1;
    while n > 0 {
        i <<= 1;
        n >>= 2;
    }
    i
}

struct XdFile<'a> {
    recs: Vec<&'a [u8]>,
    /// Class of each record (equal lines share a class).
    class: Vec<usize>,
    /// Changed flags with a sentinel on each side: rchg[i + 1] is record i.
    rchg: Vec<u8>,
    rindex: Vec<usize>,
    ha: Vec<usize>,
    dstart: i64,
    dend: i64,
}

impl XdFile<'_> {
    fn nrec(&self) -> i64 {
        self.recs.len() as i64
    }
    fn chg(&self, i: i64) -> bool {
        self.rchg[(i + 1) as usize] != 0
    }
    fn set(&mut self, i: i64, v: u8) {
        self.rchg[(i + 1) as usize] = v;
    }
}

struct Split {
    i1: i64,
    i2: i64,
    min_lo: bool,
    min_hi: bool,
}

struct Env {
    mxcost: i64,
}

/// Divide and conquer: find a middle snake (xdl_split).
#[allow(clippy::too_many_arguments)]
fn split(
    ha1: &[usize],
    off1: i64,
    lim1: i64,
    ha2: &[usize],
    off2: i64,
    lim2: i64,
    kvdf: &mut [i64],
    kvdb: &mut [i64],
    base: i64,
    need_min: bool,
    env: &Env,
) -> Split {
    let dmin = off1 - lim2;
    let dmax = lim1 - off2;
    let fmid = off1 - off2;
    let bmid = lim1 - lim2;
    let odd = (fmid - bmid) & 1 != 0;
    let (mut fmin, mut fmax) = (fmid, fmid);
    let (mut bmin, mut bmax) = (bmid, bmid);
    let kf = |d: i64| (d + base) as usize;
    let h1 = |i: i64| ha1[i as usize];
    let h2 = |i: i64| ha2[i as usize];
    kvdf[kf(fmid)] = off1;
    kvdb[kf(bmid)] = lim1;
    let mut ec: i64 = 1;
    loop {
        let mut got_snake = false;
        if fmin > dmin {
            fmin -= 1;
            kvdf[kf(fmin - 1)] = -1;
        } else {
            fmin += 1;
        }
        if fmax < dmax {
            fmax += 1;
            kvdf[kf(fmax + 1)] = -1;
        } else {
            fmax -= 1;
        }
        let mut d = fmax;
        while d >= fmin {
            let mut i1 = if kvdf[kf(d - 1)] >= kvdf[kf(d + 1)] {
                kvdf[kf(d - 1)] + 1
            } else {
                kvdf[kf(d + 1)]
            };
            let prev1 = i1;
            let mut i2 = i1 - d;
            while i1 < lim1 && i2 < lim2 && h1(i1) == h2(i2) {
                i1 += 1;
                i2 += 1;
            }
            if i1 - prev1 > XDL_SNAKE_CNT {
                got_snake = true;
            }
            kvdf[kf(d)] = i1;
            if odd && bmin <= d && d <= bmax && kvdb[kf(d)] <= i1 {
                return Split {
                    i1,
                    i2,
                    min_lo: true,
                    min_hi: true,
                };
            }
            d -= 2;
        }
        if bmin > dmin {
            bmin -= 1;
            kvdb[kf(bmin - 1)] = XDL_LINE_MAX;
        } else {
            bmin += 1;
        }
        if bmax < dmax {
            bmax += 1;
            kvdb[kf(bmax + 1)] = XDL_LINE_MAX;
        } else {
            bmax -= 1;
        }
        let mut d = bmax;
        while d >= bmin {
            let mut i1 = if kvdb[kf(d - 1)] < kvdb[kf(d + 1)] {
                kvdb[kf(d - 1)]
            } else {
                kvdb[kf(d + 1)] - 1
            };
            let prev1 = i1;
            let mut i2 = i1 - d;
            while i1 > off1 && i2 > off2 && h1(i1 - 1) == h2(i2 - 1) {
                i1 -= 1;
                i2 -= 1;
            }
            if prev1 - i1 > XDL_SNAKE_CNT {
                got_snake = true;
            }
            kvdb[kf(d)] = i1;
            if !odd && fmin <= d && d <= fmax && i1 <= kvdf[kf(d)] {
                return Split {
                    i1,
                    i2,
                    min_lo: true,
                    min_hi: true,
                };
            }
            d -= 2;
        }
        if need_min {
            ec += 1;
            continue;
        }
        if got_snake && ec > XDL_HEUR_MIN_COST {
            let mut best = 0;
            let mut sp = (0, 0);
            let mut d = fmax;
            while d >= fmin {
                let dd = if d > fmid { d - fmid } else { fmid - d };
                let i1 = kvdf[kf(d)];
                let i2 = i1 - d;
                let v = (i1 - off1) + (i2 - off2) - dd;
                if v > XDL_K_HEUR * ec
                    && v > best
                    && off1 + XDL_SNAKE_CNT <= i1
                    && i1 < lim1
                    && off2 + XDL_SNAKE_CNT <= i2
                    && i2 < lim2
                {
                    let mut k = 1;
                    while h1(i1 - k) == h2(i2 - k) {
                        if k == XDL_SNAKE_CNT {
                            best = v;
                            sp = (i1, i2);
                            break;
                        }
                        k += 1;
                    }
                }
                d -= 2;
            }
            if best > 0 {
                return Split {
                    i1: sp.0,
                    i2: sp.1,
                    min_lo: true,
                    min_hi: false,
                };
            }
            let mut best = 0;
            let mut d = bmax;
            while d >= bmin {
                let dd = if d > bmid { d - bmid } else { bmid - d };
                let i1 = kvdb[kf(d)];
                let i2 = i1 - d;
                let v = (lim1 - i1) + (lim2 - i2) - dd;
                if v > XDL_K_HEUR * ec
                    && v > best
                    && off1 < i1
                    && i1 <= lim1 - XDL_SNAKE_CNT
                    && off2 < i2
                    && i2 <= lim2 - XDL_SNAKE_CNT
                {
                    let mut k = 0;
                    while h1(i1 + k) == h2(i2 + k) {
                        if k == XDL_SNAKE_CNT - 1 {
                            best = v;
                            sp = (i1, i2);
                            break;
                        }
                        k += 1;
                    }
                }
                d -= 2;
            }
            if best > 0 {
                return Split {
                    i1: sp.0,
                    i2: sp.1,
                    min_lo: false,
                    min_hi: true,
                };
            }
        }
        if ec >= env.mxcost {
            let (mut fbest, mut fbest1) = (-1i64, -1i64);
            let mut d = fmax;
            while d >= fmin {
                let mut i1 = kvdf[kf(d)].min(lim1);
                let mut i2 = i1 - d;
                if lim2 < i2 {
                    i1 = lim2 + d;
                    i2 = lim2;
                }
                if fbest < i1 + i2 {
                    fbest = i1 + i2;
                    fbest1 = i1;
                }
                d -= 2;
            }
            let (mut bbest, mut bbest1) = (XDL_LINE_MAX, XDL_LINE_MAX);
            let mut d = bmax;
            while d >= bmin {
                let mut i1 = off1.max(kvdb[kf(d)]);
                let mut i2 = i1 - d;
                if i2 < off2 {
                    i1 = off2 + d;
                    i2 = off2;
                }
                if i1 + i2 < bbest {
                    bbest = i1 + i2;
                    bbest1 = i1;
                }
                d -= 2;
            }
            if (lim1 + lim2) - bbest < fbest - (off1 + off2) {
                return Split {
                    i1: fbest1,
                    i2: fbest - fbest1,
                    min_lo: true,
                    min_hi: false,
                };
            }
            return Split {
                i1: bbest1,
                i2: bbest - bbest1,
                min_lo: false,
                min_hi: true,
            };
        }
        ec += 1;
    }
}

/// xdl_recs_cmp, iterative over an explicit stack.
fn recs_cmp(
    f1: &mut XdFile,
    f2: &mut XdFile,
    kvdf: &mut [i64],
    kvdb: &mut [i64],
    base: i64,
    env: &Env,
) {
    let ha1 = std::mem::take(&mut f1.ha);
    let ha2 = std::mem::take(&mut f2.ha);
    let mut stack = vec![(0i64, ha1.len() as i64, 0i64, ha2.len() as i64, false)];
    while let Some((mut off1, mut lim1, mut off2, mut lim2, need_min)) = stack.pop() {
        while off1 < lim1 && off2 < lim2 && ha1[off1 as usize] == ha2[off2 as usize] {
            off1 += 1;
            off2 += 1;
        }
        while off1 < lim1 && off2 < lim2 && ha1[(lim1 - 1) as usize] == ha2[(lim2 - 1) as usize] {
            lim1 -= 1;
            lim2 -= 1;
        }
        if off1 == lim1 {
            for i in off2..lim2 {
                let r = f2.rindex[i as usize] as i64;
                f2.set(r, 1);
            }
        } else if off2 == lim2 {
            for i in off1..lim1 {
                let r = f1.rindex[i as usize] as i64;
                f1.set(r, 1);
            }
        } else {
            let spl = split(
                &ha1, off1, lim1, &ha2, off2, lim2, kvdf, kvdb, base, need_min, env,
            );
            // Push the high half first so the low half is processed first, as recursion would.
            stack.push((spl.i1, lim1, spl.i2, lim2, spl.min_hi));
            stack.push((off1, spl.i1, off2, spl.i2, spl.min_lo));
        }
    }
    f1.ha = ha1;
    f2.ha = ha2;
}

fn clean_mmatch(dis: &[u8], i: i64, mut s: i64, mut e: i64) -> bool {
    if i - s > XDL_SIMSCAN_WINDOW {
        s = i - XDL_SIMSCAN_WINDOW;
    }
    if e - i > XDL_SIMSCAN_WINDOW {
        e = i + XDL_SIMSCAN_WINDOW;
    }
    let (mut rdis0, mut rpdis0) = (0i64, 1i64);
    let mut r = 1;
    while i - r >= s {
        match dis[(i - r) as usize] {
            0 => rdis0 += 1,
            2 => rpdis0 += 1,
            _ => break,
        }
        r += 1;
    }
    if rdis0 == 0 {
        return false;
    }
    let (mut rdis1, mut rpdis1) = (0i64, 1i64);
    let mut r = 1;
    while i + r <= e {
        match dis[(i + r) as usize] {
            0 => rdis1 += 1,
            2 => rpdis1 += 1,
            _ => break,
        }
        r += 1;
    }
    if rdis1 == 0 {
        return false;
    }
    rdis1 += rdis0;
    rpdis1 += rpdis0;
    rpdis1 * XDL_KPDIS_RUN < rpdis1 + rdis1
}

fn prepare<'a>(a: &'a [u8], b: &'a [u8]) -> (XdFile<'a>, XdFile<'a>) {
    let ra = records(a);
    let rb = records(b);
    let mut classes: HashMap<&[u8], usize> = HashMap::new();
    let mut len1: Vec<i64> = vec![];
    let mut len2: Vec<i64> = vec![];
    let mut classify = |r: &'a [u8], side: u8, len1: &mut Vec<i64>, len2: &mut Vec<i64>| {
        let n = classes.len();
        let c = *classes.entry(r).or_insert(n);
        if c == len1.len() {
            len1.push(0);
            len2.push(0);
        }
        if side == 1 {
            len1[c] += 1
        } else {
            len2[c] += 1
        }
        c
    };
    let ca: Vec<usize> = ra
        .iter()
        .map(|r| classify(r, 1, &mut len1, &mut len2))
        .collect();
    let cb: Vec<usize> = rb
        .iter()
        .map(|r| classify(r, 2, &mut len1, &mut len2))
        .collect();
    let mk = |recs: Vec<&'a [u8]>, class: Vec<usize>| {
        let n = recs.len();
        XdFile {
            recs,
            class,
            rchg: vec![0; n + 2],
            rindex: vec![],
            ha: vec![],
            dstart: 0,
            dend: 0,
        }
    };
    let mut f1 = mk(ra, ca);
    let mut f2 = mk(rb, cb);
    // xdl_trim_ends
    let lim = f1.recs.len().min(f2.recs.len());
    let mut i = 0;
    while i < lim && f1.class[i] == f2.class[i] {
        i += 1;
    }
    f1.dstart = i as i64;
    f2.dstart = i as i64;
    let lim2 = lim - i;
    let mut j = 0;
    while j < lim2 && f1.class[f1.recs.len() - 1 - j] == f2.class[f2.recs.len() - 1 - j] {
        j += 1;
    }
    f1.dend = f1.nrec() - j as i64 - 1;
    f2.dend = f2.nrec() - j as i64 - 1;
    // xdl_cleanup_records
    let mut dis1 = vec![0u8; f1.recs.len() + 1];
    let mut dis2 = vec![0u8; f2.recs.len() + 1];
    let mlim = bogosqrt(f1.nrec()).min(XDL_MAX_EQLIMIT);
    for i in f1.dstart..=f1.dend {
        let nm = len2[f1.class[i as usize]];
        dis1[i as usize] = if nm == 0 {
            0
        } else if nm >= mlim {
            2
        } else {
            1
        };
    }
    let mlim = bogosqrt(f2.nrec()).min(XDL_MAX_EQLIMIT);
    for i in f2.dstart..=f2.dend {
        let nm = len1[f2.class[i as usize]];
        dis2[i as usize] = if nm == 0 {
            0
        } else if nm >= mlim {
            2
        } else {
            1
        };
    }
    for (f, dis) in [(&mut f1, &dis1), (&mut f2, &dis2)] {
        for i in f.dstart..=f.dend {
            let d = dis[i as usize];
            if d == 1 || (d == 2 && !clean_mmatch(dis, i, f.dstart, f.dend)) {
                f.rindex.push(i as usize);
                f.ha.push(f.class[i as usize]);
            } else {
                f.set(i, 1);
            }
        }
    }
    (f1, f2)
}

// ---- xdl_change_compact (slider placement with the indent heuristic) ----

const MAX_INDENT: i32 = 200;
const MAX_BLANKS: i32 = 20;
const START_OF_FILE_PENALTY: i32 = 1;
const END_OF_FILE_PENALTY: i32 = 21;
const TOTAL_BLANK_WEIGHT: i32 = -30;
const POST_BLANK_WEIGHT: i32 = 6;
const RELATIVE_INDENT_PENALTY: i32 = -4;
const RELATIVE_INDENT_WITH_BLANK_PENALTY: i32 = 10;
const RELATIVE_OUTDENT_PENALTY: i32 = 24;
const RELATIVE_OUTDENT_WITH_BLANK_PENALTY: i32 = 17;
const RELATIVE_DEDENT_PENALTY: i32 = 23;
const RELATIVE_DEDENT_WITH_BLANK_PENALTY: i32 = 17;
const INDENT_WEIGHT: i32 = 60;
const INDENT_HEURISTIC_MAX_SLIDING: i64 = 100;

fn get_indent(rec: &[u8]) -> i32 {
    let mut ret = 0;
    for &c in rec {
        if !matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
            return ret;
        } else if c == b' ' {
            ret += 1;
        } else if c == b'\t' {
            ret += 8 - ret % 8;
        }
        if ret >= MAX_INDENT {
            return MAX_INDENT;
        }
    }
    -1
}

#[derive(Default)]
struct Measure {
    end_of_file: bool,
    indent: i32,
    pre_blank: i32,
    pre_indent: i32,
    post_blank: i32,
    post_indent: i32,
}

fn measure_split(f: &XdFile, split: i64) -> Measure {
    let mut m = Measure::default();
    if split >= f.nrec() {
        m.end_of_file = true;
        m.indent = -1;
    } else {
        m.indent = get_indent(f.recs[split as usize]);
    }
    m.pre_indent = -1;
    let mut i = split - 1;
    while i >= 0 {
        m.pre_indent = get_indent(f.recs[i as usize]);
        if m.pre_indent != -1 {
            break;
        }
        m.pre_blank += 1;
        if m.pre_blank == MAX_BLANKS {
            m.pre_indent = 0;
            break;
        }
        i -= 1;
    }
    m.post_indent = -1;
    let mut i = split + 1;
    while i < f.nrec() {
        m.post_indent = get_indent(f.recs[i as usize]);
        if m.post_indent != -1 {
            break;
        }
        m.post_blank += 1;
        if m.post_blank == MAX_BLANKS {
            m.post_indent = 0;
            break;
        }
        i += 1;
    }
    m
}

#[derive(Default, Clone, Copy)]
struct Score {
    effective_indent: i32,
    penalty: i32,
}

fn score_add_split(m: &Measure, s: &mut Score) {
    if m.pre_indent == -1 && m.pre_blank == 0 {
        s.penalty += START_OF_FILE_PENALTY;
    }
    if m.end_of_file {
        s.penalty += END_OF_FILE_PENALTY;
    }
    let post_blank = if m.indent == -1 { 1 + m.post_blank } else { 0 };
    let total_blank = m.pre_blank + post_blank;
    s.penalty += TOTAL_BLANK_WEIGHT * total_blank;
    s.penalty += POST_BLANK_WEIGHT * post_blank;
    let indent = if m.indent != -1 {
        m.indent
    } else {
        m.post_indent
    };
    let any_blanks = total_blank != 0;
    s.effective_indent += indent;
    if indent == -1 || m.pre_indent == -1 {
    } else if indent > m.pre_indent {
        s.penalty += if any_blanks {
            RELATIVE_INDENT_WITH_BLANK_PENALTY
        } else {
            RELATIVE_INDENT_PENALTY
        };
    } else if indent == m.pre_indent {
    } else if m.post_indent != -1 && m.post_indent > indent {
        s.penalty += if any_blanks {
            RELATIVE_OUTDENT_WITH_BLANK_PENALTY
        } else {
            RELATIVE_OUTDENT_PENALTY
        };
    } else {
        s.penalty += if any_blanks {
            RELATIVE_DEDENT_WITH_BLANK_PENALTY
        } else {
            RELATIVE_DEDENT_PENALTY
        };
    }
}

fn score_cmp(a: &Score, b: &Score) -> i32 {
    let ci = (a.effective_indent > b.effective_indent) as i32
        - (a.effective_indent < b.effective_indent) as i32;
    INDENT_WEIGHT * ci + (a.penalty - b.penalty)
}

#[derive(Clone, Copy)]
struct Group {
    start: i64,
    end: i64,
}

fn group_init(f: &XdFile) -> Group {
    let mut g = Group { start: 0, end: 0 };
    while f.chg(g.end) {
        g.end += 1;
    }
    g
}

fn group_next(f: &XdFile, g: &mut Group) -> bool {
    if g.end == f.nrec() {
        return false;
    }
    g.start = g.end + 1;
    g.end = g.start;
    while f.chg(g.end) {
        g.end += 1;
    }
    true
}

fn group_previous(f: &XdFile, g: &mut Group) -> bool {
    if g.start == 0 {
        return false;
    }
    g.end = g.start - 1;
    g.start = g.end;
    while f.chg(g.start - 1) {
        g.start -= 1;
    }
    true
}

fn group_slide_down(f: &mut XdFile, g: &mut Group) -> bool {
    if g.end < f.nrec() && f.class[g.start as usize] == f.class[g.end as usize] {
        f.set(g.start, 0);
        g.start += 1;
        f.set(g.end, 1);
        g.end += 1;
        while f.chg(g.end) {
            g.end += 1;
        }
        true
    } else {
        false
    }
}

fn group_slide_up(f: &mut XdFile, g: &mut Group) -> bool {
    if g.start > 0 && f.class[(g.start - 1) as usize] == f.class[(g.end - 1) as usize] {
        g.start -= 1;
        f.set(g.start, 1);
        g.end -= 1;
        f.set(g.end, 0);
        while f.chg(g.start - 1) {
            g.start -= 1;
        }
        true
    } else {
        false
    }
}

fn change_compact(f: &mut XdFile, fo: &XdFile) {
    let mut g = group_init(f);
    let mut go = group_init(fo);
    loop {
        if g.end != g.start {
            let mut earliest_end;
            let mut end_matching_other;
            loop {
                let groupsize = g.end - g.start;
                end_matching_other = -1;
                while group_slide_up(f, &mut g) {
                    assert!(group_previous(fo, &mut go), "group sync broken sliding up");
                }
                earliest_end = g.end;
                if go.end > go.start {
                    end_matching_other = g.end;
                }
                while group_slide_down(f, &mut g) {
                    assert!(group_next(fo, &mut go), "group sync broken sliding down");
                    if go.end > go.start {
                        end_matching_other = g.end;
                    }
                }
                if groupsize == g.end - g.start {
                    break;
                }
            }
            let groupsize = g.end - g.start;
            if g.end == earliest_end {
                // No shifting was possible.
            } else if end_matching_other != -1 {
                while go.end == go.start {
                    assert!(group_slide_up(f, &mut g), "match disappeared");
                    assert!(
                        group_previous(fo, &mut go),
                        "group sync broken sliding to match"
                    );
                }
            } else {
                let mut shift = earliest_end;
                if g.end - groupsize - 1 > shift {
                    shift = g.end - groupsize - 1;
                }
                if g.end - INDENT_HEURISTIC_MAX_SLIDING > shift {
                    shift = g.end - INDENT_HEURISTIC_MAX_SLIDING;
                }
                let mut best_shift = -1;
                let mut best = Score::default();
                while shift <= g.end {
                    let mut score = Score::default();
                    score_add_split(&measure_split(f, shift), &mut score);
                    score_add_split(&measure_split(f, shift - groupsize), &mut score);
                    if best_shift == -1 || score_cmp(&score, &best) <= 0 {
                        best = score;
                        best_shift = shift;
                    }
                    shift += 1;
                }
                while g.end > best_shift {
                    assert!(group_slide_up(f, &mut g), "best shift unreached");
                    assert!(
                        group_previous(fo, &mut go),
                        "group sync broken sliding to blank line"
                    );
                }
            }
        }
        if !group_next(f, &mut g) {
            break;
        }
        assert!(
            group_next(fo, &mut go),
            "group sync broken moving to next group"
        );
    }
}

/// The edit script Git computes for two files (default Myers, indent heuristic on).
pub fn diff<'a>(a: &'a [u8], b: &'a [u8]) -> (Vec<&'a [u8]>, Vec<&'a [u8]>, Vec<Change>) {
    let (mut f1, mut f2) = prepare(a, b);
    let n1 = f1.ha.len() as i64;
    let n2 = f2.ha.len() as i64;
    let ndiags = n1 + n2 + 3;
    let size = (2 * ndiags + 2) as usize;
    let mut kvdf = vec![0i64; size];
    let mut kvdb = vec![0i64; size];
    let env = Env {
        mxcost: bogosqrt(ndiags).max(XDL_MAX_COST_MIN),
    };
    recs_cmp(&mut f1, &mut f2, &mut kvdf, &mut kvdb, n2 + 1, &env);
    change_compact(&mut f1, &f2);
    change_compact(&mut f2, &f1);
    // xdl_build_script
    let mut script = vec![];
    let (mut i1, mut i2) = (f1.nrec(), f2.nrec());
    while i1 >= 0 || i2 >= 0 {
        if f1.chg(i1 - 1) || f2.chg(i2 - 1) {
            let (l1, l2) = (i1, i2);
            while f1.chg(i1 - 1) {
                i1 -= 1;
            }
            while f2.chg(i2 - 1) {
                i2 -= 1;
            }
            script.push(Change {
                i1: i1 as usize,
                i2: i2 as usize,
                chg1: (l1 - i1) as usize,
                chg2: (l2 - i2) as usize,
            });
        }
        i1 -= 1;
        i2 -= 1;
    }
    script.reverse();
    (f1.recs, f2.recs, script)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple() {
        let (_, _, s) = diff(b"a\nb\nc\n", b"a\nx\nc\n");
        assert_eq!(
            s,
            vec![Change {
                i1: 1,
                i2: 1,
                chg1: 1,
                chg2: 1
            }]
        );
        let (_, _, s) = diff(b"", b"a\n");
        assert_eq!(
            s,
            vec![Change {
                i1: 0,
                i2: 0,
                chg1: 0,
                chg2: 1
            }]
        );
    }
}
