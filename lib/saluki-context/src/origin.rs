//! Metric origin.

use std::{fmt, num::NonZeroU32, sync::Arc};

use indexmap::Equivalent;
use saluki_common::hash::hash_single_fast;
use serde::Deserialize;
use stringtheory::MetaString;
use tracing::warn;

use crate::tags::{Tag, TagVisitor, Tagged};

/// The cardinality of tags associated with the origin entity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(try_from = "String")]
pub enum OriginTagCardinality {
    /// No cardinality.
    ///
    /// This implies that no tags should be added to the metric based on its origin.
    None,

    /// Low cardinality.
    ///
    /// This generally covers tags which are static, or relatively slow to change, and generally results in a small
    /// number of unique values for the given tag key.
    Low,

    /// Orchestrator cardinality.
    ///
    /// This generally covers orchestrator-specific tags, such as Kubernetes pod UID, and lands somewhere between low
    /// and high cardinality.
    Orchestrator,

    /// High cardinality.
    ///
    /// This generally covers tags which frequently change and generally results in a large number of unique values for
    /// the given tag key.
    High,
}

impl TryFrom<&str> for OriginTagCardinality {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "none" => Ok(Self::None),
            "low" => Ok(Self::Low),
            "high" => Ok(Self::High),
            "orch" | "orchestrator" => Ok(Self::Orchestrator),
            other => Err(format!("unknown tag cardinality type '{}'", other)),
        }
    }
}

impl TryFrom<String> for OriginTagCardinality {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_from(value.as_str())
    }
}

impl fmt::Display for OriginTagCardinality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Low => write!(f, "low"),
            Self::Orchestrator => write!(f, "orchestrator"),
            Self::High => write!(f, "high"),
        }
    }
}

/// A raw representation of an origin.
///
/// Metrics contain metadata about their origin, in terms of the metric's _reason_ for existing: the metric was ingested
/// via DogStatsD, or was generated by an integration, and so on. However, there is also the concept of a metric
/// originating from a particular _entity_, such as a specific Kubernetes container. This relates directly to the
/// specific sender of the metric, which is used to enrich the metric with additional tags describing the origin entity.
///
/// The origin entity will generally be the process ID of the metric sender, or the container ID, both of which are then
/// generally mapped to the relevant information for the metric, such as the orchestrator-level tags for the
/// container/pod/deployment.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct RawOrigin<'a> {
    /// Process ID of the sender.
    process_id: Option<NonZeroU32>,

    /// Container ID of the sender.
    ///
    /// This will generally be the typical long hexadecimal string that is used by container runtimes like `containerd`,
    /// but may sometimes also be a different form, such as the container's cgroups inode.
    container_id: Option<&'a str>,

    /// Pod UID of the sender.
    ///
    /// This is generally only used in Kubernetes environments to uniquely identify the pod. UIDs are equivalent to UUIDs.
    pod_uid: Option<&'a str>,

    /// Desired cardinality of any tags associated with the entity.
    ///
    /// This controls the cardinality of the tags added to this metric when enriching based on the available entity IDs.
    cardinality: Option<OriginTagCardinality>,

    /// External Data of the sender.
    ///
    /// See [`ExternalData`] for more information.
    external_data: Option<RawExternalData<'a>>,
}

impl<'a> RawOrigin<'a> {
    /// Returns `true` if the origin information is empty.
    pub fn is_empty(&self) -> bool {
        self.process_id.is_none()
            && self.container_id.is_none()
            && self.pod_uid.is_none()
            && self.cardinality.is_none()
            && self.external_data.is_none()
    }

    /// Sets the process ID of the sender.
    ///
    /// Must be a non-zero value. If the value is zero, it is silently ignored.
    pub fn set_process_id(&mut self, process_id: u32) {
        self.process_id = NonZeroU32::new(process_id);
    }

    /// Returns the process ID of the sender.
    pub fn process_id(&self) -> Option<u32> {
        self.process_id.map(NonZeroU32::get)
    }

    /// Sets the container ID of the sender.
    pub fn set_container_id(&mut self, container_id: impl Into<Option<&'a str>>) {
        self.container_id = container_id.into();
    }

    /// Returns the container ID of the sender.
    pub fn container_id(&self) -> Option<&str> {
        self.container_id
    }

    /// Sets the pod UID of the sender.
    pub fn set_pod_uid(&mut self, pod_uid: impl Into<Option<&'a str>>) {
        self.pod_uid = pod_uid.into();
    }

    /// Returns the pod UID of the sender.
    pub fn pod_uid(&self) -> Option<&str> {
        self.pod_uid
    }

    /// Sets the desired cardinality of any tags associated with the entity.
    pub fn set_cardinality(&mut self, cardinality: impl Into<Option<OriginTagCardinality>>) {
        self.cardinality = cardinality.into();
    }

    /// Returns the desired cardinality of any tags associated with the entity.
    pub fn cardinality(&self) -> Option<OriginTagCardinality> {
        self.cardinality.as_ref().copied()
    }

    /// Sets the external data of the sender.
    pub fn set_external_data(&mut self, external_data: impl Into<Option<&'a str>>) {
        self.external_data = external_data.into().and_then(RawExternalData::try_from_str);
    }

    /// Returns the external data of the sender.
    pub fn external_data(&self) -> Option<&RawExternalData<'a>> {
        self.external_data.as_ref()
    }
}

impl fmt::Display for RawOrigin<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut has_written = false;

        write!(f, "RawOrigin(")?;

        if let Some(process_id) = self.process_id {
            write!(f, "process_id={}", process_id)?;
        }

        if let Some(container_id) = self.container_id {
            if has_written {
                write!(f, " ")?;
            } else {
                has_written = true;
            }
            write!(f, "container_id={}", container_id)?;
        }

        if let Some(pod_uid) = self.pod_uid {
            if has_written {
                write!(f, " ")?;
            } else {
                has_written = true;
            }
            write!(f, "pod_uid={}", pod_uid)?;
        }

        if let Some(cardinality) = self.cardinality {
            if has_written {
                write!(f, " ")?;
            } else {
                has_written = true;
            }
            write!(f, "cardinality={}", cardinality)?;
        }

        if let Some(external_data) = self.external_data.as_ref() {
            if has_written {
                write!(f, " ")?;
            }
            write!(f, "external_data={}", external_data)?;
        }

        write!(f, ")")
    }
}

/// A key that uniquely identifies the origin of a metric.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OriginKey(u64);

impl OriginKey {
    /// Creates a new `OriginKey` from the given opaque value by hashing it.
    pub fn from_opaque<O>(opaque: O) -> Self
    where
        O: std::hash::Hash,
    {
        Self(hash_single_fast(opaque))
    }
}

impl std::hash::Hash for OriginKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

#[derive(Clone)]
enum OriginTagsInner {
    Empty,
    Resolved {
        key: OriginKey,
        resolver: Arc<dyn OriginTagsResolver>,
    },
}

/// A set of tags associated with the origin of a metric.
#[derive(Clone)]
pub struct OriginTags {
    inner: OriginTagsInner,
}

impl OriginTags {
    pub(super) fn empty() -> Self {
        Self {
            inner: OriginTagsInner::Empty,
        }
    }

    pub(super) fn from_resolved(origin_key: OriginKey, resolver: Arc<dyn OriginTagsResolver>) -> Self {
        Self {
            inner: OriginTagsInner::Resolved {
                key: origin_key,
                resolver,
            },
        }
    }

    pub(super) fn key(&self) -> Option<OriginKey> {
        match self.inner {
            OriginTagsInner::Empty => None,
            OriginTagsInner::Resolved { key, .. } => Some(key),
        }
    }

    /// Returns the size of the origin tag set, in bytes.
    ///
    /// This includes the size of each individual tag.
    ///
    /// Additionally, the value returned by this method does not compensate for externalities such as whether or not
    /// tags are are inlined, interned, or heap allocated. This means that the value returned is essentially the
    /// worst-case usage, and should be used as a rough estimate.
    pub(super) fn size_of(&self) -> usize {
        let mut tags_size = 0;
        self.visit_tags(|tag| {
            tags_size += tag.len();
        });
        tags_size
    }
}

impl Tagged for OriginTags {
    fn visit_tags<F>(&self, mut visitor: F)
    where
        F: FnMut(&Tag),
    {
        match self.inner {
            OriginTagsInner::Empty => {}
            OriginTagsInner::Resolved { key, ref resolver } => resolver.visit_origin_tags(key, &mut visitor),
        }
    }
}

/// A resolver for mapping origins to their associated tags.
pub trait OriginTagsResolver: Send + Sync {
    /// Resolves the origin key for the given origin information.
    ///
    /// If the given origin information cannot be found/resolved, `None` is returned.
    fn resolve_origin_key(&self, origin: RawOrigin<'_>) -> Option<OriginKey>;

    /// Visits the tags associated with the given origin key.
    fn visit_origin_tags(&self, origin_key: OriginKey, visitor: &mut dyn TagVisitor);
}

impl<T> OriginTagsResolver for Arc<T>
where
    T: OriginTagsResolver,
{
    fn resolve_origin_key(&self, origin: RawOrigin<'_>) -> Option<OriginKey> {
        (**self).resolve_origin_key(origin)
    }

    fn visit_origin_tags(&self, origin_key: OriginKey, visitor: &mut dyn TagVisitor) {
        (**self).visit_origin_tags(origin_key, visitor)
    }
}

impl<T> OriginTagsResolver for Option<T>
where
    T: OriginTagsResolver,
{
    fn resolve_origin_key(&self, origin: RawOrigin<'_>) -> Option<OriginKey> {
        self.as_ref().and_then(|resolver| resolver.resolve_origin_key(origin))
    }

    fn visit_origin_tags(&self, origin_key: OriginKey, visitor: &mut dyn TagVisitor) {
        if let Some(resolver) = self.as_ref() {
            resolver.visit_origin_tags(origin_key, visitor)
        }
    }
}

/// External Data associated with an origin.
///
/// "External Data" is a concept that is used to aid origin detection of workloads running in Kubernetes environments
/// where introspection is not possible or may return incorrect information. Origin detection generally centers around
/// determining the container where a metric originates from, and then enriching the metric with tags that describe that
/// container, as well as the pod the container is running within, and so on. In some cases, the origin of a metric
/// cannot be detected from the outside (such as by using peer credentials over Unix Domain sockets) and cannot be
/// detected by the workload itself (such as when running in nested virtualization environments). In these cases, we
/// need a mechanism that allows passing the necessary information to the client, who then passes it on to us, so that
/// we can correctly resolve the origin.
///
/// "External Data" supports this by allowing for an external Kubernetes admission controller to attach specific
/// metadata -- pod UID and container name -- to application pods, which is then read and sent along with metrics. This
/// information is then used during origin detection in order to correlate the container ID of the origin, which is
/// sufficient to allow enriching the metric with container-specific tags.
///
/// # Format
///
/// An External Data string is a comma-separated list of key/value pairs, where each key represents a particular aspect
/// of the workload entity. The following keys are supported:
///
/// - `it-<true/false>`: A boolean value indicating whether the entity is an init container.
/// - `pu-<pod_uid>`: The pod UID associated with the entity.
/// - `cn-<container_name>`: The container name associated with the entity.
///
/// For parsing external data strings without allocating, see [`RawExternalData`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalData {
    pod_uid: MetaString,
    container_name: MetaString,
    init_container: bool,
}

impl ExternalData {
    /// Creates a new `ExternalData` instance.
    pub fn new(pod_uid: MetaString, container_name: MetaString, init_container: bool) -> Self {
        Self {
            pod_uid,
            container_name,
            init_container,
        }
    }

    /// Returns the pod UID.
    pub fn pod_uid(&self) -> &MetaString {
        &self.pod_uid
    }

    /// Returns the container name.
    pub fn container_name(&self) -> &MetaString {
        &self.container_name
    }

    /// Returns `true` if the entity is an init container.
    pub fn is_init_container(&self) -> bool {
        self.init_container
    }
}

impl std::hash::Hash for ExternalData {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (*self.pod_uid).hash(state);
        (*self.container_name).hash(state);
        self.init_container.hash(state);
    }
}

/// A borrowed representation of [`ExternalData`].
///
/// This can be used to parse external data strings without needing to allocate backing storage for any of the fields,
/// and can be used to look up map entries (such as when using `HashMap`) when the key is [`ExternalData`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawExternalData<'a> {
    pod_uid: &'a str,
    container_name: &'a str,
    init_container: bool,
}

impl<'a> RawExternalData<'a> {
    /// Creates a new `RawExternalData` from a raw string.
    ///
    /// If the external data is not valid, `None` is returned.
    pub fn try_from_str(raw: &'a str) -> Option<Self> {
        if raw.is_empty() {
            return None;
        }

        let mut data = Self {
            pod_uid: "",
            container_name: "",
            init_container: false,
        };

        let parts = raw.split(',');
        for part in parts {
            if part.len() < 4 {
                // All key/value pairs have a prefix of `xx-` where `xx` is some short code, so we basically can't have
                // any real key/value pair that's less than four characters overall.
                warn!("Parsed external data with invalid key/value pair: {}", part);
                continue;
            }

            let key = &part[0..3];
            let value = &part[3..];

            match key {
                "it-" => data.init_container = value.parse().unwrap_or(false),
                "pu-" => data.pod_uid = value,
                "cn-" => data.container_name = value,
                _ => {
                    // Unknown key, ignore.
                    warn!("Parsed external data with unknown key: {}", key);
                }
            }
        }

        Some(data)
    }
}

impl std::hash::Hash for RawExternalData<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.pod_uid.hash(state);
        self.container_name.hash(state);
        self.init_container.hash(state);
    }
}

impl fmt::Display for RawExternalData<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pu-{},cn-{},it-{}",
            self.pod_uid, self.container_name, self.init_container
        )
    }
}

impl Equivalent<ExternalData> for RawExternalData<'_> {
    fn equivalent(&self, other: &ExternalData) -> bool {
        self.pod_uid == &*other.pod_uid
            && self.container_name == &*other.container_name
            && self.init_container == other.init_container
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash as _, Hasher as _},
    };

    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn property_test_identical_hash_impls(pod_uid in "[a-z0-9]{1,64}", container_name in "[a-z0-9]{1,64}", init_container in any::<bool>()) {
            let external_data = ExternalData::new(pod_uid.clone().into(), container_name.clone().into(), init_container);
            let external_data_ref = RawExternalData {
                pod_uid: &pod_uid,
                container_name: &container_name,
                init_container,
            };

            let mut hasher = DefaultHasher::new();
            external_data.hash(&mut hasher);
            let external_data_hash = hasher.finish();

            let mut hasher = DefaultHasher::new();
            external_data_ref.hash(&mut hasher);
            let external_data_ref_hash = hasher.finish();

            assert_eq!(external_data_hash, external_data_ref_hash);
        }
    }
}
