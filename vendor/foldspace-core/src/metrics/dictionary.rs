use std::{collections::HashMap, mem::size_of, time::Instant};

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
    tag_strings_by_id: HashMap<u64, String>,
    source_type_names: HashMap<String, u64>,
    units: HashMap<String, u64>,
    resource_strings: HashMap<String, u64>,
    resource_strings_by_id: HashMap<u64, String>,
    origins: HashMap<MetricOrigin, u64>,
    tagsets: HashMap<u64, HashMap<Vec<String>, u64>>,
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

impl MetricDictionary {
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

    pub(crate) fn apply_definitions(&mut self, definitions: &[MetricDatum], now: Instant) {
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
                    self.tag_strings_by_id
                        .insert(define.id, define.value.clone());
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
                    self.resource_strings_by_id
                        .insert(define.id, define.value.clone());
                }
                datum::Data::MetricResourceDefine(define) => {
                    let resources = define
                        .type_string_ids
                        .iter()
                        .zip(&define.name_string_ids)
                        .map(|(kind, name)| {
                            MetricResource::new(
                                self.resource_strings_by_id
                                    .get(kind)
                                    .expect("resource type definition must precede resource set")
                                    .clone(),
                                self.resource_strings_by_id
                                    .get(name)
                                    .expect("resource name definition must precede resource set")
                                    .clone(),
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
                    let tags = define
                        .tag_string_ids
                        .iter()
                        .map(|id| {
                            self.tag_strings_by_id
                                .get(id)
                                .expect("tag string definition must precede tagset")
                                .clone()
                        })
                        .collect();
                    self.next_tagset_id = self.next_tagset_id.max(define.id);
                    self.tagsets
                        .entry(define.prefix_id)
                        .or_default()
                        .insert(tags, define.id);
                }
                datum::Data::MetricSeriesBatch(_) => continue,
            }
            self.track_definition(definition);
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
        self.tag_strings_by_id.insert(id, value.to_string());
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
        self.resource_strings_by_id.insert(id, value.to_string());
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
        if let Some(id) = self
            .tagsets
            .get(&prefix_id)
            .and_then(|tagsets| tagsets.get(tags))
        {
            return *id;
        }

        let tag_string_ids = tags
            .iter()
            .map(|tag| self.intern_tag_string(tag, definitions))
            .collect();
        self.next_tagset_id += 1;
        let id = self.next_tagset_id;
        self.tagsets
            .entry(prefix_id)
            .or_default()
            .insert(tags.to_vec(), id);
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

    pub(super) fn references(&self) -> impl Iterator<Item = DefinitionKey> + '_ {
        self.retention.references()
    }

    pub(super) fn stats(&self) -> MetricDictionaryStats {
        self.retention.stats()
    }

    pub(super) fn evict(&mut self, config: &MetricDictionaryEvictionConfig) -> Vec<DefinitionKey> {
        let evicted = self.retention.evict(config);
        if !evicted.is_empty() {
            self.prune_lookups();
        }
        evicted
    }

    #[cfg(test)]
    pub(super) fn retained_keys(&self) -> Vec<DefinitionKey> {
        self.retention.keys().collect()
    }

    fn prune_lookups(&mut self) {
        let retained = &self.retention;
        self.names
            .retain(|_, id| retained.contains(DefinitionKey::Name(*id)));
        self.tag_strings
            .retain(|_, id| retained.contains(DefinitionKey::TagString(*id)));
        self.tag_strings_by_id
            .retain(|id, _| retained.contains(DefinitionKey::TagString(*id)));
        self.source_type_names
            .retain(|_, id| retained.contains(DefinitionKey::SourceType(*id)));
        self.units
            .retain(|_, id| retained.contains(DefinitionKey::Unit(*id)));
        self.resource_strings
            .retain(|_, id| retained.contains(DefinitionKey::ResourceString(*id)));
        self.resource_strings_by_id
            .retain(|id, _| retained.contains(DefinitionKey::ResourceString(*id)));
        self.origins
            .retain(|_, id| retained.contains(DefinitionKey::Origin(*id)));
        self.resources
            .retain(|_, id| retained.contains(DefinitionKey::Resource(*id)));
        self.tagsets.retain(|_, sets| {
            sets.retain(|_, id| retained.contains(DefinitionKey::Tagset(*id)));
            !sets.is_empty()
        });
    }

    fn track_definition(&mut self, definition: &MetricDatum) {
        // Charge the strings owned by the value-to-ID lookup maps.
        let lookup_bytes = match definition.data.as_ref().unwrap() {
            datum::Data::MetricNameDefine(d) => d.value.len(),
            datum::Data::MetricTagStringDefine(d) => 2 * d.value.len(),
            datum::Data::MetricSourceTypeNameDefine(d) => d.value.len(),
            datum::Data::MetricUnitDefine(d) => d.value.len(),
            datum::Data::MetricResourceStringDefine(d) => 2 * d.value.len(),
            datum::Data::MetricOriginDefine(_) => size_of::<MetricOrigin>(),
            datum::Data::MetricTagsetDefine(d) => d
                .tag_string_ids
                .iter()
                .map(|id| size_of::<String>() + self.tag_strings_by_id[id].len())
                .sum(),
            datum::Data::MetricResourceDefine(d) => d
                .type_string_ids
                .iter()
                .chain(&d.name_string_ids)
                .map(|id| size_of::<String>() + self.resource_strings_by_id[id].len())
                .sum(),
            datum::Data::MetricSeriesBatch(_) => unreachable!("only definitions are tracked"),
        };
        self.retention.insert(definition, lookup_bytes);
    }

    fn push_definition(&mut self, definition: MetricDatum, definitions: &mut Vec<MetricDatum>) {
        self.track_definition(&definition);
        definitions.push(definition);
    }
}

#[cfg(test)]
mod tests {
    use crate::proto::stateful::metric_datum as datum;

    use super::*;
    use crate::{LogicalMetricSeries, MetricPoint, MetricSeriesType};

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
}
