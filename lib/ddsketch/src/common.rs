use float_cmp::ApproxEqRatio as _;

/// Compares two floating-point values for approximate equality using a ratio-based approach.
///
/// When comparing two values, the smaller value cannot deviate by more than 0.0000001% of the larger value.
/// This handles NaN values by considering two NaN values as equal.
pub fn float_eq(l_value: f64, r_value: f64) -> bool {
    const RATIO_ERROR: f64 = 0.00000001;

    (l_value.is_nan() && r_value.is_nan()) || l_value.approx_eq_ratio(&r_value, RATIO_ERROR)
}
