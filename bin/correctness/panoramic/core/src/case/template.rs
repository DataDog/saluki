//! Parses template references and resolves early participant facts.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer};

use crate::{backend::LateFact, step::Need};

/// Text containing literals and participant fact, late-fact, endpoint or connection references.
///
/// [`FromStr`] parses every reference form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Template {
    segments: Vec<Segment>,
}

/// A step field containing literals and facts or late facts of named participants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepTemplate(pub(crate) Template);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Reference { written: String, reference: Reference },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Reference {
    Fact {
        participant: ParticipantRef,
        fact: String,
    },
    Late {
        participant: ParticipantRef,
        fact: LateFact,
    },
    Endpoint {
        participant: ParticipantRef,
        endpoint: String,
        part: AddressPart,
    },
    Connection {
        consumed: String,
        part: AddressPart,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ParticipantRef {
    Holder,
    Named(String),
}

impl ParticipantRef {
    pub(crate) fn name<'a>(&'a self, holder: &'a str) -> &'a str {
        match self {
            Self::Holder => holder,
            Self::Named(name) => name,
        }
    }
}

impl fmt::Display for ParticipantRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Holder => f.write_str("self"),
            Self::Named(name) => f.write_str(name),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AddressPart {
    Url,
    Host,
    Port,
    Path,
}

impl fmt::Display for AddressPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Url => "url",
            Self::Host => "host",
            Self::Port => "port",
            Self::Path => "path",
        })
    }
}

impl Reference {
    fn parse(reference: &str) -> Result<Self, String> {
        let body = reference[2..reference.len() - 2].trim_matches(' ');
        let parts: Vec<_> = body.split('.').collect();
        let invalid = || {
            format!(
                "'{reference}' is not a template reference; write '{{{{ <participant>.facts.<fact> }}}}', \
                 '{{{{ <participant>.late.<fact> }}}}', '{{{{ <participant>.endpoints.<endpoint>.<part> }}}}' \
                 or '{{{{ self.connections.<endpoint>.<part> }}}}'"
            )
        };
        if parts
            .iter()
            .any(|part| part.is_empty() || part.chars().any(|c| c.is_whitespace() || matches!(c, '{' | '}')))
        {
            return Err(invalid());
        }
        let participant = |name: &str| match name {
            "self" => ParticipantRef::Holder,
            name => ParticipantRef::Named(name.into()),
        };
        let address_part = |name: &str| match name {
            "url" => Ok(AddressPart::Url),
            "host" => Ok(AddressPart::Host),
            "port" => Ok(AddressPart::Port),
            "path" => Ok(AddressPart::Path),
            _ => Err(format!("'{reference}': an address part is url, host, port or path")),
        };
        match parts.as_slice() {
            [name, "facts", fact] => Ok(Self::Fact {
                participant: participant(name),
                fact: (*fact).into(),
            }),
            [name, "late", fact] => Ok(Self::Late {
                participant: participant(name),
                fact: LateFact::from_name(fact)
                    .ok_or_else(|| format!("'{reference}': a late fact is container_id, ip or pod_uid"))?,
            }),
            [name, "endpoints", endpoint, part] => Ok(Self::Endpoint {
                participant: participant(name),
                endpoint: (*endpoint).into(),
                part: address_part(part)?,
            }),
            ["self", "connections", consumed, part] => Ok(Self::Connection {
                consumed: (*consumed).into(),
                part: address_part(part)?,
            }),
            [_, "connections", _, _] => Err(format!(
                "'{reference}': connections are referenced by their consumer, \
                 as '{{{{ self.connections.<endpoint>.<part> }}}}'"
            )),
            _ => Err(invalid()),
        }
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fact { participant, fact } => write!(f, "{{{{ {participant}.facts.{fact} }}}}"),
            Self::Late { participant, fact } => write!(f, "{{{{ {participant}.late.{} }}}}", fact.name()),
            Self::Endpoint {
                participant,
                endpoint,
                part,
            } => write!(f, "{{{{ {participant}.endpoints.{endpoint}.{part} }}}}"),
            Self::Connection { consumed, part } => write!(f, "{{{{ self.connections.{consumed}.{part} }}}}"),
        }
    }
}

impl Template {
    /// Returns references in text order, including duplicates.
    pub(crate) fn references(&self) -> impl Iterator<Item = &Reference> {
        self.segments.iter().filter_map(|segment| match segment {
            Segment::Reference { reference, .. } => Some(reference),
            Segment::Literal(_) => None,
        })
    }

    /// Returns references and their original spelling in text order, including duplicates.
    pub(crate) fn written_references(&self) -> impl Iterator<Item = (&str, &Reference)> {
        self.segments.iter().filter_map(|segment| match segment {
            Segment::Reference { written, reference } => Some((written.as_str(), reference)),
            Segment::Literal(_) => None,
        })
    }

    /// Substitutes selected references, preserving the others as written.
    ///
    /// # Errors
    ///
    /// Propagates resolver errors. Rejects a replacement containing `{{` or forming it with adjacent text.
    pub(crate) fn substitute(
        &self, mut value: impl FnMut(&str, &Reference) -> Result<Option<String>, String>,
    ) -> Result<String, String> {
        const DELIMITER_ERROR: &str = "substitution would form a template delimiter ('{{')";
        let mut resolved = String::new();
        for segment in &self.segments {
            let replacement;
            let text = match segment {
                Segment::Literal(text) => text,
                Segment::Reference { written, reference } => match value(written, reference)? {
                    Some(value) => {
                        if value.contains("{{") {
                            return Err(DELIMITER_ERROR.into());
                        }
                        replacement = value;
                        &replacement
                    }
                    None => written,
                },
            };
            if resolved.ends_with('{') && text.starts_with('{') {
                return Err(DELIMITER_ERROR.into());
            }
            resolved.push_str(text);
        }
        Ok(resolved)
    }

    /// Writes selected late references to the holder in canonical form, preserving all other text.
    pub(crate) fn canonicalize_own_late(&self, holder: &str, substituted: impl Fn(LateFact) -> bool) -> String {
        self.segments
            .iter()
            .map(|segment| match segment {
                Segment::Reference {
                    reference: Reference::Late { participant, fact },
                    ..
                } if participant.name(holder) == holder && substituted(*fact) => fact.substituted_reference(),
                Segment::Reference { written, .. } => written,
                Segment::Literal(text) => text,
            })
            .collect()
    }

    fn check_step_field(&self) -> Result<(), String> {
        let allowed = "step fields accept only fact and late fact references to named participants";
        let name_participant = "a step has no 'self'; name the participant, as in";
        for segment in &self.segments {
            let Segment::Reference { written, reference } = segment else {
                continue;
            };
            match reference {
                Reference::Endpoint { .. } | Reference::Connection { .. } => {
                    return Err(format!("'{written}': {allowed}"));
                }
                Reference::Fact {
                    participant: ParticipantRef::Holder,
                    ..
                } => {
                    return Err(format!(
                        "'{written}': {name_participant} '{{{{ <participant>.facts.<fact> }}}}'"
                    ));
                }
                Reference::Late {
                    participant: ParticipantRef::Holder,
                    fact,
                } => {
                    return Err(format!(
                        "'{written}': {name_participant} '{{{{ <participant>.late.{} }}}}'",
                        fact.name()
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

impl StepTemplate {
    /// Reports named fact and late-fact needs for templates read from step fields.
    /// Preserves text order and duplicates.
    pub fn needs(&self) -> impl Iterator<Item = Need> + '_ {
        self.0.references().filter_map(|reference| match reference {
            Reference::Fact {
                participant: ParticipantRef::Named(participant),
                fact,
            } => Some(Need::Fact(participant.clone(), fact.clone())),
            Reference::Late {
                participant: ParticipantRef::Named(participant),
                fact,
            } => Some(Need::Late(participant.clone(), *fact)),
            _ => None,
        })
    }

    /// Substitutes early facts, failing on the first unavailable reference or if the result contains `{{`.
    pub fn resolve<'a>(&self, fact: impl Fn(&str, &str) -> Option<&'a str>) -> Result<String, String> {
        self.0.substitute(|written, reference| match reference {
            Reference::Fact {
                participant: ParticipantRef::Named(participant),
                fact: name,
            } => fact(participant, name)
                .map(|value| Some(value.to_owned()))
                .ok_or_else(|| format!("cannot resolve fact reference '{written}'")),
            Reference::Late { .. } => Err(format!(
                "cannot resolve late fact reference '{written}' before the participant runs"
            )),
            _ => Err(format!("cannot resolve '{written}'")),
        })
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
            let written = &text[..end + 2];
            segments.push(Segment::Reference {
                written: written.into(),
                reference: Reference::parse(written)?,
            });
            text = &text[end + 2..];
        }
        if !text.is_empty() {
            segments.push(Segment::Literal(text.into()));
        }
        Ok(Self { segments })
    }
}

impl FromStr for StepTemplate {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let template: Template = text.parse()?;
        template.check_step_field()?;
        Ok(Self(template))
    }
}

/// Reads a step field, accepting only facts and late facts of named participants.
impl<'de> Deserialize<'de> for StepTemplate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let template: Self = String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)?;
        Ok(template)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::{decode_fields, StepFields};

    #[derive(Debug, Deserialize)]
    struct Fields {
        path: StepTemplate,
    }

    #[test]
    fn canonicalizing_rewrites_the_holders_own_late_references_to_one_spelling() {
        for fact in [LateFact::Ip, LateFact::ContainerId, LateFact::PodUid] {
            for name in ["self", "subject"] {
                for spaces in ["", " ", "   "] {
                    let template: Template =
                        format!("before {{{{{spaces}{name}.late.{}{spaces}}}}} after", fact.name())
                            .parse()
                            .unwrap();
                    assert_eq!(
                        template.canonicalize_own_late("subject", |selected| selected == fact),
                        format!("before {} after", fact.substituted_reference())
                    );
                }
            }
        }
    }

    #[test]
    fn canonicalizing_keeps_other_participants_late_references_and_literals_as_written() {
        let text = "a/{{ other.late.ip }}/{{self.late.container_id}}/{{self.facts.file}}/{{ self.endpoints.api.url }}";
        let template: Template = text.parse().unwrap();
        assert_eq!(
            template.canonicalize_own_late("subject", |fact| fact == LateFact::Ip),
            text
        );
        let text = "{{subject.late.ip}}";
        assert_eq!(
            text.parse::<Template>()
                .unwrap()
                .canonicalize_own_late("subject", |_| false),
            text
        );
    }

    #[test]
    fn substitution_replaces_only_references_the_resolver_returns_and_keeps_the_rest_as_written() {
        let template: Template =
            "a/{{self.facts.file}}/{{  other.late.ip  }}/{{self.facts.file}}/{{ self.endpoints.api.url }}"
                .parse()
                .unwrap();
        assert_eq!(
            template
                .written_references()
                .map(|(written, _)| written)
                .collect::<Vec<_>>(),
            [
                "{{self.facts.file}}",
                "{{  other.late.ip  }}",
                "{{self.facts.file}}",
                "{{ self.endpoints.api.url }}"
            ]
        );
        let mut visited = Vec::new();
        let resolved = template
            .substitute(|written, reference| {
                visited.push(written.to_string());
                Ok(matches!(reference, Reference::Fact { .. }).then(|| "logs".into()))
            })
            .unwrap();
        assert_eq!(
            resolved,
            "a/logs/{{  other.late.ip  }}/logs/{{ self.endpoints.api.url }}"
        );
        assert_eq!(
            visited,
            [
                "{{self.facts.file}}",
                "{{  other.late.ip  }}",
                "{{self.facts.file}}",
                "{{ self.endpoints.api.url }}"
            ]
        );
        assert_eq!(
            template.substitute(|_, _| Err("unavailable".into())).unwrap_err(),
            "unavailable"
        );
    }

    #[test]
    fn substitution_cannot_introduce_a_template_reference() {
        for (text, value) in [
            ("{{self.facts.file}}", "{{ unresolved }}"),
            ("{{self.facts.file}}{", "{"),
            ("{{self.facts.file}}{{self.facts.file}}", "{"),
            ("{x{{self.facts.file}}", "{{"),
        ] {
            let template: Template = text.parse().unwrap();
            assert_eq!(
                template.substitute(|_, _| Ok(Some(value.into()))).unwrap_err(),
                "substitution would form a template delimiter ('{{')"
            );
        }
        let template: Template = "{{self.facts.file}}{{self.facts.empty}}{".parse().unwrap();
        assert_eq!(
            template
                .substitute(|_, reference| match reference {
                    Reference::Fact { fact, .. } => Ok(Some(if fact == "empty" { "" } else { "{" }.into())),
                    _ => unreachable!("the template references facts"),
                })
                .unwrap_err(),
            "substitution would form a template delimiter ('{{')"
        );
    }

    #[test]
    fn literal_templates_resolve_verbatim_including_backslashes_and_single_braces() {
        for text in ["", "/logs/output.log", r"C:\logs\{output}.log", "}text{", "text }}"] {
            let template: StepTemplate = text.parse().unwrap();
            assert!(template.0.references().next().is_none());
            assert_eq!(template.resolve(|_, _| None).unwrap(), text);
        }
    }

    #[test]
    fn every_reference_form_parses_with_or_without_inner_spaces() {
        let mut cases = Vec::new();
        for (name, participant) in [
            ("subject", ParticipantRef::Named("subject".into())),
            ("self", ParticipantRef::Holder),
        ] {
            cases.push((
                format!("{name}.facts.file"),
                Reference::Fact {
                    participant: participant.clone(),
                    fact: "file".into(),
                },
            ));
            for (fact, spelling) in [
                (LateFact::ContainerId, "container_id"),
                (LateFact::Ip, "ip"),
                (LateFact::PodUid, "pod_uid"),
            ] {
                cases.push((
                    format!("{name}.late.{spelling}"),
                    Reference::Late {
                        participant: participant.clone(),
                        fact,
                    },
                ));
            }
            for (part, spelling) in [
                (AddressPart::Url, "url"),
                (AddressPart::Host, "host"),
                (AddressPart::Port, "port"),
                (AddressPart::Path, "path"),
            ] {
                cases.push((
                    format!("{name}.endpoints.api.{spelling}"),
                    Reference::Endpoint {
                        participant: participant.clone(),
                        endpoint: "api".into(),
                        part,
                    },
                ));
            }
        }
        for (part, spelling) in [
            (AddressPart::Url, "url"),
            (AddressPart::Host, "host"),
            (AddressPart::Port, "port"),
            (AddressPart::Path, "path"),
        ] {
            cases.push((
                format!("self.connections.api.{spelling}"),
                Reference::Connection {
                    consumed: "api".into(),
                    part,
                },
            ));
        }
        for (body, expected) in cases {
            for spaces in ["", " ", "  "] {
                let template: Template = format!("{{{{{spaces}{body}{spaces}}}}}").parse().unwrap();
                let references = template.references().collect::<Vec<_>>();
                assert_eq!(references, [&expected]);
                assert_eq!(references[0].to_string(), format!("{{{{ {body} }}}}"));
            }
        }
    }

    #[test]
    fn literals_and_repeated_references_resolve_in_text_order() {
        let template: StepTemplate = "prefix/{{ other.facts.dir }}/{{subject.facts.file}}/{{ other.facts.dir }}.log"
            .parse()
            .unwrap();
        assert_eq!(
            template.needs().collect::<Vec<_>>(),
            [
                Need::Fact("other".into(), "dir".into()),
                Need::Fact("subject".into(), "file".into()),
                Need::Fact("other".into(), "dir".into())
            ]
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
    fn named_connection_references_fail_to_parse() {
        for reference in [
            "{{subject.connections.api.url}}",
            "{{  subject.connections.api.port  }}",
        ] {
            assert_eq!(reference.parse::<Template>().unwrap_err(), format!(
                "'{reference}': connections are referenced by their consumer, as '{{{{ self.connections.<endpoint>.<part> }}}}'"
            ));
        }
    }

    #[test]
    fn unknown_late_facts_and_address_parts_fail_to_parse_naming_the_choices() {
        for (reference, expected) in [
            ("{{subject.late.unknown}}", "a late fact is container_id, ip or pod_uid"),
            ("{{ self.late.IP }}", "a late fact is container_id, ip or pod_uid"),
            (
                "{{  subject.endpoints.api.unknown  }}",
                "an address part is url, host, port or path",
            ),
            (
                "{{self.endpoints.api.URL}}",
                "an address part is url, host, port or path",
            ),
            (
                "{{ self.connections.api.unknown }}",
                "an address part is url, host, port or path",
            ),
        ] {
            assert_eq!(
                reference.parse::<Template>().unwrap_err(),
                format!("'{reference}': {expected}")
            );
        }
    }

    #[test]
    fn malformed_references_fail_to_parse() {
        for body in [
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
            "subject.unknown.file",
            "subject.late",
            "subject.late.ip.extra",
            "subject.endpoints.api",
            "subject.endpoints.api.url.extra",
            "subject.endpoints..url",
            "self.connections.api",
            "self.connections.api.url.extra",
            "self.connections.api.",
        ] {
            let reference = format!("{{{{ {body} }}}}");
            assert_eq!(reference.parse::<Template>().unwrap_err(), format!(
                "'{reference}' is not a template reference; write '{{{{ <participant>.facts.<fact> }}}}', '{{{{ <participant>.late.<fact> }}}}', '{{{{ <participant>.endpoints.<endpoint>.<part> }}}}' or '{{{{ self.connections.<endpoint>.<part> }}}}'"
            ));
        }
    }

    #[test]
    fn step_fields_reject_endpoint_connection_and_self_references() {
        // Step-field checks retain the written spacing so errors identify the text in the case file.
        for (reference, expected) in [
            (
                "{{ self.facts.file }}",
                "a step has no 'self'; name the participant, as in '{{ <participant>.facts.<fact> }}'",
            ),
            (
                "{{self.facts.file}}",
                "a step has no 'self'; name the participant, as in '{{ <participant>.facts.<fact> }}'",
            ),
            (
                "{{ self.late.ip }}",
                "a step has no 'self'; name the participant, as in '{{ <participant>.late.ip }}'",
            ),
            (
                "{{self.late.container_id}}",
                "a step has no 'self'; name the participant, as in '{{ <participant>.late.container_id }}'",
            ),
            (
                "{{  self.late.pod_uid  }}",
                "a step has no 'self'; name the participant, as in '{{ <participant>.late.pod_uid }}'",
            ),
            (
                "{{ self.endpoints.api.url }}",
                "step fields accept only fact and late fact references to named participants",
            ),
            (
                "{{subject.endpoints.api.url}}",
                "step fields accept only fact and late fact references to named participants",
            ),
            (
                "{{  self.connections.api.url  }}",
                "step fields accept only fact and late fact references to named participants",
            ),
        ] {
            let fields: StepFields = serde_saphyr::from_str(&format!(
                "path: '{{{{ subject.facts.file }}}}/{reference}/{{{{ self.facts.other }}}}'"
            ))
            .unwrap();
            assert_eq!(
                decode_fields::<Fields>(&fields).unwrap_err(),
                format!("path: '{reference}': {expected}")
            );
        }
    }

    #[test]
    fn needs_report_fact_and_late_references_in_text_order_including_duplicates() {
        let fields: StepFields = serde_saphyr::from_str(
            "path: '{{ other.facts.dir }}/{{ subject.late.ip }}/{{ subject.facts.file }}/{{ other.late.container_id }}/{{ subject.late.ip }}/{{ other.facts.dir }}'"
        ).unwrap();
        let template = decode_fields::<Fields>(&fields).unwrap().path;
        assert_eq!(
            template.needs().collect::<Vec<_>>(),
            [
                Need::Fact("other".into(), "dir".into()),
                Need::Late("subject".into(), LateFact::Ip),
                Need::Fact("subject".into(), "file".into()),
                Need::Late("other".into(), LateFact::ContainerId),
                Need::Late("subject".into(), LateFact::Ip),
                Need::Fact("other".into(), "dir".into()),
            ]
        );
    }

    #[test]
    fn template_fields_require_yaml_strings_and_name_parse_errors() {
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
    fn resolution_substitutes_facts_and_rejects_late_facts_by_name() {
        for reference in ["subject.late.ip", "other.late.container_id", "other.late.pod_uid"] {
            let template: StepTemplate =
                format!("{{{{ subject.facts.file }}}}/{{{{{reference}}}}}/{{{{ other.late.ip }}}}")
                    .parse()
                    .unwrap();
            assert_eq!(
                template.resolve(|_, _| None).unwrap_err(),
                "cannot resolve fact reference '{{ subject.facts.file }}'"
            );
            assert_eq!(
                template.resolve(|_, _| Some("logs")).unwrap_err(),
                format!("cannot resolve late fact reference '{{{{{reference}}}}}' before the participant runs")
            );
        }
        let template: StepTemplate = "{{subject.facts.file}}".parse().unwrap();
        assert_eq!(template.resolve(|_, _| Some("logs")).unwrap(), "logs");
    }

    #[test]
    fn resolution_names_the_first_missing_reference() {
        for reference in ["{{ other.facts.b }}", "{{other.facts.b}}", "{{  other.facts.b  }}"] {
            let template: StepTemplate = format!("{{{{ subject.facts.a }}}}{reference}{{{{ subject.facts.c }}}}")
                .parse()
                .unwrap();
            assert_eq!(
                template
                    .resolve(|_, name| (name == "a").then_some("found"))
                    .unwrap_err(),
                format!("cannot resolve fact reference '{reference}'")
            );
        }
    }

    #[test]
    fn resolution_cannot_introduce_a_template_reference() {
        for (text, value) in [
            ("{{ subject.facts.file }}", "{{ unresolved }}"),
            ("{{ subject.facts.file }}{", "{"),
        ] {
            let template: StepTemplate = text.parse().unwrap();
            assert_eq!(
                template.resolve(|_, _| Some(value)).unwrap_err(),
                "substitution would form a template delimiter ('{{')"
            );
        }
    }
}
