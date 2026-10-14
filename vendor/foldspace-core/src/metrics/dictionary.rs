use std::{
    borrow::Borrow,
    collections::{BTreeSet, HashMap},
    fmt,
    hash::{BuildHasher, Hash, Hasher, RandomState},
    mem::size_of,
    time::Instant,
};

use siphasher::sip128::{Hasher128, SipHasher13};

use crate::proto::stateful::{
    metric_datum as datum, MetricDatum, MetricNameDefine, MetricOriginDefine, MetricResourceDefine,
    MetricResourceStringDefine, MetricSourceTypeNameDefine, MetricTagStringDefine,
    MetricTagsetDefine, MetricUnitDefine,
};

use super::{
    retention::{DefinitionKey, Retention},
    LogicalMetricSeries, MetricDictionaryEvictionConfig, MetricDictionaryStats, MetricOrigin,
    MetricResource, MetricTagSet,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SeriesReferences {
    pub(crate) name: u64,
    pub(crate) tags: u64,
    pub(crate) resources: u64,
    pub(crate) source_type_name: u64,
    pub(crate) origin: u64,
    pub(crate) unit: u64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct MetricDictionary {
    retention: Retention,
    names: HashMap<String, u64>,
    tag_strings: HashMap<String, u64>,
    source_type_names: HashMap<String, u64>,
    units: HashMap<String, u64>,
    resource_strings: HashMap<String, u64>,
    origins: HashMap<MetricOrigin, u64>,
    tagset_keys: TagsetKeys,
    // A hit is not checked against the tagset's strings: at random keys, two tag lists
    // sharing a 128-bit key is negligible.
    tagsets: HashMap<u64, HashMap<[u64; 2], u64>>,
    resources: HashMap<Vec<MetricResource>, u64>,
    next_name_id: u64,
    next_tag_string_id: u64,
    next_source_type_name_id: u64,
    next_unit_id: u64,
    next_resource_string_id: u64,
    next_origin_id: u64,
    next_tagset_id: u64,
    next_resource_id: u64,
}

/// SipHash keys for tagset lookup keys, drawn per dictionary.
#[derive(Clone, Copy)]
struct TagsetKeys {
    k0: u64,
    k1: u64,
}

impl Default for TagsetKeys {
    fn default() -> Self {
        let random = RandomState::new();
        Self {
            k0: random.hash_one(0u8),
            k1: random.hash_one(1u8),
        }
    }
}

impl fmt::Debug for TagsetKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TagsetKeys").finish_non_exhaustive()
    }
}

impl TagsetKeys {
    /// The lookup key of a tag list, in the given order.
    fn key<'a>(&self, tags: impl IntoIterator<Item = &'a str>) -> [u64; 2] {
        let mut hasher = SipHasher13::new_with_keys(self.k0, self.k1);
        for tag in tags {
            hasher.write(tag.as_bytes());
            // UTF-8 never contains 0xFF, so it ends each tag unambiguously.
            hasher.write_u8(0xff);
        }
        let hash = hasher.finish128();
        [hash.h1, hash.h2]
    }
}

impl MetricDictionary {
    /// A dictionary that `config` will evict, remembering value frequency across eviction.
    pub(crate) fn with_eviction(config: &MetricDictionaryEvictionConfig) -> Self {
        Self {
            retention: Retention::with_frequency_sketch(config),
            ..Self::default()
        }
    }

    pub(crate) fn intern_series(
        &mut self,
        series: &LogicalMetricSeries,
        definitions: &mut Vec<MetricDatum>,
    ) -> SeriesReferences {
        let references = SeriesReferences {
            name: self.intern_name(series.name(), definitions),
            tags: self.intern_tags(series.tags(), definitions),
            resources: self.intern_resources(series.resources(), definitions),
            source_type_name: series
                .source_type_name()
                .map_or(0, |value| self.intern_source_type_name(value, definitions)),
            origin: series
                .origin()
                .map_or(0, |origin| self.intern_origin(origin, definitions)),
            unit: series
                .unit()
                .map_or(0, |value| self.intern_unit(value, definitions)),
        };
        let roots = [
            (references.name, DefinitionKey::Name(references.name)),
            (references.tags, DefinitionKey::Tagset(references.tags)),
            (
                references.resources,
                DefinitionKey::Resource(references.resources),
            ),
            (
                references.source_type_name,
                DefinitionKey::SourceType(references.source_type_name),
            ),
            (references.origin, DefinitionKey::Origin(references.origin)),
            (references.unit, DefinitionKey::Unit(references.unit)),
        ];
        self.retention.touch(
            roots
                .into_iter()
                .filter(|(id, _)| *id != 0)
                .map(|(_, key)| key),
        );
        references
    }

    #[cfg(test)]
    fn apply_definitions(&mut self, definitions: &[MetricDatum], now: Instant) {
        self.begin_batch(now);
        for definition in definitions {
            let Some(data) = definition.data.as_ref() else {
                continue;
            };

            match data {
                datum::Data::MetricNameDefine(define) => {
                    self.next_name_id = self.next_name_id.max(define.id);
                    self.names.insert(define.value.clone(), define.id);
                }
                datum::Data::MetricTagStringDefine(define) => {
                    self.next_tag_string_id = self.next_tag_string_id.max(define.id);
                    self.tag_strings.insert(define.value.clone(), define.id);
                }
                datum::Data::MetricSourceTypeNameDefine(define) => {
                    self.next_source_type_name_id = self.next_source_type_name_id.max(define.id);
                    self.source_type_names
                        .insert(define.value.clone(), define.id);
                }
                datum::Data::MetricUnitDefine(define) => {
                    self.next_unit_id = self.next_unit_id.max(define.id);
                    self.units.insert(define.value.clone(), define.id);
                }
                datum::Data::MetricResourceStringDefine(define) => {
                    self.next_resource_string_id = self.next_resource_string_id.max(define.id);
                    self.resource_strings
                        .insert(define.value.clone(), define.id);
                }
                datum::Data::MetricResourceDefine(define) => {
                    let resources = define
                        .type_string_ids
                        .iter()
                        .zip(&define.name_string_ids)
                        .map(|(kind, name)| {
                            MetricResource::new(
                                self.resource_string(*kind)
                                    .expect("resource type definition must precede resource set"),
                                self.resource_string(*name)
                                    .expect("resource name definition must precede resource set"),
                            )
                        })
                        .collect();
                    self.next_resource_id = self.next_resource_id.max(define.id);
                    self.resources.insert(resources, define.id);
                }
                datum::Data::MetricOriginDefine(define) => {
                    let origin = MetricOrigin::new(define.product, define.category, define.service);
                    self.next_origin_id = self.next_origin_id.max(define.id);
                    self.origins.insert(origin, define.id);
                }
                datum::Data::MetricTagsetDefine(define) => {
                    let tags = self.tagset_keys.key(define.tag_string_ids.iter().map(|id| {
                        self.tag_string(*id)
                            .expect("tag string definition must precede tagset")
                    }));
                    self.next_tagset_id = self.next_tagset_id.max(define.id);
                    self.tagsets
                        .entry(define.prefix_id)
                        .or_default()
                        .insert(tags, define.id);
                }
                datum::Data::MetricSeriesBatch(_) => continue,
            }
            self.track_definition(definition, false);
        }
    }

    fn intern_name(&mut self, value: &str, definitions: &mut Vec<MetricDatum>) -> u64 {
        if value.is_empty() {
            return 0;
        }
        if let Some(id) = self.names.get(value) {
            return *id;
        }

        self.next_name_id += 1;
        let id = self.next_name_id;
        self.names.insert(value.to_string(), id);
        self.push_definition(
            MetricDatum {
                data: Some(datum::Data::MetricNameDefine(MetricNameDefine {
                    id,
                    value: value.to_string(),
                })),
            },
            definitions,
        );
        id
    }

    fn intern_tag_string(&mut self, value: &str, definitions: &mut Vec<MetricDatum>) -> u64 {
        if let Some(id) = self.tag_strings.get(value) {
            return *id;
        }

        self.next_tag_string_id += 1;
        let id = self.next_tag_string_id;
        self.tag_strings.insert(value.to_string(), id);
        self.push_definition(
            MetricDatum {
                data: Some(datum::Data::MetricTagStringDefine(MetricTagStringDefine {
                    id,
                    value: value.to_string(),
                })),
            },
            definitions,
        );
        id
    }

    fn intern_source_type_name(&mut self, value: &str, definitions: &mut Vec<MetricDatum>) -> u64 {
        if value.is_empty() {
            return 0;
        }
        if let Some(id) = self.source_type_names.get(value) {
            return *id;
        }

        self.next_source_type_name_id += 1;
        let id = self.next_source_type_name_id;
        self.source_type_names.insert(value.to_string(), id);
        self.push_definition(
            MetricDatum {
                data: Some(datum::Data::MetricSourceTypeNameDefine(
                    MetricSourceTypeNameDefine {
                        id,
                        value: value.to_string(),
                    },
                )),
            },
            definitions,
        );
        id
    }

    fn intern_unit(&mut self, value: &str, definitions: &mut Vec<MetricDatum>) -> u64 {
        if value.is_empty() {
            return 0;
        }
        if let Some(id) = self.units.get(value) {
            return *id;
        }

        self.next_unit_id += 1;
        let id = self.next_unit_id;
        self.units.insert(value.to_string(), id);
        self.push_definition(
            MetricDatum {
                data: Some(datum::Data::MetricUnitDefine(MetricUnitDefine {
                    id,
                    value: value.to_string(),
                })),
            },
            definitions,
        );
        id
    }

    fn intern_resource_string(&mut self, value: &str, definitions: &mut Vec<MetricDatum>) -> u64 {
        if let Some(id) = self.resource_strings.get(value) {
            return *id;
        }

        self.next_resource_string_id += 1;
        let id = self.next_resource_string_id;
        self.resource_strings.insert(value.to_string(), id);
        self.push_definition(
            MetricDatum {
                data: Some(datum::Data::MetricResourceStringDefine(
                    MetricResourceStringDefine {
                        id,
                        value: value.to_string(),
                    },
                )),
            },
            definitions,
        );
        id
    }

    fn intern_origin(&mut self, origin: MetricOrigin, definitions: &mut Vec<MetricDatum>) -> u64 {
        if let Some(id) = self.origins.get(&origin) {
            return *id;
        }

        self.next_origin_id += 1;
        let id = self.next_origin_id;
        self.origins.insert(origin, id);
        self.push_definition(
            MetricDatum {
                data: Some(datum::Data::MetricOriginDefine(MetricOriginDefine {
                    id,
                    product: origin.product,
                    category: origin.category,
                    service: origin.service,
                })),
            },
            definitions,
        );
        id
    }

    fn intern_resources(
        &mut self,
        resources: &[MetricResource],
        definitions: &mut Vec<MetricDatum>,
    ) -> u64 {
        if resources.is_empty() {
            return 0;
        }
        if let Some(id) = self.resources.get(resources) {
            return *id;
        }

        let mut type_string_ids = Vec::with_capacity(resources.len());
        let mut name_string_ids = Vec::with_capacity(resources.len());
        for resource in resources {
            type_string_ids.push(self.intern_resource_string(&resource.kind, definitions));
            name_string_ids.push(self.intern_resource_string(&resource.name, definitions));
        }

        self.next_resource_id += 1;
        let id = self.next_resource_id;
        self.resources.insert(resources.to_vec(), id);
        self.push_definition(
            MetricDatum {
                data: Some(datum::Data::MetricResourceDefine(MetricResourceDefine {
                    id,
                    type_string_ids,
                    name_string_ids,
                })),
            },
            definitions,
        );
        id
    }

    fn intern_tags(&mut self, tags: &MetricTagSet, definitions: &mut Vec<MetricDatum>) -> u64 {
        match (tags.prefix.is_empty(), tags.values.is_empty()) {
            (true, true) => 0,
            (true, false) => self.intern_tagset(0, &tags.values, definitions),
            (false, true) => self.intern_tagset(0, &tags.prefix, definitions),
            (false, false) => {
                let prefix_id = self.intern_tagset(0, &tags.prefix, definitions);
                self.intern_tagset(prefix_id, &tags.values, definitions)
            }
        }
    }

    fn intern_tagset(
        &mut self,
        prefix_id: u64,
        tags: &[String],
        definitions: &mut Vec<MetricDatum>,
    ) -> u64 {
        debug_assert!(tags.is_sorted());
        let key = self.tagset_keys.key(tags.iter().map(String::as_str));
        if let Some(id) = self
            .tagsets
            .get(&prefix_id)
            .and_then(|tagsets| tagsets.get(&key))
        {
            // Release builds with debug assertions (Saluki's `release` profile) must not pay for this scan.
            #[cfg(test)]
            assert!(
                self.tagset_strings(*id).eq(tags.iter().map(String::as_str)),
                "tagset {id} shares the lookup key of {tags:?}"
            );
            return *id;
        }

        let tag_string_ids = tags
            .iter()
            .map(|tag| self.intern_tag_string(tag, definitions))
            .collect();
        self.next_tagset_id += 1;
        let id = self.next_tagset_id;
        self.tagsets.entry(prefix_id).or_default().insert(key, id);
        self.push_definition(
            MetricDatum {
                data: Some(datum::Data::MetricTagsetDefine(MetricTagsetDefine {
                    id,
                    prefix_id,
                    tag_string_ids,
                })),
            },
            definitions,
        );
        id
    }

    pub(super) fn begin_batch(&mut self, now: Instant) {
        self.retention.begin_batch(now);
    }

    pub(super) fn references(&mut self) -> impl Iterator<Item = DefinitionKey> + '_ {
        self.retention.references()
    }

    pub(super) fn stats(&self) -> MetricDictionaryStats {
        self.retention.stats()
    }

    pub(super) fn evict(&mut self, config: &MetricDictionaryEvictionConfig) -> Vec<DefinitionKey> {
        let evicted = self.retention.evict(config);
        self.forget_lookups(&evicted);
        evicted.into_iter().map(|(key, _)| key).collect()
    }

    /// The wire definition in force under `key`, if it has not been evicted.
    pub(super) fn definition(&self, key: DefinitionKey) -> Option<&MetricDatum> {
        self.retention.definition(key)
    }

    /// The definitions in `references` that `sent` lacks, in dependency order.
    ///
    /// `references` must be in key order and retained, which holds for the closure of the
    /// batch most recently encoded: eviction cannot remove it until the next batch begins.
    /// Key order is a dependency order because primitives sort before composites and IDs
    /// within a kind are never reused, so a prefix tagset sorts before every tagset built on
    /// it.
    pub(super) fn definitions_for<'a>(
        &'a self,
        references: &'a [DefinitionKey],
        sent: &'a BTreeSet<DefinitionKey>,
    ) -> impl Iterator<Item = &'a MetricDatum> + 'a {
        debug_assert!(references.is_sorted(), "references are in dependency order");
        references
            .iter()
            .filter(|key| !sent.contains(key))
            .map(|key| {
                self.definition(*key)
                    .expect("a referenced definition is retained until the next batch")
            })
    }

    #[cfg(test)]
    pub(super) fn retained_keys(&self) -> Vec<DefinitionKey> {
        self.retention.keys().collect()
    }

    #[cfg(test)]
    pub(super) fn resident_keys(&self) -> Vec<DefinitionKey> {
        self.retention
            .keys()
            .filter(|key| !self.retention.is_transient(*key))
            .collect()
    }

    #[cfg(test)]
    pub(super) fn admit_all(&mut self) {
        self.retention.admit_all();
    }

    #[cfg(test)]
    pub(super) fn with_tagset_keys(self, k0: u64, k1: u64) -> Self {
        Self {
            tagset_keys: TagsetKeys { k0, k1 },
            ..self
        }
    }

    #[cfg(test)]
    fn tag_string(&self, id: u64) -> Option<&str> {
        match self
            .definition(DefinitionKey::TagString(id))?
            .data
            .as_ref()?
        {
            datum::Data::MetricTagStringDefine(define) => Some(&define.value),
            _ => None,
        }
    }

    #[cfg(test)]
    fn tagset_strings(&self, id: u64) -> impl Iterator<Item = &str> + '_ {
        let Some(datum::Data::MetricTagsetDefine(define)) = self
            .definition(DefinitionKey::Tagset(id))
            .and_then(|definition| definition.data.as_ref())
        else {
            panic!("tagset {id} is retained");
        };
        define
            .tag_string_ids
            .iter()
            .map(|id| self.tag_string(*id).expect("a tagset's strings outlive it"))
    }

    fn resource_string(&self, id: u64) -> Option<&str> {
        match self
            .definition(DefinitionKey::ResourceString(id))?
            .data
            .as_ref()?
        {
            datum::Data::MetricResourceStringDefine(define) => Some(&define.value),
            _ => None,
        }
    }

    /// Removes the lookup entries of evicted definitions. Strings a composite references are
    /// either still defined or evicted in the same pass, and a pass resolves each once.
    fn forget_lookups(&mut self, evicted: &[(DefinitionKey, MetricDatum)]) {
        let mut strings: HashMap<_, _> = evicted
            .iter()
            .filter_map(|(key, datum)| Some((*key, string_value(datum)?)))
            .collect();
        let retention = &self.retention;
        let tagset_keys = self.tagset_keys;
        let mut string = |key| {
            *strings.entry(key).or_insert_with(|| {
                retention
                    .definition(key)
                    .and_then(string_value)
                    .expect("a composite's strings outlive it")
            })
        };
        for (_, definition) in evicted {
            match definition
                .data
                .as_ref()
                .expect("dictionary datum has a definition")
            {
                datum::Data::MetricNameDefine(d) => {
                    forget(&mut self.names, d.value.as_str(), d.id);
                }
                datum::Data::MetricTagStringDefine(d) => {
                    forget(&mut self.tag_strings, d.value.as_str(), d.id);
                }
                datum::Data::MetricSourceTypeNameDefine(d) => {
                    forget(&mut self.source_type_names, d.value.as_str(), d.id);
                }
                datum::Data::MetricUnitDefine(d) => {
                    forget(&mut self.units, d.value.as_str(), d.id);
                }
                datum::Data::MetricResourceStringDefine(d) => {
                    forget(&mut self.resource_strings, d.value.as_str(), d.id);
                }
                datum::Data::MetricOriginDefine(d) => {
                    let origin = MetricOrigin::new(d.product, d.category, d.service);
                    forget(&mut self.origins, &origin, d.id);
                }
                datum::Data::MetricResourceDefine(d) => {
                    let resources: Vec<_> = d
                        .type_string_ids
                        .iter()
                        .zip(&d.name_string_ids)
                        .map(|(kind, name)| {
                            MetricResource::new(
                                string(DefinitionKey::ResourceString(*kind)),
                                string(DefinitionKey::ResourceString(*name)),
                            )
                        })
                        .collect();
                    forget(&mut self.resources, resources.as_slice(), d.id);
                }
                datum::Data::MetricTagsetDefine(d) => {
                    let Some(tagsets) = self.tagsets.get_mut(&d.prefix_id) else {
                        continue;
                    };
                    let key = tagset_keys.key(
                        d.tag_string_ids
                            .iter()
                            .map(|id| string(DefinitionKey::TagString(*id))),
                    );
                    forget(tagsets, &key, d.id);
                    if tagsets.is_empty() {
                        self.tagsets.remove(&d.prefix_id);
                    }
                }
                datum::Data::MetricSeriesBatch(_) => unreachable!("only definitions are tracked"),
            }
        }
    }

    #[cfg(test)]
    fn prune_lookups(&mut self) {
        let retained = &self.retention;
        self.names
            .retain(|_, id| retained.contains(DefinitionKey::Name(*id)));
        self.tag_strings
            .retain(|_, id| retained.contains(DefinitionKey::TagString(*id)));
        self.source_type_names
            .retain(|_, id| retained.contains(DefinitionKey::SourceType(*id)));
        self.units
            .retain(|_, id| retained.contains(DefinitionKey::Unit(*id)));
        self.resource_strings
            .retain(|_, id| retained.contains(DefinitionKey::ResourceString(*id)));
        self.origins
            .retain(|_, id| retained.contains(DefinitionKey::Origin(*id)));
        self.resources
            .retain(|_, id| retained.contains(DefinitionKey::Resource(*id)));
        self.tagsets.retain(|_, sets| {
            sets.retain(|_, id| retained.contains(DefinitionKey::Tagset(*id)));
            !sets.is_empty()
        });
    }

    fn track_definition(&mut self, definition: &MetricDatum, contended: bool) {
        // Charge the keys owned by the value-to-ID lookup maps; retention charges the datum.
        let lookup_bytes = match definition.data.as_ref().unwrap() {
            datum::Data::MetricNameDefine(d) => d.value.len(),
            datum::Data::MetricTagStringDefine(d) => d.value.len(),
            datum::Data::MetricSourceTypeNameDefine(d) => d.value.len(),
            datum::Data::MetricUnitDefine(d) => d.value.len(),
            datum::Data::MetricResourceStringDefine(d) => d.value.len(),
            datum::Data::MetricOriginDefine(_) => size_of::<MetricOrigin>(),
            datum::Data::MetricTagsetDefine(_) => size_of::<[u64; 2]>(),
            datum::Data::MetricResourceDefine(d) => d
                .type_string_ids
                .iter()
                .chain(&d.name_string_ids)
                .map(|id| {
                    size_of::<String>()
                        + self
                            .resource_string(*id)
                            .expect("resource string retained")
                            .len()
                })
                .sum(),
            datum::Data::MetricSeriesBatch(_) => unreachable!("only definitions are tracked"),
        };
        if contended {
            self.retention.insert(definition, lookup_bytes);
        } else {
            self.retention.seed(definition, lookup_bytes);
        }
    }

    fn push_definition(&mut self, definition: MetricDatum, definitions: &mut Vec<MetricDatum>) {
        self.track_definition(&definition, true);
        definitions.push(definition);
    }
}

fn string_value(definition: &MetricDatum) -> Option<&str> {
    match definition.data.as_ref()? {
        datum::Data::MetricTagStringDefine(d) => Some(&d.value),
        datum::Data::MetricResourceStringDefine(d) => Some(&d.value),
        _ => None,
    }
}

/// Removes `value` from `lookup` if it still maps to `id`.
fn forget<K, Q>(lookup: &mut HashMap<K, u64>, value: &Q, id: u64)
where
    K: Borrow<Q> + Eq + Hash,
    Q: Eq + Hash + ?Sized,
{
    if let Some((key, current)) = lookup.remove_entry(value) {
        if current != id {
            lookup.insert(key, current);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::Duration};

    use crate::proto::stateful::metric_datum as datum;

    use super::*;
    use crate::{LogicalMetricSeries, MetricPoint, MetricSeriesType};

    fn tagged(name: &str, prefix: &[&str], values: &[&str]) -> LogicalMetricSeries {
        LogicalMetricSeries::new(
            name,
            MetricSeriesType::Gauge,
            vec![MetricPoint::new(1, 1.0)],
        )
        .with_tags(MetricTagSet {
            prefix: prefix.iter().map(ToString::to_string).collect(),
            values: values.iter().map(ToString::to_string).collect(),
        })
    }

    /// Interns one series as its own batch and returns the batch's reference closure.
    fn intern(
        dictionary: &mut MetricDictionary,
        series: &LogicalMetricSeries,
    ) -> Vec<DefinitionKey> {
        dictionary.begin_batch(Instant::now());
        dictionary.intern_series(series, &mut Vec::new());
        dictionary.references().collect()
    }

    fn evict_everything() -> MetricDictionaryEvictionConfig {
        MetricDictionaryEvictionConfig {
            max_item_count: 0,
            high_watermark: 1.0,
            low_watermark: 0.0,
            grace_period: Duration::ZERO,
            stale_after: Duration::ZERO,
            ..MetricDictionaryEvictionConfig::default()
        }
    }

    fn lookups(dictionary: &MetricDictionary) -> impl PartialEq + std::fmt::Debug + '_ {
        (
            &dictionary.names,
            &dictionary.tag_strings,
            &dictionary.source_type_names,
            &dictionary.units,
            &dictionary.resource_strings,
            &dictionary.origins,
            &dictionary.tagsets,
            &dictionary.resources,
        )
    }

    /// Evicts and asserts the lookups match what a full prune after the same eviction leaves.
    fn evict_checked(
        dictionary: &mut MetricDictionary,
        config: &MetricDictionaryEvictionConfig,
    ) -> Vec<DefinitionKey> {
        let mut oracle = dictionary.clone();
        oracle.retention.evict(config);
        oracle.prune_lookups();
        let evicted = dictionary.evict(config);
        assert_eq!(dictionary.retained_keys(), oracle.retained_keys());
        assert_eq!(lookups(dictionary), lookups(&oracle));
        evicted
    }

    /// Every lookup entry belongs to a defined value and every defined value has its lookup
    /// entry: replaying the definitions rebuilds the same lookups.
    fn assert_lookups_match_retention(dictionary: &MetricDictionary) {
        let definitions: Vec<_> = dictionary
            .retained_keys()
            .into_iter()
            .map(|key| dictionary.definition(key).unwrap().clone())
            .collect();
        let mut rebuilt = MetricDictionary {
            tagset_keys: dictionary.tagset_keys,
            ..MetricDictionary::default()
        };
        rebuilt.apply_definitions(&definitions, Instant::now());
        assert_eq!(lookups(dictionary), lookups(&rebuilt));
    }

    fn random_series(next: &mut impl FnMut(u64) -> u64) -> LogicalMetricSeries {
        let shared = |i| format!("shared:{i}");
        let prefix = (0..next(3))
            .map(|_| match next(3) {
                0 => shared(next(4)),
                _ => format!("env:{}", next(5)),
            })
            .collect();
        let values = (0..next(4))
            .map(|_| match next(4) {
                0 => shared(next(4)),
                _ => format!("pod:{}", next(25)),
            })
            .collect();
        let resources = (0..next(3))
            .map(|_| {
                MetricResource::new(
                    ["host", "device", "shared"][next(3) as usize],
                    format!("r{}", next(12)),
                )
            })
            .collect();
        let mut series = LogicalMetricSeries::new(
            format!("metric.{}", next(30)),
            MetricSeriesType::Gauge,
            vec![MetricPoint::new(1, 1.0)],
        )
        .with_tags(MetricTagSet { prefix, values })
        .with_resources(resources);
        if next(2) == 0 {
            series = series.with_unit(["", "byte", "second", "request"][next(4) as usize]);
        }
        if next(2) == 0 {
            series = series.with_source_type_name(format!("source-{}", next(4)));
        }
        if next(2) == 0 {
            series = series.with_origin(MetricOrigin::new(
                next(3) as i32,
                next(2) as i32,
                next(2) as i32,
            ));
        }
        series
    }

    /// Targeted deletion leaves exactly what a full prune would, across shared strings,
    /// prefix tagsets, transient definitions, and every eviction trigger, and evicted values
    /// return under fresh IDs.
    #[test]
    fn targeted_lookup_deletion_matches_a_full_prune_under_churn() {
        for sketched in [false, true] {
            let start = Instant::now();
            let mut random = 0x9e37_79b9_7f4a_7c15u64;
            let mut next = move |bound: u64| {
                random = random
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (random >> 33) % bound
            };
            let mut dictionary = if sketched {
                MetricDictionary::with_eviction(&MetricDictionaryEvictionConfig {
                    max_item_count: 120,
                    ..evict_everything()
                })
            } else {
                MetricDictionary::default()
            };
            let mut evicted_ever = BTreeSet::new();
            let mut removing_passes = 0;
            let mut transient = 0;
            for round in 0..400u64 {
                dictionary.begin_batch(start + Duration::from_secs(round));
                for _ in 0..next(12) {
                    let series = random_series(&mut next);
                    let mut definitions = Vec::new();
                    dictionary.intern_series(&series, &mut definitions);
                    assert!(definitions
                        .iter()
                        .all(|datum| !evicted_ever.contains(&DefinitionKey::of(datum))));
                }
                transient += dictionary.stats().transient_entries;
                let config = MetricDictionaryEvictionConfig {
                    max_item_count: [0, 15, 40, 120, 100_000][(round % 5) as usize],
                    max_memory_bytes: if round % 4 == 0 { 6_000 } else { i64::MAX },
                    high_watermark: 1.0,
                    low_watermark: [0.0, 0.5, 0.9][(round / 3 % 3) as usize],
                    grace_period: Duration::from_secs([0, 2, 5][(round / 17 % 3) as usize]),
                    stale_after: Duration::from_secs([0, 4, 9][(round / 11 % 3) as usize]),
                    age_decay_factor: [0.0, 0.5, 2.0][(round / 7 % 3) as usize],
                };
                let evicted = evict_checked(&mut dictionary, &config);
                assert_lookups_match_retention(&dictionary);
                removing_passes += usize::from(!evicted.is_empty());
                evicted_ever.extend(evicted);
            }
            assert!(removing_passes > 100, "{removing_passes} removing passes");
            assert_eq!(
                transient > 100,
                sketched,
                "{transient} transient definitions"
            );

            dictionary.begin_batch(start + Duration::from_secs(10_000));
            evict_checked(&mut dictionary, &evict_everything());
            assert!(dictionary.retained_keys().is_empty());
            let empty = MetricDictionary::default();
            assert_eq!(lookups(&dictionary), lookups(&empty));
        }
    }

    /// Reseeded lookups are keyed by the definition's own tag order, which need not be
    /// sorted, and eviction forgets them as they were inserted.
    #[test]
    fn reseeded_lookups_are_forgotten_as_they_were_inserted() {
        let start = Instant::now();
        let data = |data| MetricDatum { data: Some(data) };
        let tag_string = |id, value: &str| {
            data(datum::Data::MetricTagStringDefine(MetricTagStringDefine {
                id,
                value: value.to_string(),
            }))
        };
        let tagset = |id, prefix_id, tag_string_ids| {
            data(datum::Data::MetricTagsetDefine(MetricTagsetDefine {
                id,
                prefix_id,
                tag_string_ids,
            }))
        };
        let resource_string = |id, value: &str| {
            data(datum::Data::MetricResourceStringDefine(
                MetricResourceStringDefine {
                    id,
                    value: value.to_string(),
                },
            ))
        };
        let mut dictionary = MetricDictionary::default();
        dictionary.apply_definitions(
            &[
                data(datum::Data::MetricNameDefine(MetricNameDefine {
                    id: 1,
                    value: "requests".to_string(),
                })),
                tag_string(1, "service:web"),
                tag_string(2, "env:prod"),
                tagset(1, 0, vec![1, 2]),
                tagset(2, 1, vec![2, 2]),
                tagset(3, 0, vec![2]),
                resource_string(1, "host"),
                resource_string(2, "web-1"),
                data(datum::Data::MetricResourceDefine(MetricResourceDefine {
                    id: 1,
                    type_string_ids: vec![1],
                    name_string_ids: vec![2],
                })),
                data(datum::Data::MetricOriginDefine(MetricOriginDefine {
                    id: 1,
                    product: 1,
                    category: 2,
                    service: 3,
                })),
            ],
            start,
        );
        let unsorted = dictionary.tagset_keys.key(["service:web", "env:prod"]);
        assert_eq!(dictionary.tagsets[&0].get(&unsorted), Some(&1));
        assert_lookups_match_retention(&dictionary);

        dictionary.begin_batch(start + Duration::from_secs(1));
        let mut definitions = Vec::new();
        dictionary.intern_series(&tagged("requests", &[], &["env:prod"]), &mut definitions);
        assert!(
            definitions.is_empty(),
            "the reseeded sorted tagset is reused"
        );
        let evicted = evict_checked(&mut dictionary, &evict_everything());
        assert!(evicted.contains(&DefinitionKey::Tagset(1)));
        assert!(evicted.contains(&DefinitionKey::TagString(1)));
        assert!(!dictionary.tagsets.contains_key(&1));
        assert_lookups_match_retention(&dictionary);

        dictionary.begin_batch(start + Duration::from_secs(2));
        evict_checked(&mut dictionary, &evict_everything());
        let empty = MetricDictionary::default();
        assert_eq!(lookups(&dictionary), lookups(&empty));
    }

    /// Tag boundaries, multiplicity, and the prefix are part of a tagset's identity.
    #[test]
    fn distinct_tag_lists_get_distinct_tagsets() {
        let lists: &[&[&str]] = &[
            &["ab", "c"],
            &["a", "bc"],
            &["abc"],
            &["", "abc"],
            &["a", ""],
            &["a"],
            &[""],
            &[],
            &["a", "a"],
        ];
        let mut dictionary = MetricDictionary::default();
        dictionary.begin_batch(Instant::now());
        let mut intern = |prefix_id, tags: &[&str]| {
            let mut tags: Vec<_> = tags.iter().map(ToString::to_string).collect();
            tags.sort();
            dictionary.intern_tagset(prefix_id, &tags, &mut Vec::new())
        };
        let ids: Vec<_> = lists.iter().map(|tags| intern(0, tags)).collect();
        let prefixed: Vec<_> = ids.iter().map(|prefix| intern(*prefix, &["a"])).collect();
        let distinct: BTreeSet<_> = ids.iter().chain(&prefixed).collect();
        assert_eq!(distinct.len(), 2 * lists.len());
        assert!(!distinct.contains(&0));
        assert_eq!(
            lists.iter().map(|tags| intern(0, tags)).collect::<Vec<_>>(),
            ids
        );
        assert_eq!(
            ids.iter()
                .map(|prefix| intern(*prefix, &["a"]))
                .collect::<Vec<_>>(),
            prefixed
        );
    }

    /// Tagset lookups by prefix and tag list in definition order, read back through each
    /// entry's retained definition, which must still hash to the key it is filed under.
    fn tagset_lookups(dictionary: &MetricDictionary) -> BTreeMap<(u64, Vec<String>), u64> {
        let mut view = BTreeMap::new();
        for (prefix_id, tagsets) in &dictionary.tagsets {
            for (key, id) in tagsets {
                let Some(datum::Data::MetricTagsetDefine(define)) = dictionary
                    .definition(DefinitionKey::Tagset(*id))
                    .and_then(|definition| definition.data.as_ref())
                else {
                    panic!("lookup of evicted tagset {id}");
                };
                assert_eq!(define.prefix_id, *prefix_id);
                let tags: Vec<_> = dictionary.tagset_strings(*id).map(String::from).collect();
                assert_eq!(
                    dictionary.tagset_keys.key(tags.iter().map(String::as_str)),
                    *key,
                    "tagset {id} is filed under another key"
                );
                view.insert((*prefix_id, tags), *id);
            }
        }
        view
    }

    type Shadow = BTreeMap<(u64, Vec<String>), u64>;

    /// Interns `series` and checks the tagset IDs against a string-keyed shadow; returns
    /// whether its tagsets were all found.
    fn intern_shadowed(
        dictionary: &mut MetricDictionary,
        shadow: &mut Shadow,
        series: &LogicalMetricSeries,
    ) -> bool {
        let mut definitions = Vec::new();
        let references = dictionary.intern_series(series, &mut definitions);
        let mut found = true;
        for definition in &definitions {
            if let Some(datum::Data::MetricTagsetDefine(define)) = &definition.data {
                let tags = dictionary.tagset_strings(define.id).map(String::from);
                let previous = shadow.insert((define.prefix_id, tags.collect()), define.id);
                assert_eq!(previous, None, "tagset {} defined twice", define.id);
                found = false;
            }
        }
        let tags = series.tags();
        let lookup = |prefix_id, tags: &[String]| shadow[&(prefix_id, tags.to_vec())];
        let expected = match (tags.prefix.is_empty(), tags.values.is_empty()) {
            (true, true) => 0,
            (true, false) => lookup(0, &tags.values),
            (false, true) => lookup(0, &tags.prefix),
            (false, false) => lookup(lookup(0, &tags.prefix), &tags.values),
        };
        assert_eq!(references.tags, expected);
        assert_eq!(tagset_lookups(dictionary), *shadow);
        found && expected != 0
    }

    /// Applies `definitions` and the shadow's model of them: a tagset not already retained
    /// takes its tag list's lookup, read in definition order.
    fn apply_shadowed(
        dictionary: &mut MetricDictionary,
        shadow: &mut Shadow,
        definitions: &[MetricDatum],
        now: Instant,
    ) {
        let mut applied: BTreeSet<_> = dictionary.retained_keys().into_iter().collect();
        dictionary.apply_definitions(definitions, now);
        for definition in definitions {
            if let Some(datum::Data::MetricTagsetDefine(define)) = &definition.data {
                if applied.insert(DefinitionKey::Tagset(define.id)) {
                    let tags = dictionary.tagset_strings(define.id).map(String::from);
                    shadow.insert((define.prefix_id, tags.collect()), define.id);
                }
            }
        }
        assert_eq!(tagset_lookups(dictionary), *shadow);
    }

    /// Evicts and drops evicted tagsets from the shadow; returns how many were evicted.
    fn evict_shadowed(
        dictionary: &mut MetricDictionary,
        shadow: &mut Shadow,
        config: &MetricDictionaryEvictionConfig,
    ) -> usize {
        let evicted: BTreeSet<_> = dictionary
            .evict(config)
            .into_iter()
            .filter_map(|key| match key {
                DefinitionKey::Tagset(id) => Some(id),
                _ => None,
            })
            .collect();
        shadow.retain(|_, id| !evicted.contains(id));
        assert_eq!(tagset_lookups(dictionary), *shadow);
        evicted.len()
    }

    /// Hashed tagset lookups find exactly what lookups keyed by the tag strings would,
    /// through interning, eviction with admission transients, returns under fresh IDs,
    /// reseeds that are unsorted or repeat tag strings, and replay onto a dictionary with
    /// other keys.
    #[test]
    fn hashed_tagset_lookups_agree_with_string_lookups_under_churn() {
        let long = "x".repeat(70);
        let alphabet = [
            "ab",
            "c",
            "a",
            "bc",
            "",
            "abc",
            "env:prod",
            "env:prod\0",
            "\u{ff}",
            long.as_str(),
            "team:a",
            "team:b",
        ];
        let tag_list = |next: &mut dyn FnMut(u64) -> u64| -> Vec<String> {
            (0..next(4))
                .map(|_| alphabet[next(alphabet.len() as u64) as usize].to_string())
                .collect()
        };
        let data = |data| MetricDatum { data: Some(data) };
        let (mut found, mut evicted, mut transient, mut reseeds) = (0, 0, 0, 0);
        for seed in 0..20u64 {
            let mut random = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
            let mut next = move |bound: u64| {
                random = random
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (random >> 33) % bound
            };
            let config = MetricDictionaryEvictionConfig {
                max_item_count: [30, 50, 80][(seed % 3) as usize],
                low_watermark: 0.5,
                ..evict_everything()
            };
            let mut dictionary = MetricDictionary::with_eviction(&config);
            let mut shadow = Shadow::new();
            let start = Instant::now();
            for round in 0..200u64 {
                let now = start + Duration::from_secs(round);
                dictionary.begin_batch(now);
                for _ in 0..1 + next(4) {
                    let series = LogicalMetricSeries::new(
                        format!("m{}", next(4)),
                        MetricSeriesType::Gauge,
                        Vec::new(),
                    )
                    .with_tags(MetricTagSet {
                        prefix: tag_list(&mut next),
                        values: tag_list(&mut next),
                    });
                    found += usize::from(intern_shadowed(&mut dictionary, &mut shadow, &series));
                }
                transient += dictionary.stats().transient_entries;
                if next(3) == 0 {
                    dictionary.begin_batch(now);
                }
                evicted += evict_shadowed(&mut dictionary, &mut shadow, &config);
                if round % 7 != 0 {
                    continue;
                }

                let retained = dictionary.retained_keys();
                let tag_ids: Vec<_> = retained
                    .iter()
                    .filter_map(|key| match key {
                        DefinitionKey::TagString(id) => Some(*id),
                        _ => None,
                    })
                    .collect();
                let tagset_ids: Vec<_> = retained
                    .iter()
                    .filter_map(|key| match key {
                        DefinitionKey::Tagset(id) => Some(*id),
                        _ => None,
                    })
                    .collect();
                let fresh_tag = dictionary.next_tag_string_id + 1 + next(3);
                let mut foreign = vec![data(datum::Data::MetricTagStringDefine(
                    MetricTagStringDefine {
                        id: fresh_tag,
                        value: alphabet[next(alphabet.len() as u64) as usize].to_string(),
                    },
                ))];
                let mut tag_string_ids: Vec<_> = tag_ids
                    .iter()
                    .copied()
                    .filter(|_| next(2) == 0)
                    .take(3)
                    .chain([fresh_tag])
                    .collect();
                if next(2) == 0 {
                    tag_string_ids.reverse();
                }
                if next(2) == 0 {
                    tag_string_ids.push(tag_string_ids[0]);
                }
                let fresh_tagset = dictionary.next_tagset_id + 1 + next(3);
                let prefix_id = match next(2) {
                    0 => tagset_ids.first().copied().unwrap_or(0),
                    _ => 0,
                };
                let tagset = |id, prefix_id, tag_string_ids| {
                    data(datum::Data::MetricTagsetDefine(MetricTagsetDefine {
                        id,
                        prefix_id,
                        tag_string_ids,
                    }))
                };
                foreign.push(tagset(fresh_tagset, prefix_id, tag_string_ids));
                apply_shadowed(&mut dictionary, &mut shadow, &foreign, now);
                reseeds += 1;

                let definitions: Vec<_> = dictionary
                    .retained_keys()
                    .into_iter()
                    .map(|key| dictionary.definition(key).unwrap().clone())
                    .collect();
                let mut replayed = MetricDictionary::with_eviction(&config);
                let mut replayed_shadow = Shadow::new();
                apply_shadowed(&mut replayed, &mut replayed_shadow, &definitions, now);
                replayed.begin_batch(now);
                for _ in 0..4 {
                    let series = LogicalMetricSeries::new("m", MetricSeriesType::Gauge, Vec::new())
                        .with_tags(MetricTagSet {
                            prefix: tag_list(&mut next),
                            values: tag_list(&mut next),
                        });
                    intern_shadowed(&mut replayed, &mut replayed_shadow, &series);
                }
            }
        }
        assert!(
            found > 500 && evicted > 1000 && transient > 100 && reseeds > 500,
            "found {found}, evicted {evicted}, transient {transient}, reseeds {reseeds}"
        );
    }

    /// A tagset's estimate charges its fixed-size lookup key, not its tag strings, which are
    /// charged once to their own definitions.
    #[test]
    fn tagset_estimates_do_not_depend_on_tag_lengths() {
        let estimate = |tag: &str| {
            let mut dictionary = MetricDictionary::default();
            intern(&mut dictionary, &tagged("requests", &["env:prod"], &[tag]));
            dictionary.stats().estimated_bytes
        };
        assert_eq!(
            estimate(&"a".repeat(1001)) - estimate("a"),
            2 * 1000,
            "only the tag string's definition and lookup grow"
        );
    }

    /// A stream that has seen part of a payload's closure gets only the rest, still in
    /// dependency order.
    #[test]
    fn definitions_for_skips_what_the_stream_has_sent() {
        let mut dictionary = MetricDictionary::default();
        let first = intern(&mut dictionary, &tagged("requests", &["env:prod"], &[]));
        let sent: BTreeSet<_> = first.iter().copied().collect();

        let second = intern(
            &mut dictionary,
            &tagged("requests", &["env:prod"], &["team:a"]),
        );
        let needed: Vec<_> = dictionary
            .definitions_for(&second, &sent)
            .map(DefinitionKey::of)
            .collect();
        assert_eq!(
            needed,
            [DefinitionKey::TagString(2), DefinitionKey::Tagset(2)]
        );

        let fresh: Vec<_> = dictionary
            .definitions_for(&second, &BTreeSet::new())
            .map(DefinitionKey::of)
            .collect();
        assert_eq!(fresh, second);
    }

    /// Key order is a dependency order, including after a prefix tagset is evicted and its
    /// value returns under a fresh ID: replaying the retained definitions onto an empty
    /// dictionary never meets a reference to an undefined ID, and the replayed dictionary
    /// needs no new definitions for the same series.
    #[test]
    fn key_order_replays_as_a_dependency_order_after_eviction() {
        let mut dictionary = MetricDictionary::default();
        let first = tagged("requests", &["env:prod"], &["service:api"]);
        intern(&mut dictionary, &first);

        // Drop everything the next batch does not reference, then bring the prefix back so
        // it is re-created under a higher ID than before.
        let other = tagged("latency", &[], &[]);
        intern(&mut dictionary, &other);
        let evicted = dictionary.evict(&evict_everything());
        assert!(evicted.contains(&DefinitionKey::Tagset(1)));
        let returning = tagged("requests", &["env:prod"], &["service:web"]);
        intern(&mut dictionary, &returning);
        assert_eq!(dictionary.definition(DefinitionKey::Tagset(1)), None);

        let definitions: Vec<_> = dictionary
            .retained_keys()
            .into_iter()
            .map(|key| dictionary.definition(key).unwrap().clone())
            .collect();
        let mut replayed = MetricDictionary::default();
        replayed.apply_definitions(&definitions, Instant::now());
        for series in [&returning, &other] {
            let mut introduced = Vec::new();
            replayed.intern_series(series, &mut introduced);
            assert!(introduced.is_empty());
        }
    }

    /// Evicted values stay reachable from neither direction: the value-to-ID lookups and the
    /// retained definitions are pruned together.
    #[test]
    fn eviction_prunes_definitions_and_lookups_together() {
        let mut dictionary = MetricDictionary::default();
        let keys = intern(&mut dictionary, &series());
        intern(&mut dictionary, &tagged("other", &[], &[]));
        dictionary.evict(&evict_everything());

        assert!(keys.iter().all(|key| dictionary.definition(*key).is_none()));
        let mut definitions = Vec::new();
        dictionary.intern_series(&series(), &mut definitions);
        assert_eq!(
            definitions.len(),
            keys.len(),
            "every value is defined again"
        );
        assert!(definitions
            .iter()
            .all(|datum| !keys.contains(&DefinitionKey::of(datum))));
    }

    fn series() -> LogicalMetricSeries {
        LogicalMetricSeries::new(
            "requests",
            MetricSeriesType::Count,
            vec![MetricPoint::new(1, 2.0)],
        )
        .with_tags(MetricTagSet {
            prefix: vec!["env:prod".to_string()],
            values: vec!["service:api".to_string()],
        })
        .with_resources(vec![MetricResource::new("host", "web-1")])
        .with_unit("request")
        .with_source_type_name("nginx".to_string())
        .with_origin(MetricOrigin::new(1, 2, 3))
    }

    #[test]
    fn definitions_precede_composite_references() {
        let mut dictionary = MetricDictionary::default();
        dictionary.begin_batch(Instant::now());
        let mut definitions = Vec::new();
        dictionary.intern_series(&series(), &mut definitions);

        let tag_string_position = definitions
            .iter()
            .position(|datum| matches!(datum.data, Some(datum::Data::MetricTagStringDefine(_))))
            .unwrap();
        let tagset_position = definitions
            .iter()
            .position(|datum| matches!(datum.data, Some(datum::Data::MetricTagsetDefine(_))))
            .unwrap();
        let resource_string_position = definitions
            .iter()
            .position(|datum| {
                matches!(datum.data, Some(datum::Data::MetricResourceStringDefine(_)))
            })
            .unwrap();
        let resource_position = definitions
            .iter()
            .position(|datum| matches!(datum.data, Some(datum::Data::MetricResourceDefine(_))))
            .unwrap();
        let unit = definitions
            .iter()
            .find_map(|datum| match datum.data.as_ref() {
                Some(datum::Data::MetricUnitDefine(define)) => Some(define),
                _ => None,
            })
            .unwrap();

        // Tagsets and resource sets reference string IDs, so those strings must be defined first.
        assert!(tag_string_position < tagset_position);
        assert!(resource_string_position < resource_position);
        assert_eq!(unit.id, 1);
        assert_eq!(unit.value, "request");
    }

    #[test]
    fn dictionary_reuses_references_across_batches() {
        let mut dictionary = MetricDictionary::default();
        dictionary.begin_batch(Instant::now());
        let mut first_definitions = Vec::new();
        let first = dictionary.intern_series(&series(), &mut first_definitions);
        let mut second_definitions = Vec::new();
        let second = dictionary.intern_series(&series(), &mut second_definitions);

        assert_eq!(first, second);
        assert!(!first_definitions.is_empty());
        assert!(second_definitions.is_empty());
    }

    #[test]
    fn dictionary_reuses_reordered_tagsets() {
        let first_series = series().with_tags(MetricTagSet {
            prefix: vec!["region:us".to_string(), "env:prod".to_string()],
            values: vec!["team:metrics".to_string(), "service:api".to_string()],
        });
        let second_series = series().with_tags(MetricTagSet {
            prefix: vec!["env:prod".to_string(), "region:us".to_string()],
            values: vec!["service:api".to_string(), "team:metrics".to_string()],
        });
        let mut dictionary = MetricDictionary::default();
        dictionary.begin_batch(Instant::now());
        let mut first_definitions = Vec::new();
        let first = dictionary.intern_series(&first_series, &mut first_definitions);
        let mut second_definitions = Vec::new();
        let second = dictionary.intern_series(&second_series, &mut second_definitions);

        assert_eq!(first.tags, second.tags);
        assert!(second_definitions.is_empty());
    }

    #[test]
    fn snapshot_can_reseed_an_equivalent_dictionary() {
        let mut dictionary = MetricDictionary::default();
        dictionary.begin_batch(Instant::now());
        let mut definitions = Vec::new();
        let first = dictionary.intern_series(&series(), &mut definitions);

        let mut reseeded = MetricDictionary::default();
        reseeded.apply_definitions(&definitions, Instant::now());
        let mut new_definitions = Vec::new();
        let second = reseeded.intern_series(&series(), &mut new_definitions);

        assert_eq!(first, second);
        assert!(new_definitions.is_empty());
    }

    /// Content hashes identify a value whatever its ID: every value of a series returns under
    /// new IDs with the hashes it had, and reseeding the same definitions reproduces them.
    #[test]
    fn content_hashes_survive_new_ids_and_reseeding() {
        let config = MetricDictionaryEvictionConfig {
            max_item_count: 1000,
            ..evict_everything()
        };
        let hashes = |dictionary: &MetricDictionary, keys: &[DefinitionKey]| {
            keys.iter()
                .map(|key| dictionary.retention.frequency_state(*key).unwrap())
                .collect::<Vec<_>>()
        };
        let mut dictionary = MetricDictionary::with_eviction(&config);
        dictionary.begin_batch(Instant::now());
        let mut definitions = Vec::new();
        dictionary.intern_series(&series(), &mut definitions);
        let original = dictionary.references().collect::<Vec<_>>();
        let first = hashes(&dictionary, &original);
        let distinct: BTreeSet<_> = first.iter().map(|(hash, _)| hash).collect();
        assert_eq!(distinct.len(), first.len());
        assert!(first.iter().all(|(_, returning)| !returning));

        intern(&mut dictionary, &tagged("other", &[], &[]));
        dictionary.evict(&evict_everything());
        let returned = intern(&mut dictionary, &series());
        assert!(returned.iter().all(|key| !original.contains(key)));
        assert_eq!(
            hashes(&dictionary, &returned),
            first
                .iter()
                .map(|(hash, _)| (*hash, true))
                .collect::<Vec<_>>()
        );

        let mut reseeded = MetricDictionary::with_eviction(&config);
        reseeded.apply_definitions(&definitions, Instant::now());
        assert_eq!(hashes(&reseeded, &original), first);

        let mut kinds = MetricDictionary::with_eviction(&config);
        let shared = LogicalMetricSeries::new("nginx", MetricSeriesType::Gauge, Vec::new())
            .with_source_type_name("nginx".to_string());
        let keys = intern(&mut kinds, &shared);
        let [(name, _), (source_type, _)] = hashes(&kinds, &keys)[..] else {
            panic!("a name and a source type");
        };
        assert_ne!(name, source_type);
    }

    #[test]
    fn dictionaries_without_eviction_keep_no_sketch() {
        let mut dictionary = MetricDictionary::default();
        let keys = intern(&mut dictionary, &series());
        assert_eq!(dictionary.stats().frequency_sketch_bytes, 0);
        assert!(keys
            .iter()
            .all(|key| dictionary.retention.frequency_state(*key) == Some((0, false))));
        assert!(
            MetricDictionary::with_eviction(&evict_everything())
                .stats()
                .frequency_sketch_bytes
                > 0
        );
    }
}
