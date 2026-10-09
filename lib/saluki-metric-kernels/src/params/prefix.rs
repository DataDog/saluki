/// The namespace prepended to metric names, and the names it is not prepended to.
///
/// When `namespace` is not empty, every metric name is prefixed with it, followed by a `.` if the namespace does not
/// already end with one, unless the raw name starts with one of the `exclusions`. Exclusions are compared verbatim
/// against the name before prefixing. An empty namespace leaves every name unchanged.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NamespacePrefixParams {
    /// The namespace, as configured. Empty disables prefixing.
    pub namespace: String,

    /// Raw name prefixes of the names that are left as they are.
    pub exclusions: Vec<String>,
}
