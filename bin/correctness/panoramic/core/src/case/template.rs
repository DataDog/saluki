//! Parses step templates and resolves their participant fact references.

use std::str::FromStr;

use serde::{Deserialize, Deserializer};

/// Text containing literal segments and named participant facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    segments: Vec<Segment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Fact { participant: String, fact: String },
}

impl Template {
    /// Returns referenced participant and fact names in text order, including duplicates.
    pub fn references(&self) -> impl Iterator<Item = (&str, &str)> {
        self.segments.iter().filter_map(|segment| match segment {
            Segment::Fact { participant, fact } => Some((participant.as_str(), fact.as_str())),
            Segment::Literal(_) => None,
        })
    }

    /// Substitutes facts, failing on the first missing value or if the result contains `{{`.
    pub fn resolve<'a>(&self, fact: impl Fn(&str, &str) -> Option<&'a str>) -> Result<String, String> {
        let mut resolved = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => resolved.push_str(text),
                Segment::Fact {
                    participant,
                    fact: name,
                } => {
                    let value = fact(participant, name).ok_or_else(|| {
                        format!("cannot resolve fact reference '{{{{ {participant}.facts.{name} }}}}'")
                    })?;
                    resolved.push_str(value);
                }
            }
        }
        if resolved.contains("{{") {
            return Err("resolved template contains a template reference ('{{')".into());
        }
        Ok(resolved)
    }
}

impl FromStr for Template {
    type Err = String;

    fn from_str(mut text: &str) -> Result<Self, Self::Err> {
        let mut segments = Vec::new();
        while let Some(start) = text.find("{{") {
            if start > 0 {
                segments.push(Segment::Literal(text[..start].into()));
            }
            text = &text[start..];
            let end = text
                .find("}}")
                .ok_or_else(|| format!("unterminated template reference '{text}': expected '}}}}'"))?;
            let reference = &text[..end + 2];
            let body = text[2..end].trim_matches(' ');
            if body.starts_with("self.") {
                return Err(format!(
                    "'{reference}': a step has no 'self'; name the participant, as in '{{{{ <participant>.facts.<fact> }}}}'"
                ));
            }
            let parts: Vec<_> = body.split('.').collect();
            if parts.len() != 3
                || parts[1] != "facts"
                || parts
                    .iter()
                    .any(|part| part.is_empty() || part.chars().any(|c| c.is_whitespace() || matches!(c, '{' | '}')))
            {
                return Err(format!(
                    "'{reference}' is not a fact reference of the form '{{{{ <participant>.facts.<fact> }}}}', the only reference step fields support"
                ));
            }
            segments.push(Segment::Fact {
                participant: parts[0].into(),
                fact: parts[2].into(),
            });
            text = &text[end + 2..];
        }
        if !text.is_empty() {
            segments.push(Segment::Literal(text.into()));
        }
        Ok(Self { segments })
    }
}

impl<'de> Deserialize<'de> for Template {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::{decode_fields, StepFields};

    #[test]
    fn literal_templates_resolve_verbatim_including_backslashes_and_single_braces() {
        for text in ["", "/logs/output.log", r"C:\logs\{output}.log", "}text{", "text }}"] {
            let template: Template = text.parse().unwrap();
            assert_eq!(template.references().collect::<Vec<_>>(), []);
            assert_eq!(template.resolve(|_, _| None).unwrap(), text);
        }
    }

    #[test]
    fn fact_references_parse_with_or_without_inner_spaces() {
        for text in [
            "{{subject.facts.file}}",
            "{{ subject.facts.file }}",
            "{{  subject.facts.file  }}",
        ] {
            let template: Template = text.parse().unwrap();
            assert_eq!(template.references().collect::<Vec<_>>(), [("subject", "file")]);
            assert_eq!(template.resolve(|_, _| Some("value")).unwrap(), "value");
        }
    }

    #[test]
    fn literals_and_repeated_references_resolve_in_text_order() {
        let template: Template = "prefix/{{ other.facts.dir }}/{{subject.facts.file}}/{{ other.facts.dir }}.log"
            .parse()
            .unwrap();
        assert_eq!(
            template.references().collect::<Vec<_>>(),
            [("other", "dir"), ("subject", "file"), ("other", "dir")]
        );
        assert_eq!(
            template
                .resolve(|participant, fact| match (participant, fact) {
                    ("other", "dir") => Some("logs"),
                    ("subject", "file") => Some("output"),
                    _ => None,
                })
                .unwrap(),
            "prefix/logs/output/logs.log"
        );
    }

    #[test]
    fn an_unterminated_reference_fails_to_parse() {
        assert_eq!(
            "prefix {{ subject.facts.file".parse::<Template>().unwrap_err(),
            "unterminated template reference '{{ subject.facts.file': expected '}}'"
        );
    }

    #[test]
    fn self_references_ask_for_the_participant_name() {
        let reference = "{{ self.facts.file }}";
        assert_eq!(reference.parse::<Template>().unwrap_err(),
            "'{{ self.facts.file }}': a step has no 'self'; name the participant, as in '{{ <participant>.facts.<fact> }}'");
    }

    #[test]
    fn only_fact_references_are_accepted() {
        for body in [
            "subject.endpoints.api.url",
            "subject.connections.api.url",
            "subject.late.ip",
            "subject.facts",
            "subject.facts.a.b",
            ".facts.file",
            "subject.facts.",
            "subject..file",
            "",
            "subject.facts.two words",
            "sub ject.facts.file",
            "subject.facts.{file",
            "subject.facts.file}",
            "\tsubject.facts.file\n",
        ] {
            let reference = format!("{{{{ {body} }}}}");
            assert_eq!(reference.parse::<Template>().unwrap_err(), format!(
                "'{reference}' is not a fact reference of the form '{{{{ <participant>.facts.<fact> }}}}', the only reference step fields support"
            ));
        }
    }

    #[test]
    fn template_fields_require_yaml_strings_and_name_parse_errors() {
        #[derive(Debug, Deserialize)]
        struct Fields {
            path: Template,
        }

        for (yaml, expected) in [
            ("path: true", "path: invalid type: boolean `true`, expected a string"),
            ("path: 3", "path: invalid type: integer `3`, expected a string"),
            (
                "path: '{{ unclosed'",
                "path: unterminated template reference '{{ unclosed': expected '}}'",
            ),
        ] {
            let fields: StepFields = serde_saphyr::from_str(yaml).unwrap();
            assert_eq!(decode_fields::<Fields>(&fields).unwrap_err(), expected);
        }
        let fields: StepFields = serde_saphyr::from_str("path: 'true'").unwrap();
        assert_eq!(decode_fields::<Fields>(&fields).unwrap().path, "true".parse().unwrap());
    }

    #[test]
    fn resolution_names_the_first_missing_reference() {
        let template: Template = "{{ subject.facts.a }}{{ other.facts.b }}{{ subject.facts.c }}"
            .parse()
            .unwrap();
        assert_eq!(
            template
                .resolve(|_, name| (name == "a").then_some("found"))
                .unwrap_err(),
            "cannot resolve fact reference '{{ other.facts.b }}'"
        );
    }

    #[test]
    fn resolution_cannot_introduce_a_template_reference() {
        for (text, value) in [
            ("{{ subject.facts.file }}", "{{ unresolved }}"),
            ("{{ subject.facts.file }}{", "{"),
        ] {
            let template: Template = text.parse().unwrap();
            assert_eq!(
                template.resolve(|_, _| Some(value)).unwrap_err(),
                "resolved template contains a template reference ('{{')"
            );
        }
    }
}
