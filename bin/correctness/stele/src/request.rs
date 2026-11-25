use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A single intake request, as captured by the fake intake.
///
/// The request carries the items that the intake decoded from its payload. The item type depends on the signal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IntakeRequest<T> {
    path: String,
    headers: BTreeMap<String, String>,
    items: T,
}

impl<T> IntakeRequest<T> {
    /// Creates a new `IntakeRequest`.
    ///
    /// Header names are lowercased. When a header appears more than once, its values are joined with `, `.
    pub fn new<'a, I>(path: impl Into<String>, headers: I, items: T) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut header_map = BTreeMap::<String, String>::new();
        for (name, value) in headers {
            header_map
                .entry(name.to_ascii_lowercase())
                .and_modify(|existing| {
                    existing.push_str(", ");
                    existing.push_str(value);
                })
                .or_insert_with(|| value.to_string());
        }

        Self {
            path: path.into(),
            headers: header_map,
            items,
        }
    }

    /// Returns the request path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the request headers, keyed by lowercased name.
    pub fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }

    /// Returns the value of the given header, matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }

    /// Returns the items carried by the request.
    pub fn items(&self) -> &T {
        &self.items
    }
}

#[cfg(test)]
mod tests {
    use super::IntakeRequest;

    #[test]
    fn headers_are_lowercased_and_repeated_values_joined() {
        let request = IntakeRequest::new(
            "/api/v2/series",
            [("User-Agent", "agent/1.0"), ("X-Tag", "a"), ("x-tag", "b")],
            Vec::<u32>::new(),
        );

        assert_eq!(request.path(), "/api/v2/series");
        assert_eq!(request.header("USER-AGENT"), Some("agent/1.0"));
        assert_eq!(request.header("x-tag"), Some("a, b"));
        assert_eq!(request.header("missing"), None);
        assert_eq!(request.headers().len(), 2);
    }
}
