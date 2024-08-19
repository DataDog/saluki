use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler,
};
use serde::Serialize;

use crate::{
    allocator::{AllocationGroupRegistry, AllocationStatsSnapshot},
    registry::ComponentMetadata,
};

#[derive(Serialize)]
struct ComponentUsage {
    minimum_required_bytes: usize,
    firm_limit_bytes: usize,
    actual_live_bytes: usize,
}

/// State used for the memory registry API handler.
#[derive(Clone)]
pub struct MemoryRegistryState {
    inner: Arc<Mutex<ComponentMetadata>>,
}

impl MemoryRegistryState {
    fn get_response(&self) -> String {
        // The component registry is a nested structure, whereas the allocation registry is a flat structure. We can
        // only iterate via closure with the allocation registry, so we do that, and for each entry, we look for it in
        // the component registry. Luckily, the components in the allocation registry use the same naming structure as
        // the component registry supports for doing nested lookups in a single call, so we can just use that.

        // TODO: This is only going to show components which have taken a token, which means some components in the
        // registry, the ones whose bounds _are_ display when we verify bounds at startup, won't have an entry here
        // because they don't use a token... _and_ even if we made sure to include all components in the registry, even
        // if they didn't have an allocation group associated, their usage might just be attributed to the root
        // allocation group so then you end up with components that say they have a minimum usage, etc, but show a
        // current usage of zero bytes, etc.
        //
        // One option we could try is:
        // - get the total minimum/firm bounds by rolling up all components
        // - as we iterate over each allocation group, we subtract the bounds of that component from the totals we
        //   calculated before
        // - at the end, we assign the remaining total bounds to the root component
        //
        // Essentially, because the memory usage for component with bounds that _don't_ use a token will inherently be
        // attributed to the root allocation group anyways, this would allow at least capturing those bounds for
        // comparison against the otherwise-unattributed usage.
        //
        // Realistically, we _should_ still have our code set up to track all allocations for components that declare
        // bounds, but this could be a reasonable stopgap.
        let empty_snapshot = AllocationStatsSnapshot::empty();

        let mut component_usage = BTreeMap::new();

        let mut inner = self.inner.lock().unwrap();
        AllocationGroupRegistry::global().visit_allocation_groups(|component_name, component_stats| {
            let component_meta = inner.get_or_create(component_name);
            let component_meta = component_meta.lock().unwrap();
            let bounds = component_meta.self_bounds();
            let stats_snapshot = component_stats.snapshot_delta(&empty_snapshot);

            component_usage.insert(
                component_name.to_string(),
                ComponentUsage {
                    minimum_required_bytes: bounds.self_minimum_required_bytes,
                    firm_limit_bytes: bounds.self_firm_limit_bytes,
                    actual_live_bytes: stats_snapshot.live_bytes(),
                },
            );
        });

        serde_json::to_string(&component_usage).unwrap()
    }
}

/// An API handler for reporting the memory bounds and usage of all components.
///
/// This handler exposes a single route -- `/memory/status` -- which returns the overall bounds and live usage of each
/// registered component.
pub struct MemoryAPIHandler {
    state: MemoryRegistryState,
}

impl MemoryAPIHandler {
    pub(crate) fn from_state(state: Arc<Mutex<ComponentMetadata>>) -> Self {
        Self {
            state: MemoryRegistryState { inner: state },
        }
    }

    async fn status_handler(State(state): State<MemoryRegistryState>) -> impl IntoResponse {
        state.get_response()
    }
}

impl APIHandler for MemoryAPIHandler {
    type State = MemoryRegistryState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route("/memory/status", get(Self::status_handler))
    }
}
