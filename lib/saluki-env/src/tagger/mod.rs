use saluki_event::metric::MetricTags;

pub trait TaggerProvider {
    type Error;

    fn get_tags_for_entity<E>(&self, entity_id: E) -> Result<MetricTags, Self::Error>
    where
        E: AsRef<str> + Send;
}

pub struct NoopTaggerProvider;

impl TaggerProvider for NoopTaggerProvider {
    type Error = std::convert::Infallible;

    fn get_tags_for_entity<E>(&self, _entity_id: E) -> Result<MetricTags, Self::Error>
    where
        E: AsRef<str> + Send,
    {
        Ok(MetricTags::default())
    }
}
