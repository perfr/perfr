// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The statistics a comparison rests on.
//!
//! Every figure that is compared is a ratio of two sums over *units*: time
//! per call is durations over calls, a function's share is its samples over
//! all samples. A unit is a stretch of one capture (a batch) or a whole
//! capture, never a single sample or call: neighbours in time are alike
//! (a slow phase is slow for many calls in a row), and counting them as
//! independent observations is how a profiler comes to call noise a
//! regression. Batches that are long against that likeness are close to
//! independent, which is the method of batch means.
//!
//! The variance of a ratio of sums is the linearized one of survey
//! sampling: with `r = Σy / Σx` over `k` units,
//! `Var(r) = k/(k-1) · Σ(y - r·x)² / (Σx)²`. Two sides are compared with
//! Welch's t-test on that, quantiles with a bootstrap over the same units,
//! and the p-values of a whole report are adjusted together
//! (Benjamini-Hochberg), because a report makes hundreds of comparisons.

use std::f64::consts::PI;

/// `ln Γ(x)` for `x > 0` (Lanczos, g = 7).
pub fn ln_gamma(x: f64) -> f64 {
    const C: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        return PI.ln() - (PI * x).sin().ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let t = x + 7.5;
    let mut sum = C[0];
    for (i, c) in C.iter().enumerate().skip(1) {
        sum += c / (x + i as f64);
    }
    0.5 * (2.0 * PI).ln() + (x + 0.5) * t.ln() - t + sum.ln()
}

/// The continued fraction of the incomplete beta function (modified Lentz).
fn beta_fraction(a: f64, b: f64, x: f64) -> f64 {
    const TINY: f64 = 1e-300;
    let fix = |v: f64| if v.abs() < TINY { TINY } else { v };
    let mut c = 1.0;
    let mut d = 1.0 / fix(1.0 - (a + b) * x / (a + 1.0));
    let mut h = d;
    for m in 1..=400 {
        let m = f64::from(m);
        let even = m * (b - m) * x / ((a + 2.0 * m - 1.0) * (a + 2.0 * m));
        d = 1.0 / fix(1.0 + even * d);
        c = fix(1.0 + even / c);
        h *= d * c;
        let odd = -(a + m) * (a + b + m) * x / ((a + 2.0 * m) * (a + 2.0 * m + 1.0));
        d = 1.0 / fix(1.0 + odd * d);
        c = fix(1.0 + odd / c);
        let step = d * c;
        h *= step;
        if (step - 1.0).abs() < 1e-15 {
            break;
        }
    }
    h
}

/// The regularized incomplete beta function `I_x(a, b)`.
pub fn inc_beta(a: f64, b: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let front = (ln_gamma(a + b) - ln_gamma(a) - ln_gamma(b) + a * x.ln() + b * (1.0 - x).ln()).exp();
    if x < (a + 1.0) / (a + b + 2.0) {
        front * beta_fraction(a, b, x) / a
    } else {
        1.0 - front * beta_fraction(b, a, 1.0 - x) / b
    }
}

/// `P(|T| >= |t|)` for Student's t with `df` degrees of freedom.
pub fn t_two_sided_p(t: f64, df: f64) -> f64 {
    if !t.is_finite() {
        return 0.0;
    }
    inc_beta(df / 2.0, 0.5, df / (df + t * t)).clamp(0.0, 1.0)
}

/// The `t` with `P(|T| <= t) = confidence`: the half-width of a
/// two-sided interval, in standard errors.
pub fn t_critical(confidence: f64, df: f64) -> f64 {
    let tail = 1.0 - confidence.clamp(0.0, 0.999_999_999);
    let (mut low, mut high) = (0.0_f64, 1.0_f64);
    while t_two_sided_p(high, df) > tail && high < 1e12 {
        high *= 2.0;
    }
    for _ in 0..80 {
        let mid = 0.5 * (low + high);
        if t_two_sided_p(mid, df) > tail {
            low = mid;
        } else {
            high = mid;
        }
    }
    0.5 * (low + high)
}

/// A ratio of sums over units, and how well it is known.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Estimate {
    pub value: f64,
    /// The variance of `value`; 0 when there are too few units to say.
    pub variance: f64,
    /// Units that say something: those with a numerator or a denominator.
    pub units: u32,
}

impl Estimate {
    pub fn std_error(&self) -> f64 {
        self.variance.sqrt()
    }
}

/// `Σy / Σx` over `(y, x)` units, with its linearized variance. No units,
/// or none with a denominator, is the value 0 known from nothing.
pub fn ratio_estimate(units: &[(f64, f64)]) -> Estimate {
    let (mut sum_y, mut sum_x, mut k) = (0.0, 0.0, 0u32);
    for (y, x) in units {
        sum_y += y;
        sum_x += x;
        if *y != 0.0 || *x != 0.0 {
            k += 1;
        }
    }
    if sum_x <= 0.0 {
        return Estimate::default();
    }
    let value = sum_y / sum_x;
    if k < 2 {
        return Estimate { value, variance: 0.0, units: k };
    }
    let residuals: f64 = units.iter().map(|(y, x)| (y - value * x).powi(2)).sum();
    let k_f = f64::from(k);
    Estimate { value, variance: k_f / (k_f - 1.0) * residuals / (sum_x * sum_x), units: k }
}

/// Two estimates set against each other.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Comparison {
    pub delta: f64,
    /// The interval of `delta` at the confidence asked for.
    pub delta_ci: Option<(f64, f64)>,
    /// `new / base - 1`; nothing when the base is 0.
    pub change: Option<f64>,
    pub change_ci: Option<(f64, f64)>,
    /// Two-sided, unadjusted. Nothing when a side has too few units.
    pub p: Option<f64>,
}

/// Welch's comparison of `new` against `base`. A side needs `min_units`
/// units for anything to be said beyond the difference itself; a side that
/// is not there at all (value 0 from no units) is the constant 0, and the
/// other side's units carry the test.
pub fn compare(base: &Estimate, new: &Estimate, confidence: f64, min_units: u32) -> Comparison {
    let delta = new.value - base.value;
    let change = (base.value > 0.0).then(|| new.value / base.value - 1.0);
    let absent = |e: &Estimate| e.units == 0 && e.value == 0.0;
    let known = |e: &Estimate| e.units >= min_units.max(2);
    let testable = (known(base) || absent(base)) && (known(new) || absent(new)) && (known(base) || known(new));
    if !testable {
        return Comparison { delta, change, ..Comparison::default() };
    }
    let variance = base.variance + new.variance;
    let se = variance.sqrt();
    // Welch-Satterthwaite; a side with no variance adds nothing to either sum.
    let df_term = |e: &Estimate| if e.units >= 2 { e.variance.powi(2) / f64::from(e.units - 1) } else { 0.0 };
    let df_den = df_term(base) + df_term(new);
    let df = if df_den > 0.0 {
        (variance * variance / df_den).max(1.0)
    } else {
        f64::from(base.units.max(new.units).saturating_sub(1).max(1))
    };
    let t_crit = t_critical(confidence, df);
    let p = if se > 0.0 {
        t_two_sided_p(delta / se, df)
    } else if delta == 0.0 {
        1.0
    } else {
        0.0
    };
    let delta_ci = Some((delta - t_crit * se, delta + t_crit * se));
    let change_ci = if base.value > 0.0 && new.value > 0.0 {
        // On the log scale, where a ratio's error is close to symmetric.
        let log_ratio = (new.value / base.value).ln();
        let log_se = (base.variance / base.value.powi(2) + new.variance / new.value.powi(2)).sqrt();
        Some(((log_ratio - t_crit * log_se).exp() - 1.0, (log_ratio + t_crit * log_se).exp() - 1.0))
    } else if base.value > 0.0 {
        delta_ci.map(|(low, high)| ((low / base.value).max(-1.0), (high / base.value).max(-1.0)))
    } else {
        None
    };
    Comparison { delta, delta_ci, change, change_ci, p: Some(p) }
}

/// Benjamini-Hochberg adjusted p-values (q-values), in the order given.
/// Rejecting every q at or under `alpha` keeps the expected share of false
/// findings among the findings at or under `alpha`.
pub fn benjamini_hochberg(p: &[f64]) -> Vec<f64> {
    let m = p.len();
    let mut order: Vec<usize> = (0..m).collect();
    order.sort_by(|a, b| p[*a].total_cmp(&p[*b]));
    let mut q = vec![1.0; m];
    let mut running = 1.0_f64;
    for (rank, index) in order.iter().enumerate().rev() {
        running = running.min(p[*index] * m as f64 / (rank + 1) as f64);
        q[*index] = running;
    }
    q
}

/// A small deterministic generator (SplitMix64): a report must come out the
/// same every time it is made from the same captures.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`.
    pub fn below(&mut self, n: usize) -> usize {
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Values with the unit each belongs to, for quantiles and their bootstrap.
#[derive(Clone, Debug, Default)]
pub struct UnitValues {
    /// Sorted by value.
    sorted: Vec<(f64, u32)>,
    units: u32,
}

impl UnitValues {
    /// `values` are `(value, unit)` with units numbered `0..units`.
    pub fn new(mut values: Vec<(f64, u32)>, units: u32) -> UnitValues {
        values.sort_by(|a, b| a.0.total_cmp(&b.0));
        UnitValues { sorted: values, units }
    }

    pub fn len(&self) -> usize {
        self.sorted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }

    pub fn units(&self) -> u32 {
        self.units
    }

    /// The smallest value with at least `q` of the values at or under it.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        let n = self.sorted.len();
        (n > 0).then(|| self.sorted[((q * n as f64).ceil() as usize).clamp(1, n) - 1].0)
    }

    /// The same quantile with each value counted as many times as its
    /// unit was drawn.
    fn weighted_quantile(&self, q: f64, drawn: &[u32]) -> Option<f64> {
        let total: u64 = self.sorted.iter().map(|(_, unit)| u64::from(drawn[*unit as usize])).sum();
        if total == 0 {
            return None;
        }
        let target = ((q * total as f64).ceil() as u64).clamp(1, total);
        let mut seen = 0u64;
        for (value, unit) in &self.sorted {
            seen += u64::from(drawn[*unit as usize]);
            if seen >= target {
                return Some(*value);
            }
        }
        self.sorted.last().map(|(value, _)| *value)
    }
}

/// The interval of the relative change of a quantile, `new / base - 1`,
/// from a bootstrap that redraws whole units on each side. Nothing when a
/// side has fewer than `min_units` units or the base quantile is not
/// positive.
pub fn quantile_change_ci(
    base: &UnitValues,
    new: &UnitValues,
    q: f64,
    confidence: f64,
    min_units: u32,
    resamples: usize,
    rng: &mut Rng,
) -> Option<(f64, f64)> {
    if base.units < min_units.max(2) || new.units < min_units.max(2) || resamples < 20 {
        return None;
    }
    let draw = |values: &UnitValues, drawn: &mut Vec<u32>, rng: &mut Rng| {
        drawn.clear();
        drawn.resize(values.units as usize, 0);
        for _ in 0..values.units {
            drawn[rng.below(values.units as usize)] += 1;
        }
        values.weighted_quantile(q, drawn)
    };
    let (mut drawn_base, mut drawn_new) = (Vec::new(), Vec::new());
    let mut changes: Vec<f64> = Vec::with_capacity(resamples);
    for _ in 0..resamples {
        let (Some(b), Some(n)) = (draw(base, &mut drawn_base, rng), draw(new, &mut drawn_new, rng)) else {
            continue;
        };
        if b > 0.0 {
            changes.push(n / b - 1.0);
        }
    }
    if changes.len() < resamples / 2 {
        return None;
    }
    changes.sort_by(f64::total_cmp);
    let tail = (1.0 - confidence) / 2.0;
    let at = |fraction: f64| changes[((fraction * changes.len() as f64) as usize).min(changes.len() - 1)];
    Some((at(tail), at(1.0 - tail)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tolerance: f64) -> bool {
        (a - b).abs() <= tolerance
    }

    #[test]
    fn the_t_distribution_matches_the_tables() {
        assert!(close(ln_gamma(5.0), 24.0_f64.ln(), 1e-10));
        assert!(close(ln_gamma(0.5), PI.sqrt().ln(), 1e-10));
        assert!(close(t_two_sided_p(2.0, 10.0), 0.073_388, 1e-5), "{}", t_two_sided_p(2.0, 10.0));
        assert!(close(t_two_sided_p(0.0, 7.0), 1.0, 1e-12));
        assert!(close(t_two_sided_p(12.706, 1.0), 0.05, 1e-4));
        assert!(close(t_critical(0.95, 10.0), 2.228_14, 1e-4), "{}", t_critical(0.95, 10.0));
        assert!(close(t_critical(0.95, 1.0), 12.706_2, 1e-3));
        assert!(close(t_critical(0.99, 30.0), 2.749_996, 1e-4));
        assert!(close(t_critical(0.95, 1e7), 1.959_964, 1e-4));
        // Welch's degrees of freedom are not whole numbers.
        assert!(close(t_two_sided_p(2.5, 7.3), 0.039_9, 2e-3), "{}", t_two_sided_p(2.5, 7.3));
        assert_eq!(t_two_sided_p(f64::INFINITY, 5.0), 0.0);
    }

    #[test]
    fn a_ratio_of_sums_is_the_mean_when_every_unit_counts_one() {
        let units: Vec<(f64, f64)> = [2.0, 4.0, 6.0, 8.0].iter().map(|y| (*y, 1.0)).collect();
        let estimate = ratio_estimate(&units);
        assert_eq!(estimate.value, 5.0);
        // The variance of a mean: s² / n, with s² = 20/3.
        assert!(close(estimate.variance, 20.0 / 3.0 / 4.0, 1e-12));
        assert_eq!(estimate.units, 4);
    }

    #[test]
    fn units_that_say_nothing_are_not_counted_and_nothing_is_zero_known_from_nothing() {
        let estimate = ratio_estimate(&[(1.0, 2.0), (0.0, 0.0), (3.0, 2.0), (0.0, 0.0)]);
        assert_eq!((estimate.value, estimate.units), (1.0, 2));
        assert_eq!(ratio_estimate(&[]), Estimate::default());
        assert_eq!(ratio_estimate(&[(0.0, 0.0)]), Estimate::default());
        // One unit: a value, and no idea how good.
        assert_eq!(ratio_estimate(&[(6.0, 3.0)]), Estimate { value: 2.0, variance: 0.0, units: 1 });
    }

    #[test]
    fn welch_agrees_with_a_worked_example() {
        // Two samples of five: means 10 and 12, variances 2.5 and 2.5.
        let base = ratio_estimate(&[8.0, 9.0, 10.0, 11.0, 12.0].map(|y| (y, 1.0)));
        let new = ratio_estimate(&[10.0, 11.0, 12.0, 13.0, 14.0].map(|y| (y, 1.0)));
        let found = compare(&base, &new, 0.95, 3);
        assert_eq!(found.delta, 2.0);
        // t = 2 / sqrt(0.5 + 0.5) = 2 on 8 degrees of freedom.
        assert!(close(found.p.unwrap(), 0.080_52, 1e-4), "{:?}", found.p);
        let (low, high) = found.delta_ci.unwrap();
        assert!(close(low, 2.0 - 2.306, 1e-3) && close(high, 2.0 + 2.306, 1e-3), "{low} {high}");
        assert!(close(found.change.unwrap(), 0.2, 1e-12));
        let (low, high) = found.change_ci.unwrap();
        assert!(low < 0.0 && high > 0.4 && high < 0.5, "{low} {high}");
    }

    #[test]
    fn too_few_units_give_the_difference_and_no_claim() {
        let base = ratio_estimate(&[(10.0, 1.0), (12.0, 1.0)]);
        let new = ratio_estimate(&[(20.0, 1.0), (22.0, 1.0)]);
        let found = compare(&base, &new, 0.95, 5);
        assert_eq!(found.delta, 10.0);
        assert_eq!((found.p, found.delta_ci, found.change_ci), (None, None, None));
    }

    #[test]
    fn something_that_is_only_on_one_side_is_tested_against_nothing() {
        let new = ratio_estimate(&[1.0, 1.2, 0.9, 1.1, 1.0, 0.8].map(|y| (y, 10.0)));
        let found = compare(&Estimate::default(), &new, 0.95, 5);
        assert!(found.p.unwrap() < 1e-4, "{:?}", found.p);
        assert_eq!(found.change, None);
        let gone = compare(&new, &Estimate::default(), 0.95, 5);
        assert_eq!(gone.change, Some(-1.0));
        assert!(gone.change_ci.unwrap().0 >= -1.0);
    }

    #[test]
    fn identical_sides_are_not_different_and_constant_ones_that_differ_are() {
        let side = ratio_estimate(&[3.0; 6].map(|y| (y, 1.0)));
        assert_eq!(compare(&side, &side, 0.95, 5).p, Some(1.0));
        let other = ratio_estimate(&[4.0; 6].map(|y| (y, 1.0)));
        assert_eq!(compare(&side, &other, 0.95, 5).p, Some(0.0));
    }

    #[test]
    fn adjusted_p_values_are_the_textbook_ones() {
        let q = benjamini_hochberg(&[0.01, 0.04, 0.03, 0.005, 0.5]);
        // Sorted: .005 .01 .03 .04 .5 -> .025 .025 .05 .05 .5
        let expected = [0.025, 0.05, 0.05, 0.025, 0.5];
        for (found, wanted) in q.iter().zip(expected) {
            assert!(close(*found, wanted, 1e-12), "{q:?}");
        }
        assert!(benjamini_hochberg(&[]).is_empty());
    }

    #[test]
    fn quantiles_are_nearest_rank_and_a_draw_of_every_unit_once_changes_nothing() {
        let values = UnitValues::new((1..=10).map(|v| (f64::from(v), (v as u32 - 1) / 2)).collect(), 5);
        assert_eq!(values.quantile(0.5), Some(5.0));
        assert_eq!(values.quantile(0.95), Some(10.0));
        assert_eq!(values.quantile(0.0), Some(1.0));
        assert_eq!(values.weighted_quantile(0.5, &[1; 5]), Some(5.0));
        // Only the last unit drawn: its two values are all there is.
        assert_eq!(values.weighted_quantile(0.5, &[0, 0, 0, 0, 3]), Some(9.0));
        assert_eq!(values.weighted_quantile(0.5, &[0; 5]), None);
        assert_eq!(UnitValues::default().quantile(0.5), None);
    }

    /// A normal deviate (Box-Muller), for the simulations below.
    fn normal(rng: &mut Rng) -> f64 {
        (-2.0 * (1.0 - rng.unit()).ln()).sqrt() * (2.0 * PI * rng.unit()).cos()
    }

    /// A series whose neighbours are alike: AR(1) with coefficient `phi`.
    fn series(rng: &mut Rng, n: usize, phi: f64, mean: f64) -> Vec<f64> {
        let mut level = normal(rng) / (1.0 - phi * phi).sqrt();
        (0..n)
            .map(|_| {
                level = phi * level + normal(rng);
                mean + level
            })
            .collect()
    }

    fn batched(values: &[f64], batches: usize) -> Estimate {
        let size = values.len() / batches;
        let units: Vec<(f64, f64)> = values.chunks(size).map(|chunk| (chunk.iter().sum(), chunk.len() as f64)).collect();
        ratio_estimate(&units)
    }

    #[test]
    fn batches_keep_the_false_alarm_rate_where_single_calls_do_not() {
        // The same process on both sides, 2,000 times: how often is a
        // difference called at 5%? Calls that take after their neighbours
        // (phi 0.9) are each worth far less than one independent call.
        let mut rng = Rng::new(7);
        let (mut by_call, mut by_batch) = (0, 0);
        let rounds = 2000;
        for _ in 0..rounds {
            let a = series(&mut rng, 3200, 0.9, 100.0);
            let b = series(&mut rng, 3200, 0.9, 100.0);
            if compare(&batched(&a, 3200), &batched(&b, 3200), 0.95, 5).p.unwrap() < 0.05 {
                by_call += 1;
            }
            if compare(&batched(&a, 32), &batched(&b, 32), 0.95, 5).p.unwrap() < 0.05 {
                by_batch += 1;
            }
        }
        let rate = |hits: i32| f64::from(hits) / f64::from(rounds);
        assert!(rate(by_call) > 0.5, "single calls: {}", rate(by_call));
        assert!((0.03..=0.075).contains(&rate(by_batch)), "batches: {}", rate(by_batch));
    }

    #[test]
    fn independent_calls_are_judged_at_the_stated_rate_and_a_real_shift_is_found() {
        let mut rng = Rng::new(11);
        let (mut alarms, mut found, mut covered) = (0, 0, 0);
        let rounds = 2000;
        for _ in 0..rounds {
            let a = series(&mut rng, 640, 0.0, 100.0);
            let same = series(&mut rng, 640, 0.0, 100.0);
            let slower = series(&mut rng, 640, 0.0, 100.3);
            if compare(&batched(&a, 32), &batched(&same, 32), 0.95, 5).p.unwrap() < 0.05 {
                alarms += 1;
            }
            let shifted = compare(&batched(&a, 32), &batched(&slower, 32), 0.95, 5);
            if shifted.p.unwrap() < 0.05 {
                found += 1;
            }
            let (low, high) = shifted.change_ci.unwrap();
            if low <= 0.003 && 0.003 <= high {
                covered += 1;
            }
        }
        let rate = |hits: i32| f64::from(hits) / f64::from(rounds);
        assert!((0.03..=0.07).contains(&rate(alarms)), "false alarms: {}", rate(alarms));
        // A shift of 0.3 standard deviations with 640 calls a side.
        assert!(rate(found) > 0.99, "power: {}", rate(found));
        assert!((0.93..=0.97).contains(&rate(covered)), "coverage: {}", rate(covered));
    }

    #[test]
    fn a_share_is_judged_at_the_stated_rate_when_the_total_moves_too() {
        // A function's samples and all samples per time batch, where the
        // thread is busier in some batches than others: the ratio's
        // variance has to take the moving denominator into account.
        let mut rng = Rng::new(23);
        let side = |rng: &mut Rng, share: f64| -> Estimate {
            let units: Vec<(f64, f64)> = (0..32)
                .map(|_| {
                    let total = 100 + rng.below(400);
                    let hits = (0..total).filter(|_| rng.unit() < share).count();
                    (hits as f64, total as f64)
                })
                .collect();
            ratio_estimate(&units)
        };
        let (mut alarms, mut found) = (0, 0);
        let rounds = 1000;
        for _ in 0..rounds {
            let base = side(&mut rng, 0.10);
            if compare(&base, &side(&mut rng, 0.10), 0.95, 5).p.unwrap() < 0.05 {
                alarms += 1;
            }
            if compare(&base, &side(&mut rng, 0.13), 0.95, 5).p.unwrap() < 0.05 {
                found += 1;
            }
        }
        let rate = |hits: i32| f64::from(hits) / f64::from(rounds);
        assert!((0.025..=0.075).contains(&rate(alarms)), "false alarms: {}", rate(alarms));
        assert!(rate(found) > 0.99, "power: {}", rate(found));
    }

    #[test]
    fn the_interval_of_a_median_covers_the_true_change() {
        // Skewed durations (log-normal), the new side 10% slower.
        let mut rng = Rng::new(31);
        let side = |rng: &mut Rng, scale: f64| -> UnitValues {
            UnitValues::new((0..640).map(|i| (scale * (0.5 * normal(rng)).exp(), i / 20)).collect(), 32)
        };
        let (mut covered, mut excludes_zero) = (0, 0);
        let rounds = 300;
        for _ in 0..rounds {
            let (base, new) = (side(&mut rng, 1.0), side(&mut rng, 1.1));
            let (low, high) = quantile_change_ci(&base, &new, 0.5, 0.95, 5, 400, &mut rng).unwrap();
            if low <= 0.1 && 0.1 <= high {
                covered += 1;
            }
            if low > 0.0 {
                excludes_zero += 1;
            }
        }
        let rate = |hits: i32| f64::from(hits) / f64::from(rounds);
        assert!((0.90..=0.99).contains(&rate(covered)), "coverage: {}", rate(covered));
        assert!(rate(excludes_zero) > 0.5, "power: {}", rate(excludes_zero));
    }

    #[test]
    fn a_quantile_interval_needs_units_on_both_sides() {
        let mut rng = Rng::new(1);
        let few = UnitValues::new(vec![(1.0, 0), (2.0, 1)], 2);
        let many = UnitValues::new((0..40).map(|i| (f64::from(i), i as u32 / 4)).collect(), 10);
        assert_eq!(quantile_change_ci(&few, &many, 0.5, 0.95, 5, 200, &mut rng), None);
        assert!(quantile_change_ci(&many, &many, 0.5, 0.95, 5, 200, &mut rng).is_some());
    }
}
