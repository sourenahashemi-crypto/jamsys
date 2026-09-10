//! Small helpers shared across modules. Deliberately dependency-free.

/// Human duration: "45s", "12m 3s", "2h 14m", "3d 4h".
pub fn human_duration(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        let (m, r) = (s / 60, s % 60);
        if r == 0 { format!("{m}m") } else { format!("{m}m {r}s") }
    } else if s < 86400 {
        let (h, m) = (s / 3600, (s % 3600) / 60);
        if m == 0 { format!("{h}h") } else { format!("{h}h {m}m") }
    } else {
        let (d, h) = (s / 86400, (s % 86400) / 3600);
        if h == 0 { format!("{d}d") } else { format!("{d}d {h}h") }
    }
}

/// Binary bytes with one decimal: "5.2 GB", "912 MB", "4.0 kB".
pub fn human_bytes(b: f64) -> String {
    const U: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    let mut v = b.abs();
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    let sign = if b < 0.0 { "-" } else { "" };
    if i == 0 {
        format!("{sign}{:.0} B", v)
    } else if v >= 100.0 {
        format!("{sign}{:.0} {}", v, U[i])
    } else {
        format!("{sign}{:.1} {}", v, U[i])
    }
}

pub fn human_bps(b: f64) -> String {
    format!("{}/s", human_bytes(b))
}

/// Case-insensitive glob supporting only `*`. Used for journal patterns and
/// suppression rules so the daemon needs no regex engine.
///
/// Iterative with backtracking: linear in the common case, no recursion depth risk.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Robust median. Sorts a scratch copy; input is not modified.
pub fn median(vals: &[f64]) -> f64 {
    if vals.is_empty() {
        return f64::NAN;
    }
    let mut v: Vec<f64> = vals.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

/// Median absolute deviation — the robust analogue of standard deviation.
/// A single outlier cannot inflate it, which is why it is used instead of stddev.
pub fn mad(vals: &[f64], med: f64) -> f64 {
    if vals.is_empty() {
        return f64::NAN;
    }
    let dev: Vec<f64> = vals.iter().filter(|x| x.is_finite()).map(|x| (x - med).abs()).collect();
    median(&dev)
}

/// Linear-interpolated percentile, `q` in 0..=1.
pub fn percentile(vals: &[f64], q: f64) -> f64 {
    if vals.is_empty() {
        return f64::NAN;
    }
    let mut v: Vec<f64> = vals.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = q.clamp(0.0, 1.0);
    let pos = q * (v.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi { v[lo] } else { v[lo] + (v[hi] - v[lo]) * (pos - lo as f64) }
}

/// Theil–Sen slope estimator (median of pairwise slopes), in units per x-unit.
///
/// Used for leak detection instead of least squares because a single GC pause or
/// allocation spike would drag an OLS line and manufacture a "leak".
/// Pairs are subsampled above 60 points to keep this O(n) rather than O(n²).
pub fn theil_sen_slope(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len().min(ys.len());
    if n < 3 {
        return f64::NAN;
    }
    let step = if n > 60 { n / 60 } else { 1 };
    let mut slopes = Vec::with_capacity(64 * 64 / 2);
    let mut i = 0;
    while i < n {
        let mut j = i + step;
        while j < n {
            let dx = xs[j] - xs[i];
            if dx.abs() > f64::EPSILON {
                slopes.push((ys[j] - ys[i]) / dx);
            }
            j += step;
        }
        i += step;
    }
    if slopes.is_empty() {
        return f64::NAN;
    }
    median(&slopes)
}

/// Robust z-score. 0.6745 makes MAD a consistent estimator of sigma for normal data.
/// `min_mad` floors the denominator so a perfectly-constant metric does not produce
/// an infinite score the moment it moves by one unit.
pub fn robust_z(x: f64, med: f64, mad_v: f64, min_mad: f64) -> f64 {
    let m = if mad_v.is_finite() { mad_v.max(min_mad) } else { min_mad };
    if m <= 0.0 {
        return 0.0;
    }
    0.6745 * (x - med) / m
}

/// Clamp a sysfs-derived reading, rejecting the sentinel garbage drivers emit.
pub fn sane(v: f64, lo: f64, hi: f64) -> Option<f64> {
    if v.is_finite() && v >= lo && v <= hi { Some(v) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(60), "1m");
        assert_eq!(human_duration(723), "12m 3s");
        assert_eq!(human_duration(8040), "2h 14m");
        assert_eq!(human_duration(3600), "1h");
        assert_eq!(human_duration(273_600), "3d 4h");
        assert_eq!(human_duration(-5), "0s");
    }

    #[test]
    fn bytes_read_naturally() {
        assert_eq!(human_bytes(0.0), "0 B");
        assert_eq!(human_bytes(4096.0), "4.0 kB");
        assert_eq!(human_bytes(5.2 * 1024.0 * 1024.0 * 1024.0), "5.2 GB");
        assert_eq!(human_bytes(912.0 * 1024.0 * 1024.0), "912 MB");
    }

    #[test]
    fn glob_matches_wildcards_case_insensitively() {
        assert!(glob_match("*xid*", "NVRM: Xid (PCI:0000:01:00): 13, pid=1"));
        assert!(glob_match("snap.*.service", "snap.openshell.gateway.service"));
        assert!(glob_match("exact", "EXACT"));
        assert!(!glob_match("snap.*.service", "user@1000.service"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn glob_backtracks_correctly() {
        // The naive greedy implementation fails this one.
        assert!(glob_match("*ab*cd", "xxabyyabzzcd"));
        assert!(!glob_match("*ab*cd", "xxabyyabzzce"));
    }

    #[test]
    fn median_and_mad_are_outlier_resistant() {
        let v = vec![10.0, 11.0, 10.5, 10.2, 10.8, 400.0];
        let m = median(&v);
        assert!((m - 10.65).abs() < 0.01, "median was {m}");
        // One 400 W spike must not blow up the spread estimate.
        assert!(mad(&v, m) < 1.0);
    }

    #[test]
    fn median_ignores_nan_and_handles_empty() {
        assert!(median(&[]).is_nan());
        assert_eq!(median(&[1.0, f64::NAN, 3.0]), 2.0);
    }

    #[test]
    fn percentiles_interpolate() {
        let v: Vec<f64> = (1..=100).map(|x| x as f64).collect();
        assert!((percentile(&v, 0.5) - 50.5).abs() < 0.001);
        assert!((percentile(&v, 0.95) - 95.05).abs() < 0.01);
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert_eq!(percentile(&v, 1.0), 100.0);
    }

    #[test]
    fn theil_sen_finds_a_leak_through_noise() {
        // 1 GB/h growth sampled each minute, with a big transient dip that would
        // distort a least-squares fit.
        let xs: Vec<f64> = (0..60).map(|i| i as f64 / 60.0).collect();
        let mut ys: Vec<f64> = xs.iter().map(|h| 1000.0 + 1000.0 * h).collect();
        ys[30] = 200.0;
        let slope = theil_sen_slope(&xs, &ys);
        assert!((slope - 1000.0).abs() < 60.0, "slope {slope} should be ~1000 MB/h");
    }

    #[test]
    fn theil_sen_flat_series_has_no_slope() {
        let xs: Vec<f64> = (0..30).map(|i| i as f64).collect();
        let ys = vec![500.0; 30];
        assert!(theil_sen_slope(&xs, &ys).abs() < 1e-9);
    }

    #[test]
    fn robust_z_floors_a_degenerate_mad() {
        // Constant metric: MAD is 0, which would otherwise give an infinite z-score.
        let z = robust_z(11.0, 10.0, 0.0, 0.5);
        assert!(z.is_finite() && z > 0.0, "z was {z}");
        assert!((z - 1.349).abs() < 0.01);
    }

    #[test]
    fn sane_rejects_driver_sentinels() {
        assert_eq!(sane(58.0, -50.0, 150.0), Some(58.0));
        assert_eq!(sane(-274.0, -50.0, 150.0), None);
        assert_eq!(sane(f64::NAN, 0.0, 1.0), None);
        assert_eq!(sane(f64::INFINITY, 0.0, 1e12), None);
    }
}
