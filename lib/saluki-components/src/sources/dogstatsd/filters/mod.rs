use saluki_io::deser::codec::dogstatsd::MetricPacket;

pub struct MetricPayloadFilter {}

impl Filter for MetricPayloadFilter {
    #[allow(unused)]
    fn allow_metric<'a>(&self, metric: &MetricPacket<'a>) -> bool {
        true
    }
}

pub trait Filter {
    fn allow_metric<'a>(&self, metric: &MetricPacket<'a>) -> bool;
}
