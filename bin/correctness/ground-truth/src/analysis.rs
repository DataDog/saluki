use stele::Metric;

pub struct RawTestResults {
    dsd_metrics: Vec<Metric>,
    adp_metrics: Vec<Metric>,
}

impl RawTestResults {
    pub fn new(dsd_metrics: Vec<Metric>, adp_metrics: Vec<Metric>) -> Self {
        Self {
            dsd_metrics,
            adp_metrics,
        }
    }

    pub fn dsd_metrics(&self) -> &[Metric] {
        &self.dsd_metrics
    }

    pub fn adp_metrics(&self) -> &[Metric] {
        &self.adp_metrics
    }
}
