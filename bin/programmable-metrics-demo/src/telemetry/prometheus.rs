//! A reader for the Prometheus text exposition format, as ADP's `/metrics` route writes it.
//!
//! It reads `# TYPE` lines and samples (`name{label="value",...} value [timestamp]`), and ignores `# HELP` and other
//! comments. Label values may escape `\\`, `\"`, and `\n`. Values are floats, or `+Inf`, `-Inf`, and `NaN`. A line
//! that cannot be read is skipped and reported, so one bad line never hides the rest of a scrape.

use std::collections::BTreeMap;

/// One sample: a series and its value.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// The metric name, as exposed (dots in internal names are written `__`).
    pub name: String,

    /// The labels, sorted by name.
    pub labels: Vec<(String, String)>,

    /// The value.
    pub value: f64,
}

/// A parsed exposition.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Exposition {
    /// Every sample, in the order the text lists them.
    pub samples: Vec<Sample>,

    /// The declared type of each metric family (`counter`, `gauge`, `histogram`, `summary`, or `untyped`).
    pub types: BTreeMap<String, String>,

    /// The lines that could not be read: their line number (from 1) and why.
    pub skipped: Vec<(usize, String)>,
}

impl Exposition {
    /// Returns the declared type of the family a sample name belongs to.
    ///
    /// The `_bucket`, `_sum`, and `_count` series of a histogram or summary have their family's type.
    pub fn type_of(&self, name: &str) -> Option<&str> {
        if let Some(ty) = self.types.get(name) {
            return Some(ty);
        }
        ["_bucket", "_sum", "_count"].iter().find_map(|suffix| {
            let family = name.strip_suffix(suffix)?;
            self.types
                .get(family)
                .filter(|ty| matches!(ty.as_str(), "histogram" | "summary"))
                .map(String::as_str)
        })
    }
}

/// Parses exposition text.
pub fn parse(text: &str) -> Exposition {
    let mut exposition = Exposition::default();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix('#') {
            let mut words = comment.split_whitespace();
            if words.next() == Some("TYPE") {
                match (words.next(), words.next()) {
                    (Some(name), Some(ty)) => {
                        exposition.types.insert(name.to_string(), ty.to_string());
                    }
                    _ => exposition
                        .skipped
                        .push((index + 1, "A `# TYPE` line needs a metric name and a type.".to_string())),
                }
            }
            continue;
        }
        match parse_sample(line) {
            Ok(sample) => exposition.samples.push(sample),
            Err(reason) => exposition.skipped.push((index + 1, reason)),
        }
    }
    exposition
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == ':'
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == ':'
}

fn parse_sample(line: &str) -> Result<Sample, String> {
    let name_end = line.find(|c: char| !is_name_char(c)).unwrap_or(line.len());
    let name = &line[..name_end];
    if !name.starts_with(is_name_start) {
        return Err(format!("`{}` does not start with a metric name.", line));
    }
    let mut rest = line[name_end..].trim_start();

    let mut labels = Vec::new();
    if let Some(after_brace) = rest.strip_prefix('{') {
        rest = parse_labels(after_brace, &mut labels)?;
    }

    let mut fields = rest.split_whitespace();
    let value = fields
        .next()
        .ok_or_else(|| format!("The sample of `{}` has no value.", name))?;
    let value = parse_value(value).ok_or_else(|| format!("`{}` is not a sample value.", value))?;
    if let Some(timestamp) = fields.next() {
        if timestamp.parse::<i64>().is_err() {
            return Err(format!("`{}` is not a timestamp in milliseconds.", timestamp));
        }
    }
    if let Some(extra) = fields.next() {
        return Err(format!("Unexpected `{}` after the sample of `{}`.", extra, name));
    }

    labels.sort();
    Ok(Sample {
        name: name.to_string(),
        labels,
        value,
    })
}

/// Parses labels up to and including the closing brace, and returns the text after it.
fn parse_labels<'a>(mut rest: &'a str, labels: &mut Vec<(String, String)>) -> Result<&'a str, String> {
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix('}') {
            return Ok(after);
        }
        let name_end = rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len());
        let name = &rest[..name_end];
        if name.is_empty() {
            return Err("Expected a label name or `}`.".to_string());
        }
        rest = rest[name_end..].trim_start();
        rest = rest
            .strip_prefix('=')
            .ok_or_else(|| format!("Expected `=` after the label `{}`.", name))?
            .trim_start();
        rest = rest
            .strip_prefix('"')
            .ok_or_else(|| format!("Expected a quoted value for the label `{}`.", name))?;

        let mut value = String::new();
        let mut chars = rest.char_indices();
        let end = loop {
            match chars.next() {
                Some((at, '"')) => break at,
                Some((_, '\\')) => match chars.next() {
                    Some((_, 'n')) => value.push('\n'),
                    Some((_, '\\')) => value.push('\\'),
                    Some((_, '"')) => value.push('"'),
                    Some((_, other)) => {
                        value.push('\\');
                        value.push(other);
                    }
                    None => return Err(format!("The value of the label `{}` is not closed.", name)),
                },
                Some((_, c)) => value.push(c),
                None => return Err(format!("The value of the label `{}` is not closed.", name)),
            }
        };
        labels.push((name.to_string(), value));
        rest = rest[end + 1..].trim_start();
        if let Some(after) = rest.strip_prefix(',') {
            rest = after;
        } else if !rest.starts_with('}') {
            return Err(format!("Expected `,` or `}}` after the label `{}`.", name));
        }
    }
}

fn parse_value(text: &str) -> Option<f64> {
    match text {
        "+Inf" | "Inf" => Some(f64::INFINITY),
        "-Inf" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        _ => text.parse::<f64>().ok().filter(|value| value.is_finite()),
    }
}

#[cfg(test)]
mod tests {
    use prometheus_exposition::{MetricType, PrometheusRenderer};

    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn text_rendered_by_the_exposition_renderer_reads_back() {
        let mut renderer = PrometheusRenderer::new();
        renderer.render_scalar_group(
            "adp.plan_node_events_total",
            MetricType::Counter,
            Some("Events leaving each plan node port."),
            [
                (vec![("component_id", "metrics_policy"), ("port", "0")], 120.0),
                (vec![("port", "true"), ("component_id", "drop_debug")], 7.5),
            ],
        );
        renderer.render_scalar_group(
            "adp.plan_active_generation",
            MetricType::Gauge,
            None,
            [(Vec::<(&str, &str)>::new(), 3.0)],
        );

        let exposition = parse(renderer.output());
        assert!(exposition.skipped.is_empty(), "{:?}", exposition.skipped);
        assert_eq!(
            exposition.samples,
            vec![
                Sample {
                    name: "adp__plan_node_events_total".to_string(),
                    labels: labels(&[("component_id", "metrics_policy"), ("port", "0")]),
                    value: 120.0,
                },
                Sample {
                    name: "adp__plan_node_events_total".to_string(),
                    labels: labels(&[("component_id", "drop_debug"), ("port", "true")]),
                    value: 7.5,
                },
                Sample {
                    name: "adp__plan_active_generation".to_string(),
                    labels: Vec::new(),
                    value: 3.0,
                },
            ]
        );
        assert_eq!(exposition.type_of("adp__plan_node_events_total"), Some("counter"));
        assert_eq!(exposition.type_of("adp__plan_active_generation"), Some("gauge"));
    }

    #[test]
    fn hand_written_exposition_text_reads_as_written() {
        let text = "\
# HELP adp__component_events_sent_total Events sent.
# TYPE adp__component_events_sent_total counter
adp__component_events_sent_total{component_type=\"x\",component_id=\"dsd_agg\",output=\"_default\"} 42 1700000000000
adp__component_events_sent_total { component_id = \"a\" , } 1e3
# TYPE adp__send_latency_seconds histogram
adp__send_latency_seconds_bucket{le=\"+Inf\"} 5
adp__send_latency_seconds_sum 0.25
adp__send_latency_seconds_count 5
untyped_metric NaN
infinite -Inf
escaped{path=\"C:\\\\x\",quote=\"say \\\"hi\\\"\\nbye\"} 1
# a comment that is not a TYPE line
";
        let exposition = parse(text);
        assert!(exposition.skipped.is_empty(), "{:?}", exposition.skipped);
        let samples = &exposition.samples;
        assert_eq!(samples.len(), 8);
        assert_eq!(
            samples[0].labels,
            labels(&[
                ("component_id", "dsd_agg"),
                ("component_type", "x"),
                ("output", "_default")
            ])
        );
        assert_eq!(samples[0].value, 42.0);
        assert_eq!(samples[1].labels, labels(&[("component_id", "a")]));
        assert_eq!(samples[1].value, 1000.0);
        assert_eq!(samples[2].labels, labels(&[("le", "+Inf")]));
        assert_eq!(
            exposition.type_of("adp__send_latency_seconds_bucket"),
            Some("histogram")
        );
        assert_eq!(exposition.type_of("adp__send_latency_seconds_count"), Some("histogram"));
        assert_eq!(exposition.type_of("adp__component_events_sent_count"), None);
        assert!(samples[5].value.is_nan());
        assert_eq!(samples[6].value, f64::NEG_INFINITY);
        assert_eq!(exposition.type_of("untyped_metric"), None);
        assert_eq!(
            samples[7].labels,
            labels(&[("path", "C:\\x"), ("quote", "say \"hi\"\nbye")])
        );
    }

    #[test]
    fn malformed_lines_are_skipped_and_reported_without_hiding_the_rest() {
        let cases = [
            ("{a=\"b\"} 1", "does not start with a metric name"),
            ("m{a=\"b} 1", "is not closed"),
            ("m{a=b} 1", "Expected a quoted value"),
            ("m{a \"b\"} 1", "Expected `=`"),
            ("m{a=\"b\" c=\"d\"} 1", "Expected `,` or `}`"),
            ("m", "has no value"),
            ("m one", "is not a sample value"),
            ("m 1 soon", "is not a timestamp"),
            ("m 1 2 3", "Unexpected `3`"),
            ("# TYPE m", "needs a metric name and a type"),
        ];
        for (line, reason) in cases {
            let text = format!("good 1\n{}\nalso_good 2\n", line);
            let exposition = parse(&text);
            assert_eq!(exposition.samples.len(), 2, "{}", line);
            assert_eq!(exposition.skipped.len(), 1, "{}", line);
            assert_eq!(exposition.skipped[0].0, 2, "{}", line);
            assert!(
                exposition.skipped[0].1.contains(reason),
                "{}: {}",
                line,
                exposition.skipped[0].1
            );
        }
    }
}
