use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use datadog_protos::events::EventsPayload;
use serde::{Deserialize, Serialize};
use stele::Event;

#[derive(Clone)]
pub struct EventsState {
    events: Arc<Mutex<Vec<Event>>>,
}

/// The JSON payload posted to /intake/ by the stock Datadog Agent.
#[derive(Deserialize, Serialize)]
pub struct IntakePayload {
    pub events: Option<HashMap<String, Vec<IntakeEvent>>>,
}

/// A single event from the /intake/ JSON format.
#[derive(Deserialize, Serialize)]
pub struct IntakeEvent {
    pub msg_title: Option<String>,
    pub msg_text: Option<String>,
    pub alert_type: Option<String>,
    pub aggregation_key: Option<String>,
    pub host: Option<String>,
    pub priority: Option<String>,
    pub tags: Option<Vec<String>>,
}

impl EventsState {
    pub fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn dump_events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    pub fn merge_events_payload(&self, payload: EventsPayload) {
        let new_events = Event::from_events_payload(payload);
        let mut data = self.events.lock().unwrap();
        data.extend(new_events);
    }

    pub fn merge_intake_payload(&self, payload: IntakePayload) {
        let Some(events_map) = payload.events else { return };

        let mut new_events = Vec::new();
        for (source_type, events) in events_map {
            // The stock agent's serializer uses "api" as the default map key when an event has no
            // source type name set (pkg/serializer/internal/metrics/events.go). Normalize it back
            // to empty string so it compares correctly against ADP's protobuf representation, where
            // an absent source_type_name is serialized as "".
            let normalized_source_type = if source_type == "api" {
                String::new()
            } else {
                source_type.clone()
            };
            for intake_event in events {
                let event = Event::from_intake_event(
                    intake_event.msg_title.unwrap_or_default(),
                    intake_event.msg_text.unwrap_or_default(),
                    intake_event.alert_type.unwrap_or_default(),
                    intake_event.aggregation_key.unwrap_or_default(),
                    intake_event.host.unwrap_or_default(),
                    intake_event.priority.unwrap_or_default(),
                    normalized_source_type.clone(),
                    intake_event.tags.unwrap_or_default(),
                );
                new_events.push(event);
            }
        }

        let mut data = self.events.lock().unwrap();
        data.extend(new_events);
    }
}
