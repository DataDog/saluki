//! Workload configuration.

use std::time::Duration;

use serde::Deserialize;

/// Configuration for the workload to generate.
#[derive(Clone, Debug, Deserialize)]
pub struct WorkloadConfig {
    /// The rate of requests to generate (e.g., "100/s", "1000/m").
    pub rate: RateSpec,

    /// The duration to run the simulation.
    #[serde(deserialize_with = "deserialize_duration")]
    pub duration: Duration,
}

impl WorkloadConfig {
    /// Returns the interval between requests.
    pub fn interval(&self) -> Duration {
        self.rate.interval()
    }
}

/// A rate specification (requests per time unit).
#[derive(Clone, Debug)]
pub struct RateSpec {
    /// Requests per second.
    pub per_second: f64,
}

impl RateSpec {
    /// Returns the interval between requests.
    pub fn interval(&self) -> Duration {
        if self.per_second <= 0.0 {
            Duration::from_secs(1)
        } else {
            Duration::from_secs_f64(1.0 / self.per_second)
        }
    }
}

impl<'de> Deserialize<'de> for RateSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse_rate_spec(&s).map_err(serde::de::Error::custom)
    }
}

fn parse_rate_spec(s: &str) -> Result<RateSpec, String> {
    // Expected format: "100/s", "1000/m", "60000/h"
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() != 2 {
        return Err(format!(
            "Invalid rate format '{}'. Expected format: '<count>/<unit>' (e.g., '100/s')",
            s
        ));
    }

    let count: f64 = parts[0]
        .trim()
        .parse()
        .map_err(|_| format!("Invalid count '{}' in rate spec", parts[0]))?;

    let per_second = match parts[1].trim() {
        "s" => count,
        "m" => count / 60.0,
        "h" => count / 3600.0,
        unit => return Err(format!("Invalid time unit '{}'. Expected 's', 'm', or 'h'", unit)),
    };

    Ok(RateSpec { per_second })
}

fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    parse_duration(&s).map_err(serde::de::Error::custom)
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("Duration cannot be empty".to_string());
    }

    // Find where the number ends and the unit begins
    let (num_str, unit) = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map(|i| s.split_at(i))
        .unwrap_or((s, "s")); // default to seconds

    let num: f64 = num_str
        .parse()
        .map_err(|_| format!("Invalid duration number '{}'", num_str))?;

    let multiplier = match unit.trim() {
        "s" | "sec" | "secs" | "" => 1.0,
        "m" | "min" | "mins" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86400.0,
        _ => return Err(format!("Invalid duration unit '{}'", unit)),
    };

    Ok(Duration::from_secs_f64(num * multiplier))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rate_spec() {
        let rate = parse_rate_spec("100/s").unwrap();
        assert!((rate.per_second - 100.0).abs() < 0.001);

        let rate = parse_rate_spec("60/m").unwrap();
        assert!((rate.per_second - 1.0).abs() < 0.001);

        let rate = parse_rate_spec("3600/h").unwrap();
        assert!((rate.per_second - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(172800));
    }
}
