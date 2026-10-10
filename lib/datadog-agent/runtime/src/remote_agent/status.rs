//! Status details reported to the Datadog Agent.

use std::collections::{hash_map::Entry, HashMap};

use datadog_protos::agent::status::v1::{GetStatusDetailsResponse, StatusSection};

/// A provider of status details for the Datadog Agent's status output.
///
/// Each time the Datadog Agent requests status details, the status service writes its built-in main section fields and
/// then calls every registered provider in registration order. Providers write their fields into the main section or
/// into named sections of their own.
///
/// The call happens while the status request is being answered, so it **SHOULD** only read state that is already at
/// hand, such as internal metrics, rather than performing I/O.
pub trait StatusSectionProvider: Send + Sync + 'static {
    /// Writes this provider's status fields into the given builder.
    fn write_status(&self, builder: &mut StatusBuilder);
}

/// Builder for the status details reported to the Datadog Agent.
///
/// Status details consist of a main section, which the Datadog Agent shows under the name of the subagent, and any
/// number of named sections.
pub struct StatusBuilder {
    main_section: StatusSection,
    named_sections: HashMap<String, StatusSection>,
}

impl StatusBuilder {
    /// Creates an empty `StatusBuilder`.
    pub fn new() -> Self {
        Self {
            main_section: StatusSection { fields: HashMap::new() },
            named_sections: HashMap::new(),
        }
    }

    /// Returns a writer for the main section.
    pub fn main_section(&mut self) -> StatusSectionWriter<'_> {
        StatusSectionWriter {
            section: &mut self.main_section,
        }
    }

    /// Returns a writer for the named section with the given name, creating the section if it doesn't exist yet.
    pub fn named_section<S: AsRef<str>>(&mut self, name: S) -> StatusSectionWriter<'_> {
        match self.named_sections.entry(name.as_ref().to_string()) {
            Entry::Occupied(entry) => StatusSectionWriter {
                section: entry.into_mut(),
            },
            Entry::Vacant(entry) => {
                let section = entry.insert(StatusSection { fields: HashMap::new() });
                StatusSectionWriter { section }
            }
        }
    }

    /// Consumes the builder and returns the status details in their wire form.
    pub fn into_response(self) -> GetStatusDetailsResponse {
        GetStatusDetailsResponse {
            main_section: Some(self.main_section),
            named_sections: self.named_sections,
        }
    }
}

impl Default for StatusBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Writer for the fields of a single status section.
pub struct StatusSectionWriter<'a> {
    section: &'a mut StatusSection,
}

impl StatusSectionWriter<'_> {
    /// Sets the field with the given name to the given value, replacing any previous value.
    pub fn set_field<S: AsRef<str>, V: AsRef<str>>(&mut self, name: S, value: V) -> &mut Self {
        self.section
            .fields
            .insert(name.as_ref().to_string(), value.as_ref().to_string());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_sections_accumulate_fields_across_writers() {
        let mut builder = StatusBuilder::new();
        builder.main_section().set_field("Version", "1.0.0");
        builder.named_section("Example").set_field("First", "1");
        builder
            .named_section("Example")
            .set_field("Second", "2")
            .set_field("First", "one");

        let response = builder.into_response();
        let main = response.main_section.expect("main section is always present");
        assert_eq!(main.fields.get("Version").map(String::as_str), Some("1.0.0"));

        let example = &response.named_sections["Example"];
        assert_eq!(example.fields.len(), 2);
        assert_eq!(example.fields.get("First").map(String::as_str), Some("one"));
        assert_eq!(example.fields.get("Second").map(String::as_str), Some("2"));
    }
}
