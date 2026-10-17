//! A port of git's Myers core (`xdl_recs_cmp` and `xdl_split` in
//! `xdiff/xdiffi.c`), including its cost heuristics.
//!
//! imara-diff's Myers is close but not identical: it prunes its input again
//! and scores its "good snake" heuristic differently, so on large, repetitive
//! files (lock files, generated code) it can return a longer edit script
//! than git. Running git's exact algorithm on git's preprocessed lines keeps
//! the +/- counts equal to `git diff --numstat`.

use imara_diff::Token;

/// git: `XDL_MAX_COST_MIN`.
const MAX_COST_MIN: i64 = 256;
/// git: `XDL_HEUR_MIN_COST`.
const HEUR_MIN_COST: i64 = 256;
/// git: `XDL_SNAKE_CNT`.
const SNAKE_CNT: i64 = 20;
/// git: `XDL_K_HEUR`.
const K_HEUR: i64 = 4;
/// git: `XDL_LINE_MAX`, larger than any line index.
const LINE_MAX: i64 = i64::MAX / 4;

/// git: `xdpsplit_t`.
struct Split {
    i1: i64,
    i2: i64,
    min_lo: bool,
    min_hi: bool,
}

/// Forward and backward k-vectors, indexed by diagonal.
struct Env<'a> {
    ha1: &'a [Token],
    ha2: &'a [Token],
    kv: Vec<i64>,
    /// Index in `kv` of forward diagonal 0.
    fwd: i64,
    /// Index in `kv` of backward diagonal 0.
    bwd: i64,
    max_cost: i64,
}

impl Env<'_> {
    fn f(&self, d: i64) -> i64 {
        self.kv[(self.fwd + d) as usize]
    }
    fn set_f(&mut self, d: i64, value: i64) {
        self.kv[(self.fwd + d) as usize] = value;
    }
    fn b(&self, d: i64) -> i64 {
        self.kv[(self.bwd + d) as usize]
    }
    fn set_b(&mut self, d: i64, value: i64) {
        self.kv[(self.bwd + d) as usize] = value;
    }
    fn eq(&self, i1: i64, i2: i64) -> bool {
        self.ha1[i1 as usize] == self.ha2[i2 as usize]
    }
}

/// Diffs two (already preprocessed) token sequences with git's Myers
/// (`need_min = false`) and marks the changed tokens of each side.
pub(crate) fn diff(ha1: &[Token], ha2: &[Token], changed1: &mut [bool], changed2: &mut [bool]) {
    let (n1, n2) = (ha1.len() as i64, ha2.len() as i64);
    let ndiags = n1 + n2 + 3;
    let mut env = Env {
        ha1,
        ha2,
        kv: vec![0; (2 * ndiags + 2) as usize],
        fwd: n2 + 1,
        bwd: ndiags + n2 + 1,
        max_cost: bogo_sqrt(ndiags).max(MAX_COST_MIN),
    };

    // git recurses; an explicit stack avoids deep recursion on huge files.
    // Boxes are independent, so the order they are solved in does not
    // matter.
    let mut stack = vec![(0, n1, 0, n2, false)];
    while let Some((mut off1, mut lim1, mut off2, mut lim2, need_min)) = stack.pop() {
        while off1 < lim1 && off2 < lim2 && env.eq(off1, off2) {
            off1 += 1;
            off2 += 1;
        }
        while off1 < lim1 && off2 < lim2 && env.eq(lim1 - 1, lim2 - 1) {
            lim1 -= 1;
            lim2 -= 1;
        }
        if off1 == lim1 {
            changed2[off2 as usize..lim2 as usize].fill(true);
        } else if off2 == lim2 {
            changed1[off1 as usize..lim1 as usize].fill(true);
        } else {
            let split = split(&mut env, off1, lim1, off2, lim2, need_min);
            stack.push((split.i1, lim1, split.i2, lim2, split.min_hi));
            stack.push((off1, split.i1, off2, split.i2, split.min_lo));
        }
    }
}

/// git: `xdl_bogosqrt`.
fn bogo_sqrt(mut n: i64) -> i64 {
    let mut root = 1;
    while n > 0 {
        root <<= 1;
        n >>= 2;
    }
    root
}

/// git: `xdl_split`. Finds the middle snake of the box, or a heuristic
/// split point when the search gets too expensive.
fn split(env: &mut Env, off1: i64, lim1: i64, off2: i64, lim2: i64, need_min: bool) -> Split {
    let (dmin, dmax) = (off1 - lim2, lim1 - off2);
    let (fmid, bmid) = (off1 - off2, lim1 - lim2);
    let odd = (fmid - bmid) & 1 != 0;
    let (mut fmin, mut fmax) = (fmid, fmid);
    let (mut bmin, mut bmax) = (bmid, bmid);

    env.set_f(fmid, off1);
    env.set_b(bmid, lim1);

    let mut ec = 1i64;
    loop {
        let mut got_snake = false;

        if fmin > dmin {
            fmin -= 1;
            env.set_f(fmin - 1, -1);
        } else {
            fmin += 1;
        }
        if fmax < dmax {
            fmax += 1;
            env.set_f(fmax + 1, -1);
        } else {
            fmax -= 1;
        }

        let mut d = fmax;
        while d >= fmin {
            let mut i1 = if env.f(d - 1) >= env.f(d + 1) {
                env.f(d - 1) + 1
            } else {
                env.f(d + 1)
            };
            let prev1 = i1;
            let mut i2 = i1 - d;
            while i1 < lim1 && i2 < lim2 && env.eq(i1, i2) {
                i1 += 1;
                i2 += 1;
            }
            if i1 - prev1 > SNAKE_CNT {
                got_snake = true;
            }
            env.set_f(d, i1);
            if odd && bmin <= d && d <= bmax && env.b(d) <= i1 {
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
            env.set_b(bmin - 1, LINE_MAX);
        } else {
            bmin += 1;
        }
        if bmax < dmax {
            bmax += 1;
            env.set_b(bmax + 1, LINE_MAX);
        } else {
            bmax -= 1;
        }

        let mut d = bmax;
        while d >= bmin {
            let mut i1 = if env.b(d - 1) < env.b(d + 1) {
                env.b(d - 1)
            } else {
                env.b(d + 1) - 1
            };
            let prev1 = i1;
            let mut i2 = i1 - d;
            while i1 > off1 && i2 > off2 && env.eq(i1 - 1, i2 - 1) {
                i1 -= 1;
                i2 -= 1;
            }
            if prev1 - i1 > SNAKE_CNT {
                got_snake = true;
            }
            env.set_b(d, i1);
            if !odd && fmin <= d && d <= fmax && i1 <= env.f(d) {
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

        // Past the trigger cost, take a diagonal that reached far into the
        // box and ends in a long enough snake.
        if got_snake && ec > HEUR_MIN_COST {
            let mut best = 0;
            let mut found = (0, 0);
            let mut d = fmax;
            while d >= fmin {
                let dd = (d - fmid).abs();
                let i1 = env.f(d);
                let i2 = i1 - d;
                let v = (i1 - off1) + (i2 - off2) - dd;
                if v > K_HEUR * ec
                    && v > best
                    && off1 + SNAKE_CNT <= i1
                    && i1 < lim1
                    && off2 + SNAKE_CNT <= i2
                    && i2 < lim2
                    && (1..=SNAKE_CNT).all(|k| env.eq(i1 - k, i2 - k))
                {
                    best = v;
                    found = (i1, i2);
                }
                d -= 2;
            }
            if best > 0 {
                return Split {
                    i1: found.0,
                    i2: found.1,
                    min_lo: true,
                    min_hi: false,
                };
            }

            let mut d = bmax;
            while d >= bmin {
                let dd = (d - bmid).abs();
                let i1 = env.b(d);
                let i2 = i1 - d;
                let v = (lim1 - i1) + (lim2 - i2) - dd;
                if v > K_HEUR * ec
                    && v > best
                    && off1 < i1
                    && i1 <= lim1 - SNAKE_CNT
                    && off2 < i2
                    && i2 <= lim2 - SNAKE_CNT
                    && (0..SNAKE_CNT).all(|k| env.eq(i1 + k, i2 + k))
                {
                    best = v;
                    found = (i1, i2);
                }
                d -= 2;
            }
            if best > 0 {
                return Split {
                    i1: found.0,
                    i2: found.1,
                    min_lo: false,
                    min_hi: true,
                };
            }
        }

        // Too expensive: split at the furthest-reaching path.
        if ec >= env.max_cost {
            let (mut fbest, mut fbest1) = (-1, -1);
            let mut d = fmax;
            while d >= fmin {
                let mut i1 = env.f(d).min(lim1);
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

            let (mut bbest, mut bbest1) = (LINE_MAX, LINE_MAX);
            let mut d = bmax;
            while d >= bmin {
                let mut i1 = env.b(d).max(off1);
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

            return if (lim1 + lim2) - bbest < fbest - (off1 + off2) {
                Split {
                    i1: fbest1,
                    i2: fbest - fbest1,
                    min_lo: true,
                    min_hi: false,
                }
            } else {
                Split {
                    i1: bbest1,
                    i2: bbest - bbest1,
                    min_lo: false,
                    min_hi: true,
                }
            };
        }
        ec += 1;
    }
}
