//! The bucket widths aggregation allows.

/// The bucket widths aggregation allows, in seconds, in increasing order: every divisor of 60 seconds, and 120, 180,
/// and 300 seconds.
///
/// Buckets are aligned to Unix time, so buckets of any two widths up to 60 seconds share a boundary at least every
/// minute, and the larger widths share one with every smaller width at each of their own boundaries. A change from one
/// width to another can therefore take effect at a boundary of both, so that no bucket of either width straddles it,
/// within at most [`MAX_TRANSITION_LCM_SECS`].
pub const ALLOWED_BUCKET_WIDTHS_SECS: [u64; 15] = [1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30, 60, 120, 180, 300];

/// The longest a change of bucket width may wait for a boundary that both widths share, in seconds: 300.
///
/// A change takes effect at a multiple of the least common multiple of the widths it changes between. That is at most
/// 60 seconds between widths up to 60 seconds, and the larger width when one of them is above 60 seconds, but between
/// two different widths above 60 seconds it can exceed this bound: 120 and 180 seconds line up only every 360 seconds.
/// Such a change goes through an intermediate width of at most 60 seconds.
pub const MAX_TRANSITION_LCM_SECS: u64 = 300;

/// Returns `true` if `secs` seconds is an allowed bucket width; see [`ALLOWED_BUCKET_WIDTHS_SECS`].
pub fn is_allowed_bucket_width(secs: u64) -> bool {
    ALLOWED_BUCKET_WIDTHS_SECS.contains(&secs)
}

/// Describes the allowed bucket widths for an error message: `1, 2, 3, ..., 180, or 300 seconds`.
pub fn describe_allowed_widths() -> String {
    let (last, rest) = ALLOWED_BUCKET_WIDTHS_SECS
        .split_last()
        .expect("at least one width is allowed");
    let rest = rest.iter().map(|secs| secs.to_string()).collect::<Vec<_>>().join(", ");
    format!("{}, or {} seconds", rest, last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gcd(a: u64, b: u64) -> u64 {
        if b == 0 {
            a
        } else {
            gcd(b, a % b)
        }
    }

    #[test]
    fn allowed_widths_are_the_divisors_of_a_minute_and_three_larger_widths() {
        let divisors = (1..=60).filter(|secs| 60 % secs == 0).collect::<Vec<u64>>();
        let mut expected = divisors;
        expected.extend([120, 180, 300]);
        assert_eq!(ALLOWED_BUCKET_WIDTHS_SECS.to_vec(), expected);

        for (secs, allowed) in [
            (0, false),
            (1, true),
            (7, false),
            (12, true),
            (45, false),
            (60, true),
            (90, false),
        ] {
            assert_eq!(is_allowed_bucket_width(secs), allowed, "{} seconds", secs);
        }
        for (secs, allowed) in [
            (120, true),
            (180, true),
            (240, false),
            (300, true),
            (360, false),
            (600, false),
        ] {
            assert_eq!(is_allowed_bucket_width(secs), allowed, "{} seconds", secs);
        }
    }

    #[test]
    fn a_change_lines_up_within_the_bound_unless_it_is_between_two_widths_above_a_minute() {
        for old in ALLOWED_BUCKET_WIDTHS_SECS {
            for new in ALLOWED_BUCKET_WIDTHS_SECS {
                let lcm = old / gcd(old, new) * new;
                let between_large_widths = old > 60 && new > 60 && old != new;
                assert_eq!(
                    lcm <= MAX_TRANSITION_LCM_SECS,
                    !between_large_widths,
                    "{} to {} seconds lines up every {} seconds",
                    old,
                    new,
                    lcm
                );
                if !between_large_widths {
                    assert!(
                        lcm <= old.max(new).max(60),
                        "{} to {} seconds lines up every {}",
                        old,
                        new,
                        lcm
                    );
                }
            }
        }
    }

    #[test]
    fn the_description_lists_every_width() {
        assert_eq!(
            describe_allowed_widths(),
            "1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30, 60, 120, 180, or 300 seconds"
        );
    }
}
