//! PELT changepoint detection over an RBF kernel, and the segment classifier.
//!
//! `internal/snow/{medfilt,rbf,pelt,pelt_computer}.go`, ported. What it produces is
//! `snow_events` — the classified accumulation segments the website draws as bands — and
//! not any of the seven numbers `/snow` serves.
//!
//! ```text
//!  depth series (mm) ──▶ MedFilt(kernel) ──▶ RbfCost ──▶ Pelt(penalty) ──▶ breakpoints
//!                                                                              │
//!                             segments, classified ◀────────────────────────────┘
//!                     accumulation | spike_then_settle | redistribution | plateau
//! ```
//!
//! # What the algorithm is for, in one paragraph
//!
//! "How much snow fell" is not `max - min` of a depth series and not a sum of positive
//! deltas; `super::threshold` has why, with the Go tree's own measured numbers. PELT is the
//! other answer to the same question: partition the series into segments whose *shape*
//! differs, then ask what each segment was. A segment that rose and stayed up accumulated
//! its net change; one that rose and came back accumulated its peak (a drift that blew in
//! and blew out still fell as snow); one that fell was redistribution; one that did neither
//! was a plateau. `penalty` is what decides how many segments there are.
//!
//! # The two exact reformulations, and the one deviation
//!
//! The port is value-for-value with the Go implementation **except** in three places, and
//! the first two change no result:
//!
//! 1. **The segment cost is evaluated from a 2-D prefix sum rather than by summing the
//!    sub-matrix.** `RBFCost::Error` is `sum(diagonal) - sum(all)/length` over
//!    `gram[start..end][start..end]`, which is `O(len²)` per call and is called `O(n²)`
//!    times. Two facts make it `O(1)`: the kernel's diagonal is **exactly 1**
//!    (`exp(-gamma * 0)`), so `sum(diagonal)` is the segment's length; and a sub-matrix sum
//!    is four lookups into a prefix sum. Same number, `O(n²)` total instead of `O(n⁴)`.
//! 2. **The dynamic program carries two arrays rather than a map of partitions.** Go copies
//!    the whole left partition for every candidate (`for k, v := range leftPartition`),
//!    which is the same recurrence —
//!    `F(b) = min over admissible t of F(t) + cost(t, b) + penalty` — written with `O(n)`
//!    work per candidate instead of `O(1)`. The pruning rule, the admissible set's
//!    construction and the tie-break (first minimum wins) are reproduced exactly, because
//!    each of those *can* change which breakpoints come out.
//! 3. **The deviation: the longer windows are read at the hour level rather than the
//!    5-minute level.** `super::EVENT_WINDOWS` has the whole of it. The short version is
//!    that both matrices above are `O(n²)` in the bucket count, so Go's 8 days of 5-minute
//!    buckets for the 168-hour window is 2304 points — a 42 MiB kernel matrix and a 21 MiB
//!    distance vector, inside a 10-second budget, on a release target measured at ARM1176.
//!    The 5-minute series is median-filtered with a **61-bucket** kernel first, which is
//!    five hours, so the signal being segmented is already hourly.

use mw_component::EventClass;

/// The PELT penalty: higher means fewer breakpoints.
///
/// `penalty: 8.0`.
pub const PENALTY: f64 = 8.0;

/// The shortest segment, in buckets.
///
/// `minSize: 2`, with the Go comment "Minimum 2-hour segments" — which is the comment being
/// wrong rather than the constant: the unit is buckets, and on 5-minute buckets it is ten
/// minutes. Reproduced as written.
pub const MIN_SIZE: usize = 2;

/// The candidate-breakpoint stride.
///
/// `jump: 1` — "No subsampling". Kept at one because it is what the shipped detector uses;
/// the parameter exists in the recurrence and raising it would change every event.
pub const JUMP: usize = 1;

/// The rise that makes a segment an accumulation, in millimetres.
///
/// `minAccumulation: 10.0` — "10mm minimum to count as accumulation".
pub const MIN_ACCUMULATION_MM: f64 = 10.0;

/// The most buckets this will segment.
///
/// Both of the algorithm's matrices are `O(n²)`, so this is the bound that makes their size
/// a property of the code rather than of how long a station has been reporting. At the cap,
/// the prefix sum is 8.4 MiB and the distance vector 4.2 MiB; `super::EVENT_WINDOWS` chooses
/// levels that stay under 800 buckets, so the cap is a backstop and not the operating point.
///
/// A series longer than this is **truncated to its newest buckets** by the query's own
/// `limit`, which is the right end to keep: an event window is a trailing window.
pub const MAX_POINTS: usize = 1024;

/// A median filter with zero padding — `scipy.signal.medfilt`, as Go ports it.
///
/// `kernel` must be odd; an even one returns the input unchanged rather than panicking
/// (Go panics, and a panic in a component body is a process this programme does not take
/// down for an arithmetic parameter it chose itself).
///
/// **The zero padding is reproduced and it is visible.** The first and last `kernel / 2`
/// outputs are medians of windows half-filled with zeros, so a filtered depth series begins
/// and ends by sagging toward zero — on a 61-bucket kernel over 5-minute data that is two
/// and a half hours at each end. It changes which segments the detector finds at the edges
/// of every window, and it is kept because changing it would change every stored event
/// without a specification saying what the new ones should be.
pub fn median_filter(data: &[f64], kernel: usize) -> Vec<f64> {
    if data.is_empty() {
        return Vec::new();
    }
    if kernel < 1 || kernel.is_multiple_of(2) {
        return data.to_vec();
    }
    let half = (kernel / 2) as isize;
    let n = data.len() as isize;
    let mut out = Vec::with_capacity(data.len());
    let mut window = Vec::with_capacity(kernel);
    for i in 0..n {
        window.clear();
        for j in -half..=half {
            let at = i + j;
            // Zero padding, not edge replication: `scipy.signal.medfilt`'s default and
            // what Go ports.
            window.push(if at < 0 || at >= n {
                0.0
            } else {
                data[at as usize]
            });
        }
        window.sort_by(f64::total_cmp);
        out.push(window[kernel / 2]);
    }
    out
}

/// The RBF kernel cost over one signal, as a prefix sum.
///
/// `gamma` is the median heuristic: `1 / median(nonzero pairwise squared distances)`, with
/// the median taken as `sorted[len / 2]` — the **upper** median on an even count, which is
/// Go's indexing and not the average of the two middle values.
#[derive(Debug)]
pub struct RbfCost {
    /// `(n + 1)²` row-major prefix sums of the kernel matrix, so a sub-matrix sum is four
    /// lookups. `prefix[i * (n+1) + j]` is the sum over `gram[0..i][0..j]`.
    prefix: Vec<f64>,
    /// How many samples.
    n: usize,
    /// The median-heuristic bandwidth, kept for the unit tests and the log.
    gamma: f64,
}

impl RbfCost {
    /// Fit the kernel to one signal.
    ///
    /// `O(n²)` in time and memory, which [`MAX_POINTS`] is the bound on.
    pub fn fit(signal: &[f64]) -> Self {
        let n = signal.len();
        let gamma = median_gamma(signal);
        let stride = n + 1;
        let mut prefix = vec![0.0; stride * stride];
        for i in 0..n {
            // One row of the kernel, accumulated into the prefix sum as it is produced, so
            // the kernel matrix itself is never materialised.
            let mut row_sum = 0.0;
            for j in 0..n {
                let diff = signal[i] - signal[j];
                row_sum += (-gamma * diff * diff).exp();
                prefix[(i + 1) * stride + (j + 1)] = prefix[i * stride + (j + 1)] + row_sum;
            }
        }
        Self { prefix, n, gamma }
    }

    /// How many samples the cost was fitted to.
    pub fn len(&self) -> usize {
        self.n
    }

    /// Whether the signal was empty.
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The median-heuristic bandwidth.
    pub fn gamma(&self) -> f64 {
        self.gamma
    }

    /// The cost of the segment `[start, end)`.
    ///
    /// `sum(diagonal) - sum(all) / length`, where the diagonal sum is the length because
    /// `exp(-gamma * 0) = 1`. `+inf` for a degenerate range, which is Go's answer and makes
    /// such a candidate unselectable rather than a panic.
    pub fn error(&self, start: usize, end: usize) -> f64 {
        if start >= end || end > self.n {
            return f64::INFINITY;
        }
        let length = (end - start) as f64;
        length - self.total(start, end) / length
    }

    /// The sum of the kernel sub-matrix `[start, end) × [start, end)`.
    fn total(&self, start: usize, end: usize) -> f64 {
        let s = self.n + 1;
        self.prefix[end * s + end] - self.prefix[start * s + end] - self.prefix[end * s + start]
            + self.prefix[start * s + start]
    }
}

/// `1 / median(nonzero pairwise squared distances)`, or `1.0` when there is no spread.
///
/// `computeMedianGamma`, including both of its fallbacks: fewer than two samples and a
/// signal with no two distinct values both answer `1.0`. The distances are collected and
/// sorted rather than selected, which is `O(n² log n)` — the same cost as Go, and bounded
/// by [`MAX_POINTS`].
fn median_gamma(signal: &[f64]) -> f64 {
    let n = signal.len();
    if n < 2 {
        return 1.0;
    }
    let mut distances = Vec::with_capacity(n * (n - 1) / 2);
    for i in 0..n {
        for j in (i + 1)..n {
            let diff = signal[i] - signal[j];
            let squared = diff * diff;
            // Go collects only `dist > 0`, so a flat stretch does not drag the median to
            // zero -- and a wholly flat signal collects nothing and falls through to 1.0.
            if squared > 0.0 {
                distances.push(squared);
            }
        }
    }
    if distances.is_empty() {
        return 1.0;
    }
    distances.sort_by(f64::total_cmp);
    let median = distances[distances.len() / 2];
    if median == 0.0 {
        return 1.0;
    }
    1.0 / median
}

/// The optimal breakpoints: the **ends** of the segments of the best partition, ascending.
///
/// The last element is always the signal's length when a partition exists. An empty result
/// is a signal shorter than [`MIN_SIZE`], which Go reaches by the same route — its
/// `newAdmPt` goes negative, no partition of `[0, n)` is ever recorded, and `Predict`
/// returns nothing.
pub fn breakpoints(cost: &RbfCost, penalty: f64, min_size: usize, jump: usize) -> Vec<usize> {
    let n = cost.len();
    if n == 0 || min_size == 0 || jump == 0 {
        return Vec::new();
    }
    // `F[t]` is the optimal cost of a partition of `[0, t)`, penalties included; `None` is
    // "no partition of this prefix exists", which is Go's missing map entry.
    let mut f: Vec<Option<f64>> = vec![None; n + 1];
    let mut from: Vec<usize> = vec![0; n + 1];
    f[0] = Some(0.0);

    let mut candidates: Vec<usize> = (0..n).step_by(jump).filter(|&k| k >= min_size).collect();
    candidates.push(n);

    let mut admissible: Vec<usize> = Vec::new();
    let mut evaluated: Vec<(usize, f64)> = Vec::new();
    for &bkp in &candidates {
        // The point that becomes admissible at this candidate. Computed signed, because
        // Go's `floor((bkp - minSize) / jump) * jump` is negative for a short signal and a
        // negative index has no partition -- so it is skipped rather than clamped to zero,
        // which would make `[0, bkp)` a legal first segment shorter than `minSize`.
        let numerator = bkp as isize - min_size as isize;
        if numerator >= 0 {
            admissible.push((numerator as usize / jump) * jump);
        }
        evaluated.clear();
        let mut best: Option<(f64, usize)> = None;
        for &t in &admissible {
            let Some(prefix_cost) = f[t] else {
                continue;
            };
            let total = prefix_cost + cost.error(t, bkp) + penalty;
            evaluated.push((t, total));
            // Strictly less than, so the **first** minimum wins -- which is Go's
            // `if cost < minCost` and is what makes the result deterministic on a tie.
            if best.is_none_or(|(lowest, _)| total < lowest) {
                best = Some((total, t));
            }
        }
        let Some((lowest, argmin)) = best else {
            continue;
        };
        f[bkp] = Some(lowest);
        from[bkp] = argmin;
        // PELT's pruning: a candidate whose best total is more than one penalty worse than
        // the optimum can never be part of a better partition later.
        admissible = evaluated
            .iter()
            .filter(|&&(_, total)| total <= lowest + penalty)
            .map(|&(t, _)| t)
            .collect();
    }

    if f[n].is_none() {
        return Vec::new();
    }
    let mut ends = Vec::new();
    let mut at = n;
    while at > 0 {
        ends.push(at);
        at = from[at];
    }
    ends.reverse();
    ends
}

/// One classified segment of the depth series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    /// The index of the segment's first bucket.
    pub start: usize,
    /// The index one past the segment's last bucket.
    pub end: usize,
    /// The smoothed depth at the first bucket, millimetres.
    pub start_depth_mm: f64,
    /// The smoothed depth at the last bucket, millimetres.
    pub end_depth_mm: f64,
    /// What the segment was.
    pub class: EventClass,
    /// How much snow it contributed, millimetres. Zero for the two classes that contribute
    /// none.
    pub snow_mm: f64,
}

/// Classify the segments the breakpoints cut the smoothed series into.
///
/// `classifySegments`, with its own boundary handling: a zero is prepended when the
/// breakpoints do not start at one, and an end past the series is clamped.
///
/// The four rules, in the order Go tests them:
///
/// | Test | Class | Contributes |
/// |---|---|---|
/// | the peak is ≥ 10 mm above the start, and the net is ≥ 5 mm | `accumulation` | its **net** change |
/// | the peak is ≥ 10 mm above the start, and the net is not | `spike_then_settle` | its **peak** rise |
/// | the trough is ≥ 10 mm below the start | `redistribution` | nothing |
/// | neither | `plateau` | nothing |
///
/// `spike_then_settle` contributing its peak rather than its net is the rule worth reading
/// twice: a drift that blew in and blew out still fell as snow, and a 24-hour total that
/// ignored it would under-report every windy storm.
pub fn classify(smoothed: &[f64], breakpoints: &[usize], min_accumulation_mm: f64) -> Vec<Segment> {
    let mut cuts: Vec<usize> = Vec::with_capacity(breakpoints.len() + 1);
    if breakpoints.first() != Some(&0) {
        cuts.push(0);
    }
    cuts.extend_from_slice(breakpoints);
    let mut out = Vec::new();
    for pair in cuts.windows(2) {
        let start = pair[0];
        let end = pair[1].min(smoothed.len());
        if start >= end {
            continue;
        }
        let depths = &smoothed[start..end];
        let start_depth = depths[0];
        let end_depth = depths[depths.len() - 1];
        let max_depth = depths.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let min_depth = depths.iter().copied().fold(f64::INFINITY, f64::min);
        let net = end_depth - start_depth;
        let rise = max_depth - start_depth;
        let fall = start_depth - min_depth;
        let (class, snow_mm) = if rise >= min_accumulation_mm {
            if net >= min_accumulation_mm * 0.5 {
                (EventClass::Accumulation, net)
            } else {
                (EventClass::SpikeThenSettle, rise)
            }
        } else if fall >= min_accumulation_mm {
            (EventClass::Redistribution, 0.0)
        } else {
            (EventClass::Plateau, 0.0)
        };
        out.push(Segment {
            start,
            end,
            start_depth_mm: start_depth,
            end_depth_mm: end_depth,
            class,
            snow_mm,
        });
    }
    out
}

/// Detect and classify in one call, at this module's own constants.
pub fn segments(depths: &[f64], kernel: usize) -> (Vec<f64>, Vec<Segment>) {
    let smoothed = median_filter(depths, kernel);
    let cost = RbfCost::fit(&smoothed);
    let cuts = breakpoints(&cost, PENALTY, MIN_SIZE, JUMP);
    let classified = classify(&smoothed, &cuts, MIN_ACCUMULATION_MM);
    (smoothed, classified)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The naive sub-matrix sum, for asserting the prefix-sum reformulation against the
    /// expression Go evaluates.
    fn naive_error(signal: &[f64], gamma: f64, start: usize, end: usize) -> f64 {
        if start >= end {
            return f64::INFINITY;
        }
        let length = (end - start) as f64;
        let mut diagonal = 0.0;
        let mut total = 0.0;
        for i in start..end {
            for j in start..end {
                let diff = signal[i] - signal[j];
                let k = (-gamma * diff * diff).exp();
                total += k;
                if i == j {
                    diagonal += k;
                }
            }
        }
        diagonal - total / length
    }

    #[test]
    fn the_median_filter_is_scipy_s_with_zero_padding() {
        // A single spike inside a flat run is removed.
        assert_eq!(
            median_filter(&[1.0, 1.0, 9.0, 1.0, 1.0], 3),
            vec![1.0, 1.0, 1.0, 1.0, 1.0]
        );
        // And the zero padding is visible at the edges: the first window is [0, 5, 5].
        assert_eq!(median_filter(&[5.0, 5.0, 5.0], 3), vec![5.0, 5.0, 5.0]);
        assert_eq!(median_filter(&[5.0, 5.0], 3), vec![5.0, 5.0]);
        assert_eq!(median_filter(&[5.0], 3), vec![0.0], "window [0, 5, 0]");
    }

    #[test]
    fn an_even_kernel_or_an_empty_series_is_handled_rather_than_panicking() {
        assert_eq!(median_filter(&[1.0, 2.0], 2), vec![1.0, 2.0]);
        assert_eq!(median_filter(&[1.0, 2.0], 0), vec![1.0, 2.0]);
        assert!(median_filter(&[], 3).is_empty());
    }

    /// **Reformulation 1, asserted.** The prefix-sum cost is the expression Go evaluates,
    /// for every segment of a signal with structure in it.
    #[test]
    fn the_prefix_sum_cost_is_the_submatrix_sum() {
        let signal: Vec<f64> = (0..24)
            .map(|i| if i < 12 { 0.0 } else { 40.0 } + f64::from(i % 3))
            .collect();
        let cost = RbfCost::fit(&signal);
        let gamma = cost.gamma();
        for start in 0..signal.len() {
            for end in (start + 1)..=signal.len() {
                let expected = naive_error(&signal, gamma, start, end);
                let got = cost.error(start, end);
                assert!(
                    (got - expected).abs() < 1e-9,
                    "[{start}, {end}): {got} != {expected}"
                );
            }
        }
    }

    /// The diagonal of the kernel is exactly one, which is what makes `sum(diagonal)` the
    /// segment's length — the fact reformulation 1 rests on.
    #[test]
    fn the_kernels_diagonal_is_exactly_one() {
        let cost = RbfCost::fit(&[3.0, 7.0, 11.0]);
        for i in 0..3 {
            assert_eq!(
                cost.error(i, i + 1),
                0.0,
                "a one-sample segment costs 1 - 1/1"
            );
        }
    }

    #[test]
    fn a_degenerate_range_is_infinite_rather_than_a_panic() {
        let cost = RbfCost::fit(&[1.0, 2.0, 3.0]);
        assert!(cost.error(2, 2).is_infinite());
        assert!(cost.error(3, 1).is_infinite());
        assert!(cost.error(0, 9).is_infinite());
    }

    /// `computeMedianGamma`'s two fallbacks.
    #[test]
    fn a_flat_or_tiny_signal_has_a_bandwidth_of_one() {
        assert_eq!(median_gamma(&[]), 1.0);
        assert_eq!(median_gamma(&[4.0]), 1.0);
        assert_eq!(median_gamma(&[4.0; 50]), 1.0, "no two distinct values");
    }

    /// The median is the **upper** one on an even count, which is Go's `sorted[len/2]`.
    #[test]
    fn the_bandwidth_is_the_reciprocal_of_the_upper_median_distance() {
        // Distances are 1, 4, 9 (from 0, 1, 2, 3 -> pairs 1,2,3,1,2,1 squared = 1,4,9,1,4,1)
        // sorted: 1,1,1,4,4,9 -> index 3 -> 4.
        assert_eq!(median_gamma(&[0.0, 1.0, 2.0, 3.0]), 1.0 / 4.0);
    }

    /// A step is found, and it is found at the step.
    ///
    /// The signal has to be long enough for the penalty to be worth paying, and the
    /// arithmetic is worth writing down because it is the whole meaning of `penalty = 8`.
    /// For a two-level signal of `n` samples split evenly, one segment costs
    /// `n - total/n`, which grows linearly in `n`, while two pure segments cost zero plus
    /// one extra penalty. At `n = 20` the single segment costs 6.3 and the split costs 8,
    /// so **the step is deliberately not reported** — at `n = 120` the single segment costs
    /// 38 and the split still costs 8. A detector that found a changepoint in twenty
    /// samples of anything would find one in every gust.
    #[test]
    fn a_single_step_is_one_breakpoint_at_the_step() {
        let mut signal = vec![0.0; 120];
        for v in signal.iter_mut().skip(60) {
            *v = 50.0;
        }
        let cost = RbfCost::fit(&signal);
        let cuts = breakpoints(&cost, PENALTY, MIN_SIZE, JUMP);
        assert_eq!(*cuts.last().expect("a partition exists"), 120);
        assert_eq!(cuts, vec![60, 120], "one cut, at the step: {cuts:?}");
    }

    /// A step in a signal too short for the penalty is **not** reported, which is the other
    /// half of the test above and is what the parameter is for.
    #[test]
    fn a_step_too_small_to_pay_for_is_not_reported() {
        let mut signal = vec![0.0; 20];
        for v in signal.iter_mut().skip(10) {
            *v = 50.0;
        }
        let cost = RbfCost::fit(&signal);
        assert_eq!(breakpoints(&cost, PENALTY, MIN_SIZE, JUMP), vec![20]);
        // And with a penalty it can pay, the same signal splits at the step.
        assert_eq!(breakpoints(&cost, 1.0, MIN_SIZE, JUMP), vec![10, 20]);
    }

    /// **The dynamic program finds the optimum**, asserted against an exhaustive search over
    /// every admissible partition. This is the licence for reformulation 2: PELT's pruning
    /// is only valid if the recurrence it prunes is the one being solved, and an exhaustive
    /// check is the only way to know the pruning did not cut the answer out.
    #[test]
    fn the_dynamic_program_agrees_with_an_exhaustive_search() {
        /// Every partition of `[0, n)` into segments of at least `min_size`, as cut lists.
        fn partitions(n: usize, min_size: usize) -> Vec<Vec<usize>> {
            fn walk(
                at: usize,
                n: usize,
                min_size: usize,
                so_far: &mut Vec<usize>,
                out: &mut Vec<Vec<usize>>,
            ) {
                if at == n {
                    out.push(so_far.clone());
                    return;
                }
                for end in (at + min_size)..=n {
                    if end != n && n - end < min_size {
                        continue;
                    }
                    so_far.push(end);
                    walk(end, n, min_size, so_far, out);
                    so_far.pop();
                }
            }
            let mut out = Vec::new();
            walk(0, n, min_size, &mut Vec::new(), &mut out);
            out
        }

        // Three shapes: a step, a ramp and noise. Each at a length the exhaustive search
        // can enumerate and the penalty can act on.
        let signals: Vec<Vec<f64>> = vec![
            (0..14).map(|i| if i < 7 { 0.0 } else { 40.0 }).collect(),
            (0..14).map(|i| f64::from(i) * 4.0).collect(),
            (0..14).map(|i| f64::from((i * 7) % 5) * 3.0).collect(),
        ];
        for signal in &signals {
            let cost = RbfCost::fit(signal);
            for penalty in [0.5, 2.0, 8.0] {
                let got = breakpoints(&cost, penalty, MIN_SIZE, JUMP);
                let total = |cuts: &[usize]| -> f64 {
                    let mut sum = 0.0;
                    let mut at = 0;
                    for &end in cuts {
                        sum += cost.error(at, end) + penalty;
                        at = end;
                    }
                    sum
                };
                let best = partitions(signal.len(), MIN_SIZE)
                    .iter()
                    .map(|cuts| total(cuts))
                    .fold(f64::INFINITY, f64::min);
                assert!(
                    (total(&got) - best).abs() < 1e-9,
                    "penalty {penalty}: the program found {} and the optimum is {best} \
                     ({got:?})",
                    total(&got)
                );
            }
        }
    }

    /// A flat signal has no interior breakpoints — one segment, ending at the length.
    #[test]
    fn a_flat_signal_is_one_segment() {
        let cost = RbfCost::fit(&[7.0; 30]);
        assert_eq!(breakpoints(&cost, PENALTY, MIN_SIZE, JUMP), vec![30]);
    }

    /// A higher penalty buys fewer segments, which is what the parameter means.
    #[test]
    fn the_penalty_trades_segments_for_cost() {
        let signal: Vec<f64> = (0..40)
            .map(|i| f64::from(i / 8) * 20.0 + f64::from(i % 2))
            .collect();
        let cost = RbfCost::fit(&signal);
        let few = breakpoints(&cost, 1000.0, MIN_SIZE, JUMP).len();
        let many = breakpoints(&cost, 0.1, MIN_SIZE, JUMP).len();
        assert!(many > few, "{many} should exceed {few}");
    }

    /// A signal shorter than `min_size` produces no partition, which is the route Go's
    /// negative `newAdmPt` takes.
    #[test]
    fn a_signal_shorter_than_the_minimum_segment_has_no_breakpoints() {
        for len in 0..MIN_SIZE {
            let signal = vec![1.0; len];
            let cost = RbfCost::fit(&signal);
            assert!(
                breakpoints(&cost, PENALTY, MIN_SIZE, JUMP).is_empty(),
                "a signal of {len}"
            );
        }
    }

    /// Every segment is at least `min_size` long — which is what the signed `newAdmPt`
    /// guard buys and a clamp to zero would have broken.
    #[test]
    fn no_segment_is_shorter_than_the_minimum() {
        let signal: Vec<f64> = (0..37).map(|i| f64::from(i / 3) * 15.0).collect();
        let cost = RbfCost::fit(&signal);
        let cuts = breakpoints(&cost, PENALTY, MIN_SIZE, JUMP);
        let mut previous = 0;
        for &cut in &cuts {
            assert!(
                cut - previous >= MIN_SIZE,
                "segment [{previous}, {cut}) is shorter than {MIN_SIZE}: {cuts:?}"
            );
            previous = cut;
        }
    }

    /// The classifier's four rules, one constructed segment each.
    #[test]
    fn the_four_classes_are_the_four_shapes() {
        // Rose and stayed up: accumulation, contributing its net change.
        let up = classify(&[0.0, 10.0, 20.0], &[3], MIN_ACCUMULATION_MM);
        assert_eq!(up[0].class, EventClass::Accumulation);
        assert_eq!(up[0].snow_mm, 20.0);

        // Rose 30 and came back to 2: a spike, contributing its PEAK and not its net.
        let spike = classify(&[0.0, 30.0, 2.0], &[3], MIN_ACCUMULATION_MM);
        assert_eq!(spike[0].class, EventClass::SpikeThenSettle);
        assert_eq!(spike[0].snow_mm, 30.0);

        // Fell past the threshold: redistribution, contributing nothing.
        let down = classify(&[40.0, 20.0, 15.0], &[3], MIN_ACCUMULATION_MM);
        assert_eq!(down[0].class, EventClass::Redistribution);
        assert_eq!(down[0].snow_mm, 0.0);

        // Neither: a plateau.
        let flat = classify(&[10.0, 11.0, 9.0], &[3], MIN_ACCUMULATION_MM);
        assert_eq!(flat[0].class, EventClass::Plateau);
        assert_eq!(flat[0].snow_mm, 0.0);
    }

    /// The accumulation/spike boundary is `net >= minAccumulation * 0.5`, which is 5 mm.
    #[test]
    fn the_net_change_decides_between_accumulation_and_a_spike() {
        let just_accumulation = classify(&[0.0, 30.0, 5.0], &[3], MIN_ACCUMULATION_MM);
        assert_eq!(just_accumulation[0].class, EventClass::Accumulation);
        assert_eq!(just_accumulation[0].snow_mm, 5.0);
        let just_spike = classify(&[0.0, 30.0, 4.9], &[3], MIN_ACCUMULATION_MM);
        assert_eq!(just_spike[0].class, EventClass::SpikeThenSettle);
    }

    /// The boundary handling: a zero is prepended and an end past the series is clamped.
    #[test]
    fn the_cuts_are_closed_at_both_ends() {
        let segments = classify(&[0.0, 1.0, 2.0, 3.0], &[2, 4], MIN_ACCUMULATION_MM);
        assert_eq!(segments.len(), 2);
        assert_eq!((segments[0].start, segments[0].end), (0, 2));
        assert_eq!((segments[1].start, segments[1].end), (2, 4));
        // A breakpoint past the end is clamped rather than panicking.
        let clamped = classify(&[0.0, 1.0], &[9], MIN_ACCUMULATION_MM);
        assert_eq!((clamped[0].start, clamped[0].end), (0, 2));
        // And no breakpoints at all is no segments.
        assert!(classify(&[0.0, 1.0], &[], MIN_ACCUMULATION_MM).is_empty());
    }

    /// End to end on the shape the component sees: a storm that drops 100 mm over six
    /// hours of 5-minute buckets, and the detector reports one accumulation that carries
    /// most of it.
    #[test]
    fn a_storm_is_found_as_accumulation() {
        // Twelve hours: flat, then a six-hour rise, then flat.
        let mut depths = vec![0.0; 36];
        depths.extend((0..72).map(|i| f64::from(i) * (100.0 / 72.0)));
        depths.extend(vec![100.0; 36]);
        let (smoothed, segments) = segments(&depths, 5);
        assert_eq!(smoothed.len(), depths.len());
        let snow: f64 = segments.iter().map(|s| s.snow_mm).sum();
        assert!(
            snow > 80.0 && snow <= 101.0,
            "about a hundred millimetres: {snow} from {segments:#?}"
        );
        // Every segment that carries snow is an accumulation: the rise was monotonic, so
        // nothing should be classified as a spike or as redistribution. How MANY segments
        // the detector cuts the rise into is the penalty's business and not this test's.
        for segment in segments.iter().filter(|s| s.snow_mm > 0.0) {
            assert_eq!(
                segment.class,
                EventClass::Accumulation,
                "{segment:?} in {segments:#?}"
            );
        }
    }

    /// The cap is a bound on the matrices and nothing in `EVENT_WINDOWS` reaches it.
    #[test]
    fn the_point_cap_bounds_the_matrices() {
        assert_eq!(MAX_POINTS, 1024);
        let bytes = (MAX_POINTS + 1) * (MAX_POINTS + 1) * std::mem::size_of::<f64>();
        assert!(bytes < 9 * 1024 * 1024, "{bytes} bytes at the cap");
    }
}
