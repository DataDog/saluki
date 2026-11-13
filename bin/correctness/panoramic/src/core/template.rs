//! Text that may hold references to values the harness owns.
//!
//! A reference is written inside double braces and names an endpoint, a connection or an identity of a participant:
//!
//! ```text
//! reference = "{{" path "}}"
//! path      = holder ".endpoints." name "." ( "port" | "path" | "name" )
//!           | "self.connections." name "." ( "url" | "host" | "port" | "path" )
//!           | holder "." ( "dir" | "ip" | "container_id" | "pod_uid" )
//! holder    = "self" | participant-name
//! ```
//!
//! Every opening double brace starts a reference, and there is no escape. Text is parsed when the case loads, so a
//! malformed reference is an error before anything starts.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::case::{is_name, is_participant_name};

/// Text with the references it holds, parsed.
///
/// Serializes as the text as written, or, once a reference is filled in, as `{"written": ..., "resolved": ...}`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Template {
    written: String,
    /// The written text with each reference filled in so far, once one is.
    resolved: Option<String>,
    parts: Vec<Part>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Part {
    Text(String),
    Reference { reference: Reference, written: String },
}

/// The participant a reference names: the one that holds it, or another by name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Holder {
    /// `self`: the participant whose field or file holds the reference.
    Own,
    Participant(String),
}

/// A value the harness owns, by name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Reference {
    /// `<holder>.endpoints.<endpoint>.<part>`.
    Endpoint {
        holder: Holder,
        endpoint: String,
        part: EndpointPart,
    },
    /// `self.connections.<name>.<part>`.
    Connection { name: String, part: ConnectionPart },
    /// `<holder>.<part>`.
    Identity { holder: Holder, part: IdentityPart },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EndpointPart {
    Port,
    Path,
    Name,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectionPart {
    Url,
    Host,
    Port,
    Path,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdentityPart {
    Dir,
    Ip,
    ContainerId,
    PodUid,
}

const OPEN: &str = "{{";
const CLOSE: &str = "}}";

impl Template {
    /// Parses `text`.
    ///
    /// # Errors
    ///
    /// Returns an error if an opening double brace does not start a well-formed reference.
    pub(crate) fn parse(text: impl Into<String>) -> Result<Self, String> {
        let written = text.into();
        let mut parts = Vec::new();
        let mut rest = written.as_str();
        while let Some(start) = rest.find(OPEN) {
            if start > 0 {
                parts.push(Part::Text(rest[..start].to_string()));
            }
            let after = &rest[start + OPEN.len()..];
            let end = after
                .find(CLOSE)
                .ok_or_else(|| format!("'{}' has a '{}' that is never closed by '{}'", written, OPEN, CLOSE))?;
            let inner = &after[..end];
            if inner.contains(OPEN) {
                return Err(format!("'{}' has a '{}' inside a reference", written, OPEN));
            }
            parts.push(Part::Reference {
                reference: Reference::parse(inner.trim())?,
                written: rest[start..start + OPEN.len() + end + CLOSE.len()].to_string(),
            });
            rest = &after[end + CLOSE.len()..];
        }
        if !rest.is_empty() {
            parts.push(Part::Text(rest.to_string()));
        }
        Ok(Self {
            written,
            resolved: None,
            parts,
        })
    }

    /// The text, with each reference filled in so far.
    pub(crate) fn text(&self) -> &str {
        self.resolved.as_deref().unwrap_or(&self.written)
    }

    /// The references the text holds, in order.
    pub(crate) fn references(&self) -> impl Iterator<Item = &Reference> {
        self.parts.iter().filter_map(|part| match part {
            Part::Reference { reference, .. } => Some(reference),
            Part::Text(_) => None,
        })
    }

    /// Fills in each reference `value` knows and keeps the others as written.
    ///
    /// In a pattern matched as a regex, each value is escaped, so a path or an address matches itself.
    pub(crate) fn resolve<F>(&self, regex: bool, mut value: F) -> Self
    where
        F: FnMut(&Reference) -> Option<String>,
    {
        let mut parts: Vec<Part> = Vec::new();
        let mut text = String::new();
        let mut filled_any = false;
        for part in &self.parts {
            let filled = match part {
                Part::Text(literal) => literal.clone(),
                Part::Reference { reference, written } => match value(reference) {
                    Some(value) => {
                        filled_any = true;
                        if regex {
                            regex::escape(&value)
                        } else {
                            value
                        }
                    }
                    None => {
                        text.push_str(written);
                        parts.push(part.clone());
                        continue;
                    }
                },
            };
            text.push_str(&filled);
            match parts.last_mut() {
                Some(Part::Text(last)) => last.push_str(&filled),
                _ => parts.push(Part::Text(filled)),
            }
        }
        if !filled_any {
            return self.clone();
        }
        Self {
            written: self.written.clone(),
            resolved: Some(text),
            parts,
        }
    }
}

impl Reference {
    fn parse(path: &str) -> Result<Self, String> {
        let segments: Vec<&str> = path.split('.').collect();
        let reference = match segments.as_slice() {
            ["self", "connections", name, part] => {
                let part = match *part {
                    "url" => ConnectionPart::Url,
                    "host" => ConnectionPart::Host,
                    "port" => ConnectionPart::Port,
                    "path" => ConnectionPart::Path,
                    _ => return Err(not_a_reference(path)),
                };
                Self::Connection {
                    name: checked_name(path, name)?,
                    part,
                }
            }
            [holder, "endpoints", endpoint, part] => {
                let part = match *part {
                    "port" => EndpointPart::Port,
                    "path" => EndpointPart::Path,
                    "name" => EndpointPart::Name,
                    _ => return Err(not_a_reference(path)),
                };
                Self::Endpoint {
                    holder: Holder::parse(path, holder)?,
                    endpoint: checked_name(path, endpoint)?,
                    part,
                }
            }
            [holder, part] => {
                let part = match *part {
                    "dir" => IdentityPart::Dir,
                    "ip" => IdentityPart::Ip,
                    "container_id" => IdentityPart::ContainerId,
                    "pod_uid" => IdentityPart::PodUid,
                    _ => return Err(not_a_reference(path)),
                };
                Self::Identity {
                    holder: Holder::parse(path, holder)?,
                    part,
                }
            }
            _ => return Err(not_a_reference(path)),
        };
        Ok(reference)
    }
}

impl Holder {
    /// The participant this names, when `participant` holds the reference.
    pub(crate) fn resolve<'a>(&'a self, participant: &'a str) -> &'a str {
        match self {
            Self::Own => participant,
            Self::Participant(name) => name,
        }
    }

    /// Whether this names `participant` when `participant` holds the reference.
    pub(crate) fn names(&self, participant: &str) -> bool {
        self.resolve(participant) == participant
    }

    fn parse(path: &str, holder: &str) -> Result<Self, String> {
        if holder == "self" {
            Ok(Self::Own)
        } else if is_participant_name(holder) {
            Ok(Self::Participant(holder.to_string()))
        } else {
            Err(not_a_reference(path))
        }
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Endpoint { holder, endpoint, part } => {
                let part = match part {
                    EndpointPart::Port => "port",
                    EndpointPart::Path => "path",
                    EndpointPart::Name => "name",
                };
                write!(f, "{} {}.endpoints.{}.{} {}", OPEN, holder, endpoint, part, CLOSE)
            }
            Self::Connection { name, part } => {
                let part = match part {
                    ConnectionPart::Url => "url",
                    ConnectionPart::Host => "host",
                    ConnectionPart::Port => "port",
                    ConnectionPart::Path => "path",
                };
                write!(f, "{} self.connections.{}.{} {}", OPEN, name, part, CLOSE)
            }
            Self::Identity { holder, part } => {
                let part = match part {
                    IdentityPart::Dir => "dir",
                    IdentityPart::Ip => "ip",
                    IdentityPart::ContainerId => "container_id",
                    IdentityPart::PodUid => "pod_uid",
                };
                write!(f, "{} {}.{} {}", OPEN, holder, part, CLOSE)
            }
        }
    }
}

impl fmt::Display for Holder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Own => f.write_str("self"),
            Self::Participant(name) => f.write_str(name),
        }
    }
}

fn checked_name(path: &str, name: &str) -> Result<String, String> {
    if is_name(name) {
        Ok(name.to_string())
    } else {
        Err(not_a_reference(path))
    }
}

fn not_a_reference(path: &str) -> String {
    format!(
        "'{} {} {}' is not a reference: write '<participant>.endpoints.<name>.port', '.path' or '.name', \
         'self.connections.<name>.url', '.host', '.port' or '.path', or '<participant>.dir', '.ip', \
         '.container_id' or '.pod_uid', where <participant> may be 'self'",
        OPEN, path, CLOSE
    )
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text())
    }
}

impl Serialize for Template {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct Filled<'a> {
            written: &'a str,
            resolved: &'a str,
        }

        match &self.resolved {
            None => serializer.serialize_str(&self.written),
            Some(resolved) => Filled {
                written: &self.written,
                resolved,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Template {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = super::spec::strict_string(deserializer)?;
        Self::parse(text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_parse_by_the_grammar() {
        let template = Template::parse(
            "a {{ self.endpoints.api.port }}:{{adp.endpoints.dsd_uds.path}} {{ self.connections.target.url }} \
             {{ load.container_id }} {{self.dir}}",
        )
        .unwrap();
        let references: Vec<_> = template.references().cloned().collect();
        assert_eq!(
            references,
            [
                Reference::Endpoint {
                    holder: Holder::Own,
                    endpoint: "api".into(),
                    part: EndpointPart::Port
                },
                Reference::Endpoint {
                    holder: Holder::Participant("adp".into()),
                    endpoint: "dsd_uds".into(),
                    part: EndpointPart::Path
                },
                Reference::Connection {
                    name: "target".into(),
                    part: ConnectionPart::Url
                },
                Reference::Identity {
                    holder: Holder::Participant("load".into()),
                    part: IdentityPart::ContainerId
                },
                Reference::Identity {
                    holder: Holder::Own,
                    part: IdentityPart::Dir
                },
            ]
        );
        assert_eq!(Template::parse("no references }}").unwrap().references().count(), 0);
    }

    #[test]
    fn resolving_fills_what_is_known_escapes_it_in_a_regex_and_keeps_the_rest() {
        let template = Template::parse("{{ self.dir }}/a.sock {{self.ip}}").unwrap();
        let dir = |reference: &Reference| {
            matches!(
                reference,
                Reference::Identity {
                    part: IdentityPart::Dir,
                    ..
                }
            )
            .then(|| "/x.y".to_string())
        };
        let plain = template.resolve(false, dir);
        assert_eq!(plain.text(), "/x.y/a.sock {{self.ip}}");
        assert_eq!(plain.references().count(), 1);
        assert_eq!(template.resolve(true, dir).text(), "/x\\.y/a.sock {{self.ip}}");
    }

    #[test]
    fn every_open_brace_pair_starts_a_well_formed_reference() {
        for text in [
            "{{ self.dir",
            "{{ }}",
            "{{ self.endpoints.api }}",
            "{{ self.endpoints.api.url }}",
            "{{ adp.connections.target.url }}",
            "{{ Adp.dir }}",
            "{{ self.{{ self.dir }} }}",
            "{{ self.env.HOME }}",
        ] {
            assert!(Template::parse(text).is_err(), "'{}' should be rejected", text);
        }
    }
}
