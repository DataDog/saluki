use saluki_app::api::APIBuilder;
use saluki_config::GenericConfiguration;
use saluki_core::state::reflector::Reflector;
use saluki_error::GenericError;
use saluki_health::HealthRegistry;

use crate::state::metrics::AggregatedMetricsProcessor;

pub fn spawn_internal_processes(config: &GenericConfiguration, internal_metrics: Reflector<AggregatedMetricsProcessor>,
    unprivileged_api: APIBuilder, mut privileged_api: APIBuilder, health_registry: HealthRegistry) -> Result<(), GenericError>
{
	// We spawn two separate current-thread runtimes: one for APIs/health checks, and one for the internal observability topology.
	//
	// This is because we want to ensure these are isolated from any badness in the primary data topology, either in
	// terms of how its affecting the runtime behavior or in terms of the topology, and things like high data volume
	// consuming event buffers and starving the internal observability components, and so on.

	todo!()
}
