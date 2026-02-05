use super::*;

use async_trait::async_trait;

pub struct Console {
    check_id: String,
}

#[async_trait]
impl Sink for Console {
    async fn submit_metric(&self, metric: metric::Metric, _flush_first: bool) {
        println!("[{}] submit_metric: {metric:#?}", self.check_id);
    }

    async fn submit_service_check(&self, service_check: service_check::ServiceCheck) {
        println!("[{}] submit_service_check: {service_check:#?}", self.check_id);
    }

    async fn submit_event(&self, event: event::Event) {
        println!("[{}] submit_event: {event:#?}", self.check_id);
    }

    async fn submit_histogram(&self, histogram: histogram::Histrogram, _flush_first: bool) {
        println!("[{}] submit_histogram: {histogram:#?}", self.check_id);
    }

    async fn submit_event_platform_event(&self, event: event_platform::Event) {
        println!("[{}] submit_event_platform_event: {event:#?}", self.check_id);
    }

    async fn log(&self, level: log::Level, message: String) {
        println!("[{}] [{level:?}] {message}", self.check_id)
    }
}

impl Console {
    pub fn new(check_id: &str) -> Self {
        Self {
            check_id: check_id.to_string(),
        }
    }
}
