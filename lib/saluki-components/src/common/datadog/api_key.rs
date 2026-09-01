//! Live API keys for resolved endpoints.
//!
//! An endpoint's API key can change while the process runs: an operator rotates it, or the Core Agent
//! resolves a secret and sends the result. Each endpoint therefore holds its key in an [`ApiKeyCell`]
//! instead of a plain `String`. The request path reads the cell, and an [`ApiKeyRefresher`] task
//! writes it when typed configuration reports a new key. The write is an atomic pointer swap, so a
//! request reads either the previous key or the new one.
//!
//! Splitting the read from the write keeps the policy in one place. Trimming, and the decision to
//! keep the last usable key when configuration supplies none, happen where the key is written rather
//! than on every read.

use std::collections::HashMap;
use std::sync::Arc;

use agent_data_plane_config::Live;
use arc_swap::ArcSwap;
use saluki_common::task::spawn_traced_named;
use stringtheory::MetaString;
use tracing::{debug, warn};

use super::endpoints::RoutableEndpoint;

/// The API key one endpoint presents, shared between the request path and the task that updates it.
///
/// Cloning shares the cell: an endpoint, the validation task's copy of that endpoint, and the
/// refresher all read and write the same key.
#[derive(Clone, Debug)]
pub(crate) struct ApiKeyCell {
    current: Arc<ArcSwap<MetaString>>,
}

impl ApiKeyCell {
    /// Creates a cell holding `api_key`, trimmed.
    pub(crate) fn new(api_key: &str) -> Self {
        Self {
            current: Arc::new(ArcSwap::from_pointee(shared(api_key.trim()))),
        }
    }

    /// Returns the current API key.
    pub(crate) fn load(&self) -> MetaString {
        MetaString::clone(&self.current.load())
    }

    /// Replaces the current API key, and reports whether it changed.
    ///
    /// `api_key` is trimmed here so that a caller cannot install a key that differs from the
    /// configured one only by surrounding whitespace. A blank key, or no key at all, leaves the cell
    /// alone: a request with no key cannot succeed, so the last usable key stays in place.
    fn store(&self, api_key: Option<&str>) -> StoreOutcome {
        let Some(api_key) = api_key.map(str::trim).filter(|key| !key.is_empty()) else {
            return StoreOutcome::NoUsableKey;
        };

        if **self.current.load() == *api_key {
            return StoreOutcome::Unchanged;
        }

        self.current.store(Arc::new(shared(api_key)));
        StoreOutcome::Replaced
    }
}

/// Returns `api_key` as a `MetaString` that clones without allocating, whatever its length.
fn shared(api_key: &str) -> MetaString {
    MetaString::from(Arc::<str>::from(api_key))
}

/// What a store did to the cell.
#[derive(Debug, Eq, PartialEq)]
enum StoreOutcome {
    /// Configuration supplied no key, or a blank one, and the cell keeps its key.
    NoUsableKey,

    /// Configuration supplied the key the cell already holds.
    Unchanged,

    /// The cell now holds a new key.
    Replaced,
}

/// A live view of the configured additional endpoints, keyed by intake URL as configuration spells it.
type AdditionalEndpointsView = Live<HashMap<String, Vec<String>>>;

/// A live view of one configured API key.
///
/// Configuration always resolves the primary intake's key to a string, while a failover region's key
/// can be left unset. The two shapes are kept apart so that neither view has to invent a value.
#[derive(Clone, Debug)]
pub(crate) enum ApiKeyView {
    /// A key configuration always resolves, such as the primary intake's key.
    Required(Live<String>),

    /// A key configuration can leave unset, such as a failover region's key.
    Optional(Live<Option<String>>),
}

impl ApiKeyView {
    /// Returns the key the view has projected so far.
    fn current(&self) -> Option<&str> {
        match self {
            Self::Required(view) => Some(view.as_str()),
            Self::Optional(view) => view.as_deref(),
        }
    }

    /// Waits for the configured key to change and returns it.
    async fn changed(&mut self) -> Option<String> {
        match self {
            Self::Required(view) => Some(view.changed().await),
            Self::Optional(view) => view.changed().await,
        }
    }
}

/// The live configuration views a forwarder's endpoints take their API keys from.
///
/// Both views are optional, and each forwarder supplies the combination it needs:
///
/// - The Datadog forwarder supplies both: primary and metrics-primary share the primary view because
///   the metrics override changes the URL, not the credential; additional endpoints follow their lists.
/// - A failover forwarder supplies only `primary`, holding the failover region's key, because a
///   single destination does not dual-ship.
/// - The Cluster Agent forwarder supplies neither: it presents a bearer token, which is not a
///   configured API key, so nothing refreshes it.
#[derive(Clone, Debug, Default)]
pub(crate) struct LiveApiKeys {
    /// The key the primary and metrics-primary endpoints follow.
    pub(crate) primary: Option<ApiKeyView>,

    /// The configured additional endpoints, keyed by intake URL as configuration spells it.
    pub(crate) additional: Option<AdditionalEndpointsView>,
}

/// An endpoint that follows the one primary key.
struct PrimaryTarget {
    /// Resolved endpoint URL, to name the endpoint in a log line.
    endpoint: String,

    cell: ApiKeyCell,
}

/// An endpoint that follows one position in one configured key list.
struct AdditionalTarget {
    /// Intake URL, as configuration spells it and before normalization.
    url: String,

    /// Position of this endpoint's key in the key list configured for that URL. This is the configured
    /// position, not a post-deduplication counter.
    index: usize,

    cell: ApiKeyCell,
}

/// Updates endpoint API keys as typed configuration changes.
pub(crate) struct ApiKeyRefresher {
    primary: Option<(ApiKeyView, Vec<PrimaryTarget>)>,
    additional: Option<(AdditionalEndpointsView, Vec<AdditionalTarget>)>,
}

impl ApiKeyRefresher {
    /// Binds endpoint API key cells to the supplied live views.
    ///
    /// An additional endpoint is bound to its configured URL and key position when an additional-endpoints
    /// view is supplied. Any other endpoint is bound to the primary view when one is supplied. Returns
    /// `None` when no supplied view has an endpoint to follow.
    pub(crate) fn new(endpoints: &[RoutableEndpoint], api_keys: &LiveApiKeys) -> Option<Self> {
        let mut primary_targets = Vec::new();
        let mut additional_targets = Vec::new();

        for routable in endpoints {
            let endpoint = routable.endpoint();
            let cell = endpoint.api_key_cell().clone();

            match endpoint.additional_endpoint_queue_key() {
                Some((url, index)) => additional_targets.push(AdditionalTarget {
                    url: url.to_string(),
                    index,
                    cell,
                }),
                None => primary_targets.push(PrimaryTarget {
                    endpoint: endpoint.endpoint().to_string(),
                    cell,
                }),
            }
        }

        let primary = match &api_keys.primary {
            Some(view) if !primary_targets.is_empty() => Some((view.clone(), primary_targets)),
            _ => None,
        };
        let additional = match &api_keys.additional {
            Some(view) if !additional_targets.is_empty() => Some((view.clone(), additional_targets)),
            _ => None,
        };

        if primary.is_none() && additional.is_none() {
            return None;
        }

        // An endpoint's starting key came from a configuration snapshot read before these views were
        // created. A configuration store in between leaves the view's baseline ahead of the endpoint,
        // and `changed` reports a value only once it differs from that baseline, so the endpoint would
        // keep the older key until some later rotation. Storing what the views project now closes
        // that gap here, while the forwarder is still being built and no request can be sent.
        if let Some((view, targets)) = &primary {
            let api_key = view.current();
            for target in targets {
                store(&target.cell, &target.endpoint, api_key);
            }
        }

        if let Some((view, targets)) = &additional {
            for target in targets {
                store(&target.cell, &target.url, additional_api_key(view, target));
            }
        }

        Some(Self { primary, additional })
    }

    /// Spawns one task per bound view.
    ///
    /// The two views are independent, so a change to one does not re-read the other.
    pub(crate) fn spawn(self) {
        if let Some((view, targets)) = self.primary {
            spawn_traced_named("dd-api-key-refresher-primary", refresh_primary(view, targets));
        }

        if let Some((view, targets)) = self.additional {
            spawn_traced_named("dd-api-key-refresher-additional", refresh_additional(view, targets));
        }
    }
}

async fn refresh_primary(mut view: ApiKeyView, targets: Vec<PrimaryTarget>) {
    loop {
        let api_key = view.changed().await;
        for target in &targets {
            store(&target.cell, &target.endpoint, api_key.as_deref());
        }
    }
}

async fn refresh_additional(mut view: AdditionalEndpointsView, targets: Vec<AdditionalTarget>) {
    loop {
        let configured = view.changed().await;
        for target in &targets {
            store(&target.cell, &target.url, additional_api_key(&configured, target));
        }
    }
}

/// Returns the key configured for `target`'s URL and position, if there is one.
fn additional_api_key<'a>(configured: &'a HashMap<String, Vec<String>>, target: &AdditionalTarget) -> Option<&'a str> {
    configured
        .get(&target.url)
        .and_then(|api_keys| api_keys.get(target.index))
        .map(String::as_str)
}

fn store(cell: &ApiKeyCell, endpoint: &str, api_key: Option<&str>) {
    match cell.store(api_key) {
        StoreOutcome::NoUsableKey => warn!(
            endpoint,
            "Configuration no longer supplies a usable API key for this endpoint. Continuing with the previously \
             configured API key."
        ),
        StoreOutcome::Unchanged => {}
        StoreOutcome::Replaced => debug!(endpoint, "Refreshed endpoint API key."),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use agent_data_plane_config::SalukiConfiguration;

    use super::*;
    use crate::common::datadog::{
        config::ForwarderConfiguration,
        endpoints::{ResolvedEndpoint, SingleDestination},
        test_util::{shared_configuration, LiveConfiguration},
    };

    const PRIMARY_URL: &str = "http://primary.example.com";
    const ADDITIONAL_URL: &str = "http://additional.example.com";

    /// Returns a configuration whose primary key is `api_key` and whose additional endpoints are
    /// `additional`.
    fn config(api_key: &str, additional: &[(&str, &[&str])]) -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        config.shared.endpoints.api_key = api_key.to_string();
        config.shared.endpoints.additional_endpoints = additional
            .iter()
            .map(|(url, api_keys)| {
                (
                    (*url).to_string(),
                    api_keys.iter().map(|api_key| (*api_key).to_string()).collect(),
                )
            })
            .collect();

        config
    }

    /// Returns the endpoints the Datadog forwarder builds for `additional`, with the refresher that
    /// follows `live` already spawned.
    fn spawn_datadog_endpoints(live: &LiveConfiguration, additional: &[(&str, &[&str])]) -> Vec<RoutableEndpoint> {
        let endpoints = datadog_endpoints(additional);

        ApiKeyRefresher::new(&endpoints, &live.api_keys())
            .expect("the endpoints should follow the live views")
            .spawn();

        endpoints
    }

    /// Returns the endpoints the Datadog forwarder builds for `additional`.
    fn datadog_endpoints(additional: &[(&str, &[&str])]) -> Vec<RoutableEndpoint> {
        let mut shared = shared_configuration();
        shared.endpoints.api_key = "start-key".to_string();
        shared.endpoints.dd_url = agent_data_plane_config::ConfigValue::explicit(PRIMARY_URL.to_string());
        shared.endpoints.additional_endpoints = additional
            .iter()
            .map(|(url, api_keys)| {
                (
                    (*url).to_string(),
                    api_keys.iter().map(|api_key| (*api_key).to_string()).collect(),
                )
            })
            .collect();

        ForwarderConfiguration::from_configuration(&shared)
            .build_routable_endpoints()
            .expect("endpoints should resolve")
    }

    /// Waits for `endpoint` to present `expected`, failing if it never does.
    async fn await_api_key(endpoint: &ResolvedEndpoint, expected: &str) {
        let observed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let api_key = endpoint.api_key();
                if &*api_key == expected {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;

        assert!(
            observed.is_ok(),
            "endpoint should present {expected:?}, but presents {:?}",
            endpoint.api_key()
        );
    }

    #[test]
    fn a_cell_trims_the_key_it_is_created_with() {
        let cell = ApiKeyCell::new("  key-1  ");
        assert_eq!("key-1", &*cell.load());
    }

    #[test]
    fn a_store_replaces_a_key_that_differs() {
        let cell = ApiKeyCell::new("key-1");
        assert_eq!(StoreOutcome::Replaced, cell.store(Some("key-2")));
        assert_eq!("key-2", &*cell.load());
    }

    #[test]
    fn a_store_trims_the_key_before_comparing_it() {
        let cell = ApiKeyCell::new("key-1");
        assert_eq!(StoreOutcome::Unchanged, cell.store(Some("  key-1  ")));
        assert_eq!("key-1", &*cell.load());
    }

    #[test]
    fn a_store_of_a_blank_key_leaves_the_last_usable_key() {
        let cell = ApiKeyCell::new("key-1");
        assert_eq!(StoreOutcome::NoUsableKey, cell.store(Some("   ")));
        assert_eq!("key-1", &*cell.load());
    }

    #[test]
    fn a_store_of_no_key_leaves_the_last_usable_key() {
        let cell = ApiKeyCell::new("key-1");
        assert_eq!(StoreOutcome::NoUsableKey, cell.store(None));
        assert_eq!("key-1", &*cell.load());
    }

    #[test]
    fn nothing_refreshes_an_endpoint_whose_forwarder_supplied_no_views() {
        // The Cluster Agent's shape: a bearer token that configuration does not own.
        let mut shared = shared_configuration();
        shared.endpoints.api_key = "bearer-token".to_string();
        let destination = SingleDestination {
            url: "http://cluster-agent.example.com".to_string(),
            api_key: "bearer-token".to_string(),
            accepts_v3_series: false,
        };
        let endpoints = ForwarderConfiguration::for_single_destination(&shared, &destination)
            .build_routable_endpoints()
            .expect("endpoint should resolve");

        assert!(ApiKeyRefresher::new(&endpoints, &LiveApiKeys::default()).is_none());
    }

    #[test]
    fn nothing_refreshes_additional_endpoints_a_forwarder_does_not_have() {
        // A forwarder that supplies both views but configures no additional endpoints leaves the
        // additional view unbound, so no task re-reads it.
        let live = LiveConfiguration::new(config("start-key", &[]));
        let mut shared = shared_configuration();
        shared.endpoints.dd_url = agent_data_plane_config::ConfigValue::explicit(PRIMARY_URL.to_string());
        let endpoints = ForwarderConfiguration::from_configuration(&shared)
            .build_routable_endpoints()
            .expect("endpoints should resolve");

        let refresher =
            ApiKeyRefresher::new(&endpoints, &live.api_keys()).expect("the primary endpoint should follow the view");
        assert!(refresher.primary.is_some());
        assert!(refresher.additional.is_none());
    }

    #[tokio::test]
    async fn a_rotated_primary_key_reaches_the_endpoint() {
        let live = LiveConfiguration::new(config("start-key", &[]));
        let endpoints = spawn_datadog_endpoints(&live, &[]);

        live.store(config("rotated-key", &[]));

        await_api_key(endpoints[0].endpoint(), "rotated-key").await;
    }

    #[tokio::test]
    async fn a_primary_key_rotated_before_the_views_existed_reaches_the_endpoint() {
        let live = LiveConfiguration::new(config("start-key", &[]));
        let endpoints = datadog_endpoints(&[]);

        // The rotation lands after the endpoints were built and before the views exist, so it is
        // already the views' baseline and no later change reports it.
        live.store(config("rotated-key", &[]));

        ApiKeyRefresher::new(&endpoints, &live.api_keys())
            .expect("the endpoints should follow the live views")
            .spawn();

        await_api_key(endpoints[0].endpoint(), "rotated-key").await;
    }

    #[tokio::test]
    async fn an_additional_key_rotated_before_the_views_existed_reaches_the_endpoint() {
        let additional: &[(&str, &[&str])] = &[(ADDITIONAL_URL, &["extra-key"])];
        let live = LiveConfiguration::new(config("start-key", additional));
        let endpoints = datadog_endpoints(additional);

        live.store(config("start-key", &[(ADDITIONAL_URL, &["rotated-extra-key"])]));

        ApiKeyRefresher::new(&endpoints, &live.api_keys())
            .expect("the endpoints should follow the live views")
            .spawn();

        let additional_endpoint = endpoints
            .iter()
            .find(|routable| routable.endpoint().additional_endpoint_queue_key().is_some())
            .expect("the additional endpoint should exist");
        await_api_key(additional_endpoint.endpoint(), "rotated-extra-key").await;
    }

    #[tokio::test]
    async fn primary_and_metrics_primary_endpoints_follow_the_same_primary_key() {
        let live = LiveConfiguration::new(config("start-key", &[]));
        let mut shared = shared_configuration();
        shared.endpoints.api_key = "start-key".to_string();
        shared.endpoints.opw_intake.enabled = true;
        shared.endpoints.opw_intake.url = "http://opw.example.com".to_string();

        let endpoints = ForwarderConfiguration::from_configuration(&shared)
            .build_routable_endpoints()
            .expect("endpoints should resolve");
        ApiKeyRefresher::new(&endpoints, &live.api_keys())
            .expect("both primary endpoints should follow the view")
            .spawn();

        live.store(config("rotated-key", &[]));

        for endpoint in &endpoints {
            await_api_key(endpoint.endpoint(), "rotated-key").await;
        }
    }

    #[tokio::test]
    async fn a_rotated_additional_key_reaches_only_its_own_endpoint() {
        let additional: &[(&str, &[&str])] = &[(ADDITIONAL_URL, &["extra-key-1", "extra-key-2"])];
        let live = LiveConfiguration::new(config("start-key", additional));
        let endpoints = spawn_datadog_endpoints(&live, additional);

        live.store(config(
            "start-key",
            &[(ADDITIONAL_URL, &["extra-key-1", "rotated-extra-key"])],
        ));

        let by_position: HashMap<usize, &ResolvedEndpoint> = endpoints
            .iter()
            .filter_map(|routable| {
                let endpoint = routable.endpoint();
                let (_, index) = endpoint.additional_endpoint_queue_key()?;
                Some((index, endpoint))
            })
            .collect();

        await_api_key(by_position[&1], "rotated-extra-key").await;
        assert_eq!("extra-key-1", &*by_position[&0].api_key());
    }

    #[tokio::test]
    async fn a_removed_additional_key_leaves_the_last_usable_key() {
        let additional: &[(&str, &[&str])] = &[(ADDITIONAL_URL, &["extra-key-1", "extra-key-2"])];
        let live = LiveConfiguration::new(config("start-key", additional));
        let endpoints = spawn_datadog_endpoints(&live, additional);

        live.store(config("start-key", &[(ADDITIONAL_URL, &["rotated-extra-key"])]));

        let by_position: HashMap<usize, &ResolvedEndpoint> = endpoints
            .iter()
            .filter_map(|routable| {
                let endpoint = routable.endpoint();
                let (_, index) = endpoint.additional_endpoint_queue_key()?;
                Some((index, endpoint))
            })
            .collect();

        await_api_key(by_position[&0], "rotated-extra-key").await;
        assert_eq!("extra-key-2", &*by_position[&1].api_key());
    }

    #[tokio::test]
    async fn a_rotated_failover_key_reaches_the_endpoint() {
        let mut config = SalukiConfiguration::default();
        config.domains.multi_region_failover.api_key = Some("failover-key".to_string());
        let live = LiveConfiguration::new(config);

        let shared = shared_configuration();
        let destination = SingleDestination {
            url: "http://failover.example.com".to_string(),
            api_key: "failover-key".to_string(),
            accepts_v3_series: true,
        };
        let endpoints = ForwarderConfiguration::for_single_destination(&shared, &destination)
            .build_routable_endpoints()
            .expect("endpoint should resolve");
        let api_keys = LiveApiKeys {
            primary: Some(ApiKeyView::Optional(
                live.live(|config| &config.domains.multi_region_failover.api_key),
            )),
            additional: None,
        };
        ApiKeyRefresher::new(&endpoints, &api_keys)
            .expect("the destination should follow the view")
            .spawn();

        let mut rotated = SalukiConfiguration::default();
        rotated.domains.multi_region_failover.api_key = Some("rotated-failover-key".to_string());
        live.store(rotated);

        // The optional view must carry configured values even though it can also report absence.
        await_api_key(endpoints[0].endpoint(), "rotated-failover-key").await;
    }
}
