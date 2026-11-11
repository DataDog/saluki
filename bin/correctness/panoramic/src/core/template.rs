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
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Template {
    text: String,
    parts: Vec<Part>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Part {
    Text(String),
    Reference(Reference),
}

/// The participant a reference names: the one that holds it, or another by name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Holder {
    /// `self`: the participant whose field or file holds the reference.
    Own,
    Participant(String),
}

/// A value the harness owns, by name.
// TODO: remove the dead_code allowance once references are resolved.
#[allow(dead_code)]
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
        let text = text.into();
        let mut parts = Vec::new();
        let mut rest = text.as_str();
        while let Some(start) = rest.find(OPEN) {
            if start > 0 {
                parts.push(Part::Text(rest[..start].to_string()));
            }
            let after = &rest[start + OPEN.len()..];
            let end = after
                .find(CLOSE)
                .ok_or_else(|| format!("'{}' has a '{}' that is never closed by '{}'", text, OPEN, CLOSE))?;
            let inner = &after[..end];
            if inner.contains(OPEN) {
                return Err(format!("'{}' has a '{}' inside a reference", text, OPEN));
            }
            parts.push(Part::Reference(Reference::parse(inner.trim())?));
            rest = &after[end + CLOSE.len()..];
        }
        if !rest.is_empty() {
            parts.push(Part::Text(rest.to_string()));
        }
        Ok(Self { text, parts })
    }

    /// The text as written.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The references the text holds, in order.
    // TODO: remove the dead_code allowance once references are resolved.
    #[allow(dead_code)]
    pub(crate) fn references(&self) -> impl Iterator<Item = &Reference> {
        self.parts.iter().filter_map(|part| match part {
            Part::Reference(reference) => Some(reference),
            Part::Text(_) => None,
        })
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
        f.write_str(&self.text)
    }
}

impl Serialize for Template {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.text)
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
