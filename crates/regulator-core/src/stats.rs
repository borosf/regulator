// SPDX-License-Identifier: GPL-2.0
//! Order statistics helpers ported from tg's algo.c: quickselect with a
//! median-of-three pivot, trimmed means, and running max.

/// Max of `v[a..b]` (exclusive end), also returning the argmax index.
pub fn vmax(v: &[f32], a: usize, b: usize) -> (f32, usize) {
    let mut max = v[a];
    let mut i_max = a;
    for (i, &x) in v.iter().enumerate().take(b).skip(a + 1) {
        if x > max {
            max = x;
            i_max = i;
        }
    }
    (max, i_max)
}

fn pivot(x: &[f32], l: usize, r: usize) -> usize {
    let m = (l + r) / 2;
    if x[l] < x[r] {
        if x[m] <= x[l] {
            l
        } else if x[m] >= x[r] {
            r
        } else {
            m
        }
    } else if x[m] <= x[r] {
        r
    } else if x[m] >= x[l] {
        l
    } else {
        m
    }
}

/// Partition `x` in descending order at rank `k`: afterwards
/// `x[0..k] >= x[k] >= x[k+1..]`. Average case O(n).
pub fn quickselect(x: &mut [f32], k: usize) {
    let n = x.len();
    let mut l = 0usize;
    let mut r = n - 1;
    loop {
        if l == r {
            return;
        }
        if r - l == 1 {
            if x[l] < x[r] {
                x.swap(l, r);
            }
            return;
        }
        let p = pivot(x, l, r);
        let pv = x[p];
        x[p] = x[r];
        let mut p = l;
        for i in l..r {
            if x[i] > pv {
                x.swap(i, p);
                p += 1;
            }
        }
        x[r] = x[p];
        x[p] = pv;
        if k == p {
            return;
        }
        if k < p {
            if p == 0 {
                return;
            }
            r = p - 1;
        } else {
            l = p + 1;
        }
    }
}

/// Median of a slice, via quickselect; O(n) on average.
pub fn median(x: &mut [f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    let k = x.len() / 2;
    quickselect(x, k);
    x[k]
}

fn mean_less_greatest(x: &[f32]) -> f32 {
    let mut greatest = x[0];
    let mut sum = x[0];
    for &v in &x[1..] {
        sum += v;
        if v > greatest {
            greatest = v;
        }
    }
    (sum - greatest) / (x.len() - 1) as f32
}

fn mean_less_two_greatest(x: &[f32]) -> f32 {
    let mut sum = x[0];
    let mut greatest = [x[0], f32::MIN];
    for &v in &x[1..] {
        sum += v;
        if v > greatest[0] {
            greatest[1] = greatest[0];
            greatest[0] = v;
        } else if v > greatest[1] {
            greatest[1] = v;
        }
    }
    (sum - greatest[0] - greatest[1]) / (x.len() - 2) as f32
}

/// Mean of the lower four quintiles: the largest 20% of values are excluded.
/// May reorder `x`.
pub fn tmean(x: &mut [f32]) -> f32 {
    let n = x.len();
    if n == 0 {
        return 0.0;
    }
    if n <= 5 {
        mean_less_greatest(x)
    } else if n <= 10 {
        mean_less_two_greatest(x)
    } else {
        let k = (n + 4) / 5;
        quickselect(x, k);
        x[k..].iter().sum::<f32>() / (n - k) as f32
    }
}
