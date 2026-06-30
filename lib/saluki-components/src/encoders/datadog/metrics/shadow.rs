use rand::RngExt as _;

#[derive(Clone, Copy, Debug)]
pub(super) struct SeriesShadowConfig {
    sample_rate: f64,
}

impl SeriesShadowConfig {
    pub(super) const fn new(sample_rate: f64) -> Self {
        Self { sample_rate }
    }

    pub(super) fn is_enabled(self) -> bool {
        self.sample_rate > 0.0
    }

    fn sample(self) -> bool {
        self.is_enabled() && shadow_sample_matches(self.sample_rate, rand::rng().random::<f64>())
    }
}

#[derive(Debug, Default)]
pub(super) struct SeriesShadowState {
    pub(super) active: Option<bool>,
}

impl SeriesShadowState {
    pub(super) fn ensure_decision(&mut self, config: SeriesShadowConfig) -> bool {
        *self.active.get_or_insert_with(|| config.sample())
    }

    pub(super) fn reset(&mut self) {
        self.active = None;
    }
}

pub(super) fn shadow_sample_matches(sample_rate: f64, sample: f64) -> bool {
    sample_rate > 0.0 && sample < sample_rate
}
