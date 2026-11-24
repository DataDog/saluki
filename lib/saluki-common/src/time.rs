//! Time-related functions.

use std::{
    fmt::{self, Write as _},
    ops::Range,
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Once, OnceLock,
    },
    thread,
    time::{Duration, SystemTime},
};

use jiff::{
    fmt::{temporal::DateTimePrinter, StdFmtWrite},
    tz::Offset,
    Unit,
};
use saluki_error::{generic_error, GenericError};

static COARSE_TIME_INITIALIZED: Once = Once::new();
static COARSE_TIME: AtomicU64 = AtomicU64::new(0);
const COARSE_TIME_UPDATE_INTERVAL: Duration = Duration::from_millis(250);

/// Get the current Unix timestamp, in seconds.
///
/// This function is accurate, as it always retrieves the current time for each call. In scenarios where this function
/// is being called frequently, it may pose an unacceptable performance overhead. In such cases, consider using
/// `get_coarse_unix_timestamp`, which provides a cached value that's updated periodically.
pub fn get_unix_timestamp() -> u64 {
    let since_unix_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    since_unix_epoch.as_secs()
}

/// Get the current Unix timestamp, in nanoseconds.
///
/// Like `get_unix_timestamp`, this function always retrieves the current time for each call.
pub fn get_unix_timestamp_nanos() -> u64 {
    let since_unix_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    since_unix_epoch.as_nanos() as u64
}

/// Get the current coarse Unix timestamp, in seconds.
///
/// In scenarios where the current Unix timestamp is needed frequently, this function provides a cached value that's
/// updated periodically. As the precision of the timestamp is one second, this function allows trading off accuracy
/// (see below) for reduced overhead. The resulting value is considered "coarse," because it might be off by significant
/// percentage of the overall precision.
///
/// # Accuracy
///
/// The underlying coarse time is updated roughly every 250 milliseconds, so the value returned by this function may be
/// behind by up to 250 milliseconds. This means that if calling the function at true time `t` (where `t` is in
/// seconds), the value returned may be `t-1` _or_ `t`, but will never be behind by more than 250 milliseconds, and
/// never _ahead_ of `t`.
pub fn get_coarse_unix_timestamp() -> u64 {
    // Initialize a background thread to update the coarse time if it hasn't been initialized yet.
    COARSE_TIME_INITIALIZED.call_once(|| {
        // Initialize the coarse time with the current Unix timestamp.
        COARSE_TIME.store(get_unix_timestamp(), Relaxed);

        thread::spawn(|| {
            loop {
                // Sleep for 250 milliseconds.
                thread::sleep(COARSE_TIME_UPDATE_INTERVAL);

                // Update the coarse time with the current Unix timestamp.
                COARSE_TIME.store(get_unix_timestamp(), Relaxed);
            }
        });
    });

    COARSE_TIME.load(Relaxed)
}

/// A point in time, with nanosecond precision.
///
/// Write it with [`Timestamp::format`], or parse it from RFC 3339 with [`str::parse`]. It can represent any time from
/// the year -9999 to 9999-12-30T22:00:00Z.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(jiff::Timestamp);

impl Timestamp {
    /// The Unix epoch, 1970-01-01T00:00:00Z.
    pub const UNIX_EPOCH: Self = Self(jiff::Timestamp::UNIX_EPOCH);

    /// Gets the current time.
    ///
    /// # Errors
    ///
    /// If the system clock is outside of the range that `Timestamp` supports, an error is returned.
    pub fn now() -> Result<Self, GenericError> {
        jiff::Timestamp::try_from(SystemTime::now())
            .map(Self)
            .map_err(|e| generic_error!("System clock is outside of the supported range: {}", e))
    }

    /// Creates a `Timestamp` from a number of seconds since the Unix epoch, or `None` if it's out of range.
    pub fn from_unix_seconds(seconds: i64) -> Option<Self> {
        jiff::Timestamp::from_second(seconds).ok().map(Self)
    }

    /// Creates a `Timestamp` from a number of milliseconds since the Unix epoch, or `None` if it's out of range.
    pub fn from_unix_millis(millis: i64) -> Option<Self> {
        jiff::Timestamp::from_millisecond(millis).ok().map(Self)
    }

    /// Creates a `Timestamp` from a number of nanoseconds since the Unix epoch, or `None` if it's out of range.
    pub fn from_unix_nanos(nanos: i128) -> Option<Self> {
        jiff::Timestamp::from_nanosecond(nanos).ok().map(Self)
    }

    /// Returns the number of whole milliseconds since the Unix epoch, rounded down to the earlier millisecond.
    pub fn as_unix_millis(self) -> i64 {
        // jiff truncates toward zero, so a time before the epoch with a fractional millisecond needs one less.
        let millis = self.0.as_millisecond();
        if self.0.subsec_nanosecond() % 1_000_000 < 0 {
            millis - 1
        } else {
            millis
        }
    }

    /// Returns the number of nanoseconds since the Unix epoch.
    pub fn as_unix_nanos(self) -> i128 {
        self.0.as_nanosecond()
    }

    /// Returns the number of whole seconds from `earlier` to this timestamp, rounded toward zero.
    ///
    /// The result is negative if `earlier` is actually later, such as when the system clock went backwards.
    pub fn whole_seconds_since(self, earlier: Self) -> i64 {
        self.0.duration_since(earlier.0).as_secs()
    }

    /// Returns a value that writes this timestamp in the given format.
    pub fn format(self, format: TimestampFormat<'_>) -> FormattedTimestamp<'_> {
        FormattedTimestamp {
            timestamp: self,
            format,
        }
    }
}

impl FromStr for Timestamp {
    type Err = GenericError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        input
            .parse()
            .map(Self)
            .map_err(|e| generic_error!("Invalid RFC 3339 timestamp '{}': {}", input, e))
    }
}

/// A time zone, for writing a [`Timestamp`] as local time.
#[derive(Clone, Debug)]
pub struct TimeZone(jiff::tz::TimeZone);

impl TimeZone {
    /// Gets UTC.
    pub fn utc() -> &'static Self {
        static UTC: TimeZone = TimeZone(jiff::tz::TimeZone::UTC);
        &UTC
    }

    /// Gets the local time zone of the system.
    ///
    /// The time zone is detected on the first call and reused for the life of the process. If the `TZ` environment
    /// variable is set, it alone decides the time zone: a value that can't be resolved gives UTC, without looking
    /// anywhere else. Otherwise, the time zone comes from `/etc/localtime` on Unix or the system settings on Windows,
    /// falling back to UTC if no time zone can be found.
    ///
    /// The first call may read the system's time zone database from disk, so make it during startup rather than on a
    /// hot path.
    pub fn system() -> &'static Self {
        static SYSTEM: OnceLock<TimeZone> = OnceLock::new();
        SYSTEM.get_or_init(|| Self(jiff::tz::TimeZone::try_system().unwrap_or(jiff::tz::TimeZone::UTC)))
    }

    /// Creates a time zone from a POSIX `TZ` rule, such as `EST5EDT,M3.2.0,M11.1.0`.
    ///
    /// # Errors
    ///
    /// If the rule isn't valid, an error is returned.
    pub fn posix(rule: &str) -> Result<Self, GenericError> {
        jiff::tz::TimeZone::posix(rule)
            .map(Self)
            .map_err(|e| generic_error!("Invalid POSIX time zone rule '{}': {}", rule, e))
    }
}

/// A format for writing a [`Timestamp`].
///
/// The examples are all 2024-12-31T23:59:59.123456789Z, written in `America/New_York` where the format takes a time
/// zone. Formats in whole seconds or milliseconds truncate the rest, toward the earlier time.
#[derive(Clone, Copy, Debug)]
pub enum TimestampFormat<'a> {
    /// RFC 3339 with a numeric offset: `2024-12-31T18:59:59.123456789-05:00`.
    ///
    /// Fractional seconds use the fewest of 0, 3, 6, or 9 digits that represent the timestamp exactly, and UTC is
    /// written as `+00:00` rather than `Z`.
    ///
    /// RFC 3339 can only write offsets in whole minutes and under 24 hours. An offset with seconds, such as `-01:00:30`
    /// from `TZ=TST1:00:30`, is rounded to the nearest minute, and the local time shifts by up to 30 seconds to match,
    /// so that the timestamp still names the same instant. The last example in section 5.8 of RFC 3339 does the same.
    /// An offset that rounds to 24 hours or more is replaced by UTC.
    Rfc3339(&'a TimeZone),

    /// RFC 3339 in UTC, in whole seconds: `2024-12-31T23:59:59Z`.
    Rfc3339UtcSeconds,

    /// RFC 3339 in UTC, in milliseconds: `2024-12-31T23:59:59.123Z`.
    Rfc3339UtcMillis,

    /// Date and time, in whole seconds: `2024-12-31 18:59:59`.
    DateTime(&'a TimeZone),

    /// Date and time, in whole seconds, followed by the time zone's abbreviation: `2024-12-31 18:59:59 EST`.
    DateTimeWithAbbreviation(&'a TimeZone),

    /// Go's default layout for a time in UTC, in whole seconds: `2024-12-31 23:59:59 +0000 UTC`.
    GoUtc,

    /// Date and time without separators except between them, in whole seconds: `20241231-185959`.
    CompactDateTime(&'a TimeZone),

    /// Time of day, in whole seconds: `18:59:59`.
    TimeOfDay(&'a TimeZone),

    /// Date and time in UTC, without separators and with nine fractional digits: `20241231235959123456789`.
    ///
    /// Parse it with [`parse_compact_utc_nanos`].
    CompactUtcNanos,
}

/// A [`Timestamp`] to be written in a [`TimestampFormat`], returned by [`Timestamp::format`].
#[derive(Clone, Copy, Debug)]
pub struct FormattedTimestamp<'a> {
    timestamp: Timestamp,
    format: TimestampFormat<'a>,
}

impl fmt::Display for FormattedTimestamp<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let timestamp = self.timestamp.0;
        match self.format {
            TimestampFormat::Rfc3339(tz) => write_rfc3339(f, timestamp, rfc3339_offset(tz.0.to_offset(timestamp))),
            TimestampFormat::Rfc3339UtcSeconds => write_rfc3339_utc(f, timestamp, 0),
            TimestampFormat::Rfc3339UtcMillis => write_rfc3339_utc(f, timestamp, 3),
            TimestampFormat::DateTime(tz) => write_date_time(f, &tz.0.to_datetime(timestamp)),
            TimestampFormat::DateTimeWithAbbreviation(tz) => {
                let info = tz.0.to_offset_info(timestamp);
                write_date_time(f, &info.offset().to_datetime(timestamp))?;
                write!(f, " {}", info.abbreviation())
            }
            TimestampFormat::GoUtc => {
                write_date_time(f, &Offset::UTC.to_datetime(timestamp))?;
                f.write_str(" +0000 UTC")
            }
            TimestampFormat::CompactDateTime(tz) => {
                let dt = tz.0.to_datetime(timestamp);
                write_year(f, dt.year())?;
                write!(
                    f,
                    "{:02}{:02}-{:02}{:02}{:02}",
                    dt.month(),
                    dt.day(),
                    dt.hour(),
                    dt.minute(),
                    dt.second()
                )
            }
            TimestampFormat::TimeOfDay(tz) => {
                let dt = tz.0.to_datetime(timestamp);
                write!(f, "{:02}:{:02}:{:02}", dt.hour(), dt.minute(), dt.second())
            }
            TimestampFormat::CompactUtcNanos => {
                let dt = Offset::UTC.to_datetime(timestamp);
                write_year(f, dt.year())?;
                write!(
                    f,
                    "{:02}{:02}{:02}{:02}{:02}{:09}",
                    dt.month(),
                    dt.day(),
                    dt.hour(),
                    dt.minute(),
                    dt.second(),
                    dt.subsec_nanosecond()
                )
            }
        }
    }
}

/// Parses a timestamp written in [`TimestampFormat::CompactUtcNanos`] into nanoseconds since the Unix epoch.
///
/// Returns `None` unless the input is exactly 23 digits that make a valid date and time. Unlike [`Timestamp`], this
/// works up to the end of the year 9999.
pub fn parse_compact_utc_nanos(input: &str) -> Option<i128> {
    let digits = input.as_bytes();
    if digits.len() != 23 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }

    let field = |range: Range<usize>| {
        digits[range]
            .iter()
            .fold(0i32, |value, digit| value * 10 + i32::from(digit - b'0'))
    };
    let datetime = jiff::civil::DateTime::new(
        i16::try_from(field(0..4)).ok()?,
        i8::try_from(field(4..6)).ok()?,
        i8::try_from(field(6..8)).ok()?,
        i8::try_from(field(8..10)).ok()?,
        i8::try_from(field(10..12)).ok()?,
        i8::try_from(field(12..14)).ok()?,
        field(14..23),
    )
    .ok()?;

    // Subtracting civil datetimes, rather than converting to a `Timestamp`, works up to the end of the year 9999.
    let since_epoch = datetime.duration_since(jiff::civil::DateTime::constant(1970, 1, 1, 0, 0, 0, 0));
    Some(since_epoch.as_nanos())
}

/// Returns the offset to write in RFC 3339 for `offset`: rounded to the nearest minute, or UTC if that's 24 hours or
/// more.
fn rfc3339_offset(offset: Offset) -> Offset {
    // Rounding only fails when the result is outside of jiff's offset range, which is also 24 hours or more.
    match offset.round(Unit::Minute) {
        Ok(rounded) if rounded.seconds().abs() < 24 * 60 * 60 => rounded,
        _ => Offset::UTC,
    }
}

fn write_rfc3339(f: &mut fmt::Formatter<'_>, timestamp: jiff::Timestamp, offset: Offset) -> fmt::Result {
    let nanos = offset.to_datetime(timestamp).subsec_nanosecond();
    let precision = if nanos == 0 {
        0
    } else if nanos % 1_000_000 == 0 {
        3
    } else if nanos % 1_000 == 0 {
        6
    } else {
        9
    };

    DateTimePrinter::new()
        .precision(Some(precision))
        .print_timestamp_with_offset(&timestamp, offset, StdFmtWrite(f))
        .map_err(|_| fmt::Error)
}

fn write_rfc3339_utc(f: &mut fmt::Formatter<'_>, timestamp: jiff::Timestamp, precision: u8) -> fmt::Result {
    DateTimePrinter::new()
        .precision(Some(precision))
        .print_timestamp(&timestamp, StdFmtWrite(f))
        .map_err(|_| fmt::Error)
}

fn write_date_time(f: &mut fmt::Formatter<'_>, dt: &jiff::civil::DateTime) -> fmt::Result {
    write_year(f, dt.year())?;
    write!(
        f,
        "-{:02}-{:02} {:02}:{:02}:{:02}",
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

/// Writes a year with at least four digits, and a sign only when it's negative.
fn write_year(f: &mut fmt::Formatter<'_>, year: i16) -> fmt::Result {
    if year < 0 {
        f.write_char('-')?;
    }
    write!(f, "{:04}", year.unsigned_abs())
}

#[cfg(test)]
mod tests {
    use std::{thread::sleep, time::Duration};

    use super::*;

    // 2024-12-31T23:59:59Z
    const SECS: i64 = 1_735_689_599;

    fn at(secs: i64, nanos: i32) -> Timestamp {
        Timestamp(jiff::Timestamp::new(secs, nanos).expect("test timestamp should be valid"))
    }

    fn fixed(seconds: i32) -> TimeZone {
        let offset = Offset::from_seconds(seconds).expect("test offset should be valid");
        TimeZone(jiff::tz::TimeZone::fixed(offset))
    }

    // Matches the current rules of `America/New_York`, without depending on the host's time zone database.
    fn new_york() -> TimeZone {
        TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").expect("test zone should be valid")
    }

    #[test]
    fn every_format_writes_its_documented_example() {
        let timestamp = at(SECS, 123_456_789);
        let tz = new_york();
        let cases = [
            (TimestampFormat::Rfc3339(&tz), "2024-12-31T18:59:59.123456789-05:00"),
            (TimestampFormat::Rfc3339UtcSeconds, "2024-12-31T23:59:59Z"),
            (TimestampFormat::Rfc3339UtcMillis, "2024-12-31T23:59:59.123Z"),
            (TimestampFormat::DateTime(&tz), "2024-12-31 18:59:59"),
            (
                TimestampFormat::DateTimeWithAbbreviation(&tz),
                "2024-12-31 18:59:59 EST",
            ),
            (TimestampFormat::GoUtc, "2024-12-31 23:59:59 +0000 UTC"),
            (TimestampFormat::CompactDateTime(&tz), "20241231-185959"),
            (TimestampFormat::TimeOfDay(&tz), "18:59:59"),
            (TimestampFormat::CompactUtcNanos, "20241231235959123456789"),
        ];

        for (format, expected) in cases {
            assert_eq!(timestamp.format(format).to_string(), expected, "{format:?}");
        }
    }

    #[test]
    fn local_formats_follow_daylight_saving_time() {
        // 2024-07-08T22:46:44Z, during daylight saving time in New York.
        let timestamp = at(1_720_478_804, 0);
        let tz = new_york();
        let cases = [
            (TimestampFormat::Rfc3339(&tz), "2024-07-08T18:46:44-04:00"),
            (
                TimestampFormat::DateTimeWithAbbreviation(&tz),
                "2024-07-08 18:46:44 EDT",
            ),
            (TimestampFormat::CompactDateTime(&tz), "20240708-184644"),
            (TimestampFormat::TimeOfDay(&tz), "18:46:44"),
        ];

        for (format, expected) in cases {
            assert_eq!(timestamp.format(format).to_string(), expected, "{format:?}");
        }
    }

    #[test]
    fn coarse_formats_truncate_toward_the_earlier_time() {
        let utc = TimeZone::utc();
        let cases = [
            (
                at(SECS, 999_999_999),
                TimestampFormat::Rfc3339UtcSeconds,
                "2024-12-31T23:59:59Z",
            ),
            (
                at(SECS, 999_999_999),
                TimestampFormat::Rfc3339UtcMillis,
                "2024-12-31T23:59:59.999Z",
            ),
            (
                at(SECS, 999_999_999),
                TimestampFormat::DateTime(utc),
                "2024-12-31 23:59:59",
            ),
            (
                at(SECS, 999_999_999),
                TimestampFormat::GoUtc,
                "2024-12-31 23:59:59 +0000 UTC",
            ),
            (at(SECS, 999_999_999), TimestampFormat::TimeOfDay(utc), "23:59:59"),
            // Half a second before the epoch is still in the previous second.
            (
                at(0, -500_000_000),
                TimestampFormat::Rfc3339UtcSeconds,
                "1969-12-31T23:59:59Z",
            ),
            (
                at(0, -500_000_000),
                TimestampFormat::Rfc3339UtcMillis,
                "1969-12-31T23:59:59.500Z",
            ),
            (
                at(0, -500_000_000),
                TimestampFormat::DateTime(utc),
                "1969-12-31 23:59:59",
            ),
        ];

        for (timestamp, format, expected) in cases {
            assert_eq!(
                timestamp.format(format).to_string(),
                expected,
                "{timestamp:?} {format:?}"
            );
        }
    }

    #[test]
    fn negative_years_have_a_sign_and_four_digits() {
        let datetime = jiff::civil::DateTime::constant(-1, 6, 15, 12, 0, 0, 0);
        let timestamp = Timestamp(
            Offset::UTC
                .to_timestamp(datetime)
                .expect("test timestamp should be valid"),
        );
        let utc = TimeZone::utc();
        let cases = [
            (TimestampFormat::DateTime(utc), "-0001-06-15 12:00:00"),
            (TimestampFormat::CompactDateTime(utc), "-00010615-120000"),
            (TimestampFormat::CompactUtcNanos, "-00010615120000000000000"),
        ];

        for (format, expected) in cases {
            assert_eq!(timestamp.format(format).to_string(), expected, "{format:?}");
        }
    }

    #[test]
    fn rfc3339_uses_the_fewest_exact_fraction_digits_and_a_numeric_offset() {
        let utc = TimeZone::utc();
        let minus_five = fixed(-5 * 60 * 60);
        let cases = [
            (0, utc, "2024-12-31T23:59:59+00:00"),
            (1, utc, "2024-12-31T23:59:59.000000001+00:00"),
            (1_000, utc, "2024-12-31T23:59:59.000001+00:00"),
            (1_000_000, utc, "2024-12-31T23:59:59.001+00:00"),
            (120_000_000, utc, "2024-12-31T23:59:59.120+00:00"),
            (123_400_000, utc, "2024-12-31T23:59:59.123400+00:00"),
            (123_456_789, utc, "2024-12-31T23:59:59.123456789+00:00"),
            (500_000_000, &minus_five, "2024-12-31T18:59:59.500-05:00"),
        ];

        for (nanos, tz, expected) in cases {
            let actual = at(SECS, nanos).format(TimestampFormat::Rfc3339(tz)).to_string();
            assert_eq!(actual, expected, "nanos {nanos}");
        }
    }

    // Checks each `(offset in seconds, expected output)` case at 2024-12-31T23:59:59Z, and that the output parses back
    // to the same instant.
    fn assert_rfc3339_offset_cases(cases: &[(i32, &str)]) {
        let timestamp = at(SECS, 0);
        for &(seconds, expected) in cases {
            let actual = timestamp.format(TimestampFormat::Rfc3339(&fixed(seconds))).to_string();
            assert_eq!(actual, expected, "offset {seconds}");

            let parsed: Timestamp = actual.parse().expect("output should parse");
            assert_eq!(parsed, timestamp, "offset {seconds}");
        }
    }

    #[test]
    fn rfc3339_rounds_offsets_with_seconds_and_shifts_the_local_time_to_match() {
        // Halves round away from zero. jiff's printer rounds the offset too, but on its own it would keep the local
        // time exact, which names an instant up to 30 seconds away.
        assert_rfc3339_offset_cases(&[
            (-3_600, "2024-12-31T22:59:59-01:00"),
            (19_800, "2025-01-01T05:29:59+05:30"),
            (-3_629, "2024-12-31T22:59:59-01:00"),
            (-3_630, "2024-12-31T22:58:59-01:01"),
            (19_829, "2025-01-01T05:29:59+05:30"),
            (19_830, "2025-01-01T05:30:59+05:31"),
            (-17_762, "2024-12-31T19:03:59-04:56"),
            (30, "2025-01-01T00:00:59+00:01"),
            (-30, "2024-12-31T23:58:59-00:01"),
            // Offsets that round to zero are `+00:00`, because RFC 3339 uses `-00:00` for an unknown local offset.
            (29, "2024-12-31T23:59:59+00:00"),
            (-29, "2024-12-31T23:59:59+00:00"),
            (86_369, "2025-01-01T23:58:59+23:59"),
            (-86_369, "2024-12-31T00:00:59-23:59"),
        ]);
    }

    #[test]
    fn rfc3339_uses_utc_for_offsets_that_round_to_24_hours_or_more() {
        assert_rfc3339_offset_cases(&[
            (86_370, "2024-12-31T23:59:59+00:00"),
            (-86_370, "2024-12-31T23:59:59+00:00"),
            (86_400, "2024-12-31T23:59:59+00:00"),
            (-86_400, "2024-12-31T23:59:59+00:00"),
            (Offset::MAX.seconds(), "2024-12-31T23:59:59+00:00"),
            (Offset::MIN.seconds(), "2024-12-31T23:59:59+00:00"),
        ]);
    }

    #[test]
    fn rfc3339_uses_the_civil_fraction_before_the_epoch() {
        // One nanosecond before the epoch is 23:59:59.999999999 on the previous day, not a negative fraction.
        let timestamp = at(0, -1);
        assert_eq!(
            timestamp.format(TimestampFormat::Rfc3339(TimeZone::utc())).to_string(),
            "1969-12-31T23:59:59.999999999+00:00"
        );
    }

    #[test]
    fn compact_utc_nanos_parse_back_to_the_same_instant() {
        let timestamps = [
            Timestamp::UNIX_EPOCH,
            at(SECS, 123_456_789),
            at(SECS, 1),
            at(0, -1),
            at(253_402_207_200, 999_999_999),
        ];

        for timestamp in timestamps {
            let written = timestamp.format(TimestampFormat::CompactUtcNanos).to_string();
            assert_eq!(
                parse_compact_utc_nanos(&written),
                Some(timestamp.as_unix_nanos()),
                "{written}"
            );
        }
    }

    #[test]
    fn compact_utc_nanos_parse_past_the_range_of_timestamp() {
        assert_eq!(
            parse_compact_utc_nanos("99991231235959999999999"),
            Some(253_402_300_799_999_999_999)
        );
    }

    #[test]
    fn compact_utc_nanos_reject_anything_but_23_digits_of_a_valid_date_and_time() {
        let cases = [
            "",
            "2024123123595912345678",
            "202412312359591234567890",
            "2024123123595912345678x",
            "+0241231235959123456789",
            "20241331235959000000000",
            "20240230000000000000000",
            "20241231240000000000000",
            "20241231235960000000000",
            "２0241231235959123456789",
        ];

        for input in cases {
            assert_eq!(parse_compact_utc_nanos(input), None, "{input}");
        }
    }

    #[test]
    fn unix_conversions_cover_the_supported_range() {
        assert_eq!(Timestamp::from_unix_seconds(0), Some(Timestamp::UNIX_EPOCH));
        assert_eq!(
            Timestamp::from_unix_seconds(253_402_207_200),
            Some(at(253_402_207_200, 0))
        );
        assert_eq!(Timestamp::from_unix_seconds(253_402_207_201), None);
        assert_eq!(Timestamp::from_unix_seconds(i64::MIN), None);
        assert_eq!(
            Timestamp::from_unix_millis(253_402_207_200_000),
            Some(at(253_402_207_200, 0))
        );
        assert_eq!(Timestamp::from_unix_millis(253_402_207_200_001), None);
        assert_eq!(Timestamp::from_unix_nanos(-1), Some(at(0, -1)));
        assert_eq!(Timestamp::from_unix_nanos(i128::MAX), None);

        assert_eq!(at(SECS, 123_456_789).as_unix_nanos(), 1_735_689_599_123_456_789);
        assert_eq!(at(SECS, 123_999_999).as_unix_millis(), 1_735_689_599_123);
        assert_eq!(at(0, -1).as_unix_millis(), -1);
        assert_eq!(at(0, -1_000_000).as_unix_millis(), -1);
        assert_eq!(at(0, -1_999_999).as_unix_millis(), -2);
        assert_eq!(at(-2, -500_000_000).as_unix_millis(), -2_500);
    }

    #[test]
    fn whole_seconds_since_rounds_toward_zero() {
        let started = at(1_795_000_000, 500_000_000);
        assert_eq!(at(1_795_000_090, 200_000_000).whole_seconds_since(started), 89);
        assert_eq!(at(1_795_000_090, 500_000_000).whole_seconds_since(started), 90);
        assert_eq!(at(1_794_999_970, 800_000_000).whole_seconds_since(started), -29);
    }

    #[test]
    fn parses_rfc3339() {
        let expected = at(SECS, 123_000_000);
        for input in ["2024-12-31T23:59:59.123Z", "2024-12-31T18:59:59.123-05:00"] {
            assert_eq!(
                input.parse::<Timestamp>().expect("input should parse"),
                expected,
                "{input}"
            );
        }

        let error = "yesterday".parse::<Timestamp>().expect_err("input should not parse");
        assert!(error.to_string().contains("'yesterday'"), "{error}");
    }

    #[test]
    fn now_reads_the_system_clock() {
        let since_epoch = || {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock should be after the epoch")
                .as_nanos() as i128
        };

        let before = since_epoch();
        let now = Timestamp::now().expect("clock should be in range").as_unix_nanos();
        let after = since_epoch();
        assert!((before..=after).contains(&now), "{now} not in {before}..={after}");
    }

    #[test]
    fn invalid_posix_rules_are_rejected() {
        assert!(TimeZone::posix("not a rule").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn time_zone_detection_works_on_windows_with_the_bundled_database() {
        // Windows has no IANA time zone database, so detection relies on the bundled one. This calls jiff directly
        // because `TimeZone::system` hides a failure by falling back to UTC. It fails if `TZ` is set to a time zone
        // that can't be resolved.
        assert!(jiff::tz::TimeZone::try_system().is_ok());
    }

    #[test]
    fn coarse_timestamp_never_exceeds_accurate_timestamp() {
        // The documented contract is that the coarse timestamp is never _ahead_ of the true time:
        // it is only ever equal to it or lagging behind it, because it is a cached snapshot of a
        // previous `get_unix_timestamp()` call. Sample repeatedly to make an accidental "ahead"
        // read unlikely to slip through.
        for _ in 0..1_000 {
            let coarse = get_coarse_unix_timestamp();
            let accurate = get_unix_timestamp();
            assert!(
                coarse <= accurate,
                "coarse timestamp {coarse} must never be ahead of accurate timestamp {accurate}"
            );
        }
    }

    #[test]
    fn coarse_timestamp_is_monotonically_non_decreasing() {
        // The coarse timestamp is only ever replaced with a newer `get_unix_timestamp()` value, so
        // successive reads must never go backwards, including across an update-interval boundary.
        let first = get_coarse_unix_timestamp();

        let mut previous = first;
        for _ in 0..1_000 {
            let current = get_coarse_unix_timestamp();
            assert!(
                current >= previous,
                "coarse timestamp went backwards: {previous} -> {current}"
            );
            previous = current;
        }

        // Sleep past a couple of update intervals so the background updater runs, then confirm the
        // value has not regressed.
        sleep(COARSE_TIME_UPDATE_INTERVAL * 3);
        let after_sleep = get_coarse_unix_timestamp();
        assert!(
            after_sleep >= previous,
            "coarse timestamp regressed after update: {previous} -> {after_sleep}"
        );
    }

    #[test]
    fn coarse_timestamp_tracks_accurate_within_staleness_bound() {
        // The documented staleness bound is that the coarse value may lag true time by at most
        // ~250ms, i.e. in whole seconds it is either `t` or `t-1`. Once the background updater is
        // running, the difference against the accurate timestamp should therefore be no more than
        // one second. Retry across a short window to absorb one-off scheduling jitter on busy CI.
        get_coarse_unix_timestamp();
        sleep(COARSE_TIME_UPDATE_INTERVAL * 2);

        let mut within_bound = false;
        for _ in 0..20 {
            let accurate = get_unix_timestamp();
            let coarse = get_coarse_unix_timestamp();
            if accurate - coarse <= 1 {
                within_bound = true;
                break;
            }
            sleep(Duration::from_millis(100));
        }

        assert!(
            within_bound,
            "coarse timestamp never came within one second of the accurate timestamp"
        );
    }
}
