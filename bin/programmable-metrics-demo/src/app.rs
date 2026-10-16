//! The demo server: the UI page and the JSON API it uses.
//!
//! - `GET /` (and `/app.js`, `/app.css`): the UI, embedded in the binary.
//! - `GET /api/state`: everything the UI shows, polled about once per second, the Lanes panel's `lanes` included.
//! - `POST /api/compile` `{source}`: parses a program, composes it into ADP's default program, with the artifacts ADP
//!   runs for the other scopes, and validates the composed plan as ADP will, against the export of the scope it
//!   replaces; returns diagnostics, the artifact, its hash, its canonical source, and the plan diff against the active
//!   plan.
//! - `POST /api/publish` `{artifact_hash}`: publishes the last compiled artifact as the next revision, if
//!   `artifact_hash` is its hash. An approval is used once: publishing again needs another compile.
//! - `POST /api/revoke`: empties the policy snapshot, so that ADP goes back to the default program, aggregation and
//!   routing both.
//! - `POST /api/destinations`: adds a destination (see [`Destinations::add`]): a recording intake on a free port, added
//!   to `additional_endpoints` in ADP's configuration file, which ADP re-reads. Answers its name, label, URL, port, and
//!   API key.
//! - `DELETE /api/destinations/{name}`: removes an additional endpoint from the file; its intake answers until ADP
//!   retires its recipient (see [`Destinations::remove`]).
//! - `POST /api/native` `{metric_filterlist?, failover_metrics?}`: changes the native settings the demo writes in ADP's
//!   configuration file: the metric filterlist, a native policy setting, and the failover signal.
//! - `GET /api/default-program`: the export bundle's artifact, the default program's one scope, printed as source with
//!   ADP's notes as comments, with its legend, or why there is none.
//! - `POST /api/traffic` `{running?, rate?, mix?}`: starts, pauses, or changes the rate of the traffic generator, or
//!   the lines its datagrams carry (`mix`: `full`, or `canary_only`).
//! - `POST /api/intakes/{name}/pause` `{paused}`: pauses an intake, which then answers `503 Service Unavailable`, or
//!   resumes it.
//! - `GET /api/intakes/{name}/recent`: the most recent series and requests an intake received.
//! - `GET /api/canary?from_s=<s>&to_s=<s>`: the canary conservation check over a range of seconds (Unix seconds,
//!   within the last five minutes, ending at least seven seconds ago), such as the whole of a scripted run, with the
//!   canaries sent in each second and those the primary endpoint received for each bucket start, and how the range's
//!   datagrams reached ADP (`delivery`), which tells canaries the host dropped from canaries ADP lost.
//! - `GET /api/adp/{status,export,plan,default-program,explain,explain-text}`: ADP's control surface routes, passed
//!   through.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use saluki_metric_plan::{canonical::PlanHash, ir::Plan};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    adp_api::{routes, AdpApi, Surface},
    canary::{self, CanaryCheck, CanaryElsewhere, CanaryState, Delivery},
    clock::unix_millis,
    compile::{compile, default_program_source, CompileOutcome, CompiledArtifact, RecipientLabels},
    control::{ControlSurface, ControlView, EXPORT_NOT_EXPOSED_MESSAGE},
    destinations::{DestinationView, Destinations, NativeChange, NativeView, RemoveError},
    export::{ExportBundle, Legend},
    intake::IntakeSummary,
    launcher::{Launch, LaunchView, Phase},
    preview::{other_scope_artifacts, ActivePlan, CompileContext},
    recipients::RecipientsView,
    settings::{DemoPorts, Recipient, RecipientKind, RunSettings},
    stub::{
        protocol::{ApprovedAgainst, ChannelStatus, POLICY_PRODUCT, POLICY_SCOPE},
        ChannelView, StubChannel,
    },
    telemetry::{
        lanes::{self, LanesView},
        TelemetryScraper, TelemetrySnapshot,
    },
    traffic::{TrafficChange, TrafficGenerator, TrafficMix, TrafficState, CANARY_HISTORY_SECS},
    ui,
};

/// How many acknowledgment timeline entries `/api/state` returns.
const TIMELINE_LIMIT: usize = 40;

/// How many records `/api/intakes/{name}/recent` returns.
const RECENT_LIMIT: usize = 200;

/// The last successfully compiled artifact, which a publish approves by its hash.
#[derive(Clone, Debug)]
struct Compiled {
    artifact: CompiledArtifact,
    compiled_ms: u64,
    active_plan_hash: Option<String>,
}

/// Everything the demo server's handlers share.
#[derive(Clone)]
pub struct App {
    settings: Arc<RunSettings>,
    destinations: Destinations,
    traffic: TrafficGenerator,
    telemetry: TelemetryScraper,
    channel: StubChannel,
    control: ControlSurface,
    launch: Launch,
    api: AdpApi,
    compiled: Arc<Mutex<Option<Compiled>>>,
    /// The bucket widths that the artifacts the stub served set, by revision (see
    /// [`App::canary_widths`]).
    served_widths: Arc<Mutex<BTreeMap<u64, ServedWidths>>>,
}

/// The bucket widths one artifact sets, and when the stub last served it.
#[derive(Clone, Debug)]
struct ServedWidths {
    widths: Vec<u64>,
    last_served_ms: u64,
}

/// The demo's components, as the server uses them.
pub struct Components {
    /// The run's settings.
    pub settings: Arc<RunSettings>,

    /// The destinations: the recording intakes, and the live part of ADP's configuration.
    pub destinations: Destinations,

    /// The traffic generator.
    pub traffic: TrafficGenerator,

    /// The telemetry scraper.
    pub telemetry: TelemetryScraper,

    /// The artifact channel stub.
    pub channel: StubChannel,

    /// The control surface watcher.
    pub control: ControlSurface,

    /// The launcher's record of ADP.
    pub launch: Launch,

    /// The clients for ADP's APIs.
    pub api: AdpApi,
}

impl App {
    /// Creates the server's shared state.
    pub fn new(components: Components) -> Self {
        Self {
            settings: components.settings,
            destinations: components.destinations,
            traffic: components.traffic,
            telemetry: components.telemetry,
            channel: components.channel,
            control: components.control,
            launch: components.launch,
            api: components.api,
            compiled: Arc::new(Mutex::new(None)),
            served_widths: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Returns ADP's latest status: whichever of the privileged status route's and the latest poll's arrived last.
    fn latest_status(&self) -> Option<ChannelStatus> {
        let reported = self.control.status().map(|(at_ms, status)| (at_ms, status.status));
        match (reported, self.channel.latest_status()) {
            (Some(reported), Some(polled)) => Some(if polled.0 >= reported.0 { polled.1 } else { reported.1 }),
            (reported, polled) => reported.or(polled).map(|(_, status)| status),
        }
    }

    /// Returns the revision high-water mark the privileged status route reported, which publishing counts.
    fn reported_high_water(&self) -> Option<u64> {
        self.control
            .status()
            .and_then(|(_, status)| status.status.revision_high_water)
    }

    fn recipients(&self) -> Vec<Recipient> {
        self.destinations
            .intakes()
            .iter()
            .map(|intake| intake.recipient().clone())
            .collect()
    }

    /// Returns the labels the editor writes beside recipients named by their URLs.
    fn labels(&self) -> RecipientLabels {
        RecipientLabels::new(self.destinations.labels())
    }

    /// Notes, as served at `now_ms`, the bucket widths that the artifacts the stub serves set (see
    /// [`canary::artifact_widths`]).
    fn note_served_widths(&self, now_ms: u64) {
        let served = self
            .channel
            .envelopes()
            .into_iter()
            .filter(|(scope, _)| scope == POLICY_SCOPE)
            .map(|(_, envelope)| (envelope.revision, canary::artifact_widths(&envelope.artifact)))
            .filter(|(_, widths)| !widths.is_empty());
        let mut noted = self
            .served_widths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (revision, widths) in served {
            noted.insert(
                revision,
                ServedWidths {
                    widths,
                    last_served_ms: now_ms,
                },
            );
        }
    }

    /// Returns the policy product's revisions that ADP's status acknowledges as `rejected` or `failed`: artifacts that
    /// never ran.
    fn refused_revisions(&self) -> BTreeSet<u64> {
        self.control
            .status()
            .into_iter()
            .flat_map(|(_, status)| status.config_states)
            .filter(|state| state.product.is_empty() || state.product == POLICY_PRODUCT)
            .filter(|state| matches!(state.state.as_str(), "rejected" | "failed"))
            .filter_map(|state| state.revision)
            .collect()
    }

    /// Returns the bucket widths, besides the configured aggregation window, that the canary check aligns its ranges
    /// to at `now_ms`, in increasing order: those set by every artifact of the default program's scope that the stub
    /// served within the last [`CANARY_HISTORY_SECS`], unless ADP refused it.
    ///
    /// A width stays for that long after its artifact is revoked or replaced: its buckets stay in the history the
    /// check reads, and the cut back to another width takes effect only at a common multiple of the two. An artifact of
    /// another scope never sets a width, since ADP declares no other scope and refuses it.
    fn canary_widths(&self, now_ms: u64) -> Vec<u64> {
        self.note_served_widths(now_ms);
        let refused = self.refused_revisions();
        let mut noted = self
            .served_widths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        noted.retain(|revision, served| {
            !refused.contains(revision) && served.last_served_ms + CANARY_HISTORY_SECS * 1000 >= now_ms
        });
        let widths = noted
            .values()
            .flat_map(|served| served.widths.iter().copied())
            .collect::<BTreeSet<_>>();
        widths.into_iter().collect()
    }

    /// Returns the width the canary check's ranges are aligned to at `now_ms`: the least common multiple of the
    /// configured aggregation window and the widths in use ([`App::canary_widths`]), or `None` if no range the check
    /// can read holds whole buckets of every one (see [`canary::common_width`]).
    fn canary_width(&self, now_ms: u64) -> Option<u64> {
        canary::common_width(self.settings.aggregate_window_secs, self.canary_widths(now_ms))
    }

    /// Runs the canary conservation check at `now_ms`, over the range aligned to the widths in use. While those share
    /// no bucket boundary the check can wait for, it waits, over the configured window's range.
    fn canary_check(&self, now_ms: u64, launch: &LaunchView, telemetry: &TelemetrySnapshot) -> CanaryCheck {
        let widths = self.canary_widths(now_ms);
        let configured = self.settings.aggregate_window_secs;
        if let Some(width) = canary::common_width(configured, widths.iter().copied()) {
            return self.canary_check_range(canary::window(now_ms, width), launch, telemetry);
        }
        let mut check = self.canary_check_range(canary::window(now_ms, configured), launch, telemetry);
        check.state = CanaryState::Waiting;
        check.reason = Some(format!(
            "The bucket widths in use, {} s with the configured {} s, share no boundary within the {} s the check \
             aligns to. It resumes {} s after the stub last served the artifact that set the older width.",
            widths.iter().map(ToString::to_string).collect::<Vec<_>>().join(" and "),
            configured,
            canary::MAX_COMMON_WIDTH_SECS,
            CANARY_HISTORY_SECS
        ));
        check
    }

    /// Runs the canary conservation check over a range of seconds, `(from_s, to_s)`.
    fn canary_check_range(&self, range: (u64, u64), launch: &LaunchView, telemetry: &TelemetrySnapshot) -> CanaryCheck {
        let (from_s, to_s) = range;
        let mut received = 0.0;
        let mut elsewhere = Vec::new();
        for intake in self.destinations.intakes() {
            let count = intake.canaries_between(from_s, to_s);
            if intake.recipient().kind == RecipientKind::Primary {
                received = count;
            } else if count != 0.0 {
                elsewhere.push(CanaryElsewhere {
                    intake: intake.recipient().name.clone(),
                    received: count,
                });
            }
        }
        canary::check(
            range,
            self.traffic.canaries_sent_between(from_s, to_s),
            received,
            elsewhere,
            self.traffic.state().rate,
            up_since_ms(launch, telemetry),
            self.delivery(range),
        )
    }

    /// Returns how the datagrams of a range of seconds reached ADP: how many of those sent between the scrapes around
    /// the range ADP's DogStatsD source did not receive, and what the host dropped of the control datagrams sent in the
    /// range and in the second on each side of it.
    ///
    /// The scrapes around the range, rather than its own ends, bound what the source received: ADP reads datagrams late
    /// while the host is overloaded, and a scrape's count is read a moment before the scrape completes, so the count at
    /// an instant between scrapes is not known.
    fn delivery(&self, (from_s, to_s): (u64, u64)) -> Delivery {
        let not_received = self
            .telemetry
            .source_receipts_around(from_s * 1000, to_s * 1000)
            .map(|receipts| {
                self.traffic.datagrams_sent_between_ms(receipts.from_ms, receipts.to_ms) - receipts.datagrams
            });
        Delivery {
            not_received,
            control: self.traffic.control_between(from_s.saturating_sub(1), to_s + 1),
        }
    }
}

/// Returns since when the canary check counts ADP as up without interruption, or `None` if it is not up.
///
/// ADP the demo launched is up from when it reported ready for as long as its process runs, whether or not a scrape of
/// its telemetry failed meanwhile, as one can while the host is overloaded: its socket still receives. ADP started
/// separately is up while it answers its API, from the first of the scrapes that have all succeeded since.
fn up_since_ms(launch: &LaunchView, telemetry: &TelemetrySnapshot) -> Option<u64> {
    match launch.phase {
        Phase::Starting | Phase::Exited => None,
        Phase::Ready => launch.ready_ms.or(telemetry.up_since_ms),
        Phase::External => telemetry.up_since_ms,
    }
}

/// Builds the demo server's router.
pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(ui::index))
        .route("/app.js", get(ui::script))
        .route("/app.css", get(ui::stylesheet))
        .route("/reference", get(ui::reference))
        .route("/reference.js", get(ui::reference_script))
        .route("/api/reference", get(reference))
        .route("/api/state", get(state))
        .route("/api/compile", post(compile_source))
        .route("/api/publish", post(publish))
        .route("/api/revoke", post(revoke))
        .route("/api/destinations", post(add_destination))
        .route("/api/destinations/{name}", delete(remove_destination))
        .route("/api/native", post(native))
        .route("/api/default-program", get(default_program))
        .route("/api/traffic", post(traffic))
        .route("/api/intakes/{name}/pause", post(pause_intake))
        .route("/api/intakes/{name}/recent", get(intake_recent))
        .route("/api/canary", get(canary_range))
        .route("/api/adp/{route}", get(adp_route))
        .with_state(app)
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// Everything the UI shows.
#[derive(Serialize)]
struct StateResponse {
    now_ms: u64,
    run_dir: String,
    ports: DemoPorts,
    poll_interval_ms: u64,
    aggregate_window_secs: u64,
    adp: LaunchView,
    control: ControlView,
    legend: Option<Legend>,
    channel: ChannelView,
    intakes: Vec<IntakeView>,
    native: NativeView,
    telemetry: TelemetrySnapshot,
    traffic: TrafficState,
    canary: CanaryCheck,
    /// The width the canary check's ranges are aligned to, in seconds, or `null` while the bucket widths in use share
    /// no boundary the check can wait for.
    canary_width_secs: Option<u64>,
    compiled: Option<CompiledView>,
    recipients: RecipientsView,
    lanes: LanesView,
}

#[derive(Serialize)]
struct CompiledView {
    artifact_hash: String,
    compiled_ms: u64,
}

/// One intake, as the UI shows it: what it received, and what ADP makes of its endpoint.
#[derive(Serialize)]
struct IntakeView {
    #[serde(flatten)]
    summary: IntakeSummary,

    /// When its removal from ADP's configuration started, while its intake waits for ADP to retire its recipient.
    removing_since_ms: Option<u64>,

    /// Whether ADP's recipient directory, in the export in hand, names its endpoint: `false` for a destination ADP has
    /// not read from its configuration yet, or no longer declares. `None` without an export.
    in_directory: Option<bool>,

    /// Whether the active plan in hand has an `emit` whose output delivers to its endpoint. `false` is a destination
    /// that is declared but not addressed: under a published program, one added after the program was written. `None`
    /// without an active plan.
    addressed: Option<bool>,
}

/// Returns whether ADP's recipient directory in `export` names the endpoint at `url`.
fn in_directory(export: &ExportBundle, url: &str) -> bool {
    export.catalog.recipients().any(|(_, entry)| entry.endpoint == url)
}

/// Returns whether `plan` has an `emit` whose output delivers to the endpoint at `url`: one of the output's recipients
/// is the endpoint's, whose identity ends with `/<url>` (`dd_out/additional/0/http://127.0.0.1:17102`).
fn addressed(plan: &Plan, url: &str) -> bool {
    let suffix = format!("/{}", url);
    plan.nodes
        .iter()
        .filter(|node| node.op.as_str() == saluki_metric_plan::operators::names::EMIT)
        .filter_map(|node| node.params.get("output")?.as_str())
        .filter_map(|output| plan.outputs.iter().find(|declared| declared.id.as_str() == output))
        .any(|output| {
            output
                .recipients
                .iter()
                .any(|recipient| recipient.as_str().ends_with(&suffix))
        })
}

async fn state(State(app): State<App>) -> Json<StateResponse> {
    let now_ms = unix_millis();
    let compiled = app
        .compiled
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(|compiled| CompiledView {
            artifact_hash: compiled.artifact.hash.to_string(),
            compiled_ms: compiled.compiled_ms,
        });
    let adp = app.launch.view();
    let telemetry = app.telemetry.snapshot(now_ms);
    let (layout, layout_error) = app.control.lane_layout();
    let lanes = lanes::view(layout.as_deref(), layout_error, &telemetry);
    let export = app.control.export();
    let active_plan = app
        .control
        .reported_active_plan_hash()
        .and_then(|hash| app.control.active_plan(&hash));
    let intakes = app
        .destinations
        .intakes_with_removal()
        .into_iter()
        .map(|(intake, removing_since_ms)| {
            let url = intake.recipient().url();
            IntakeView {
                summary: intake.summary(now_ms),
                removing_since_ms,
                in_directory: export.as_deref().map(|export| in_directory(export, &url)),
                addressed: active_plan.as_deref().map(|plan| addressed(plan, &url)),
            }
        })
        .collect();
    Json(StateResponse {
        now_ms,
        run_dir: app.settings.run_dir.display().to_string(),
        ports: app.settings.ports.clone(),
        poll_interval_ms: app.settings.poll_interval.as_millis() as u64,
        aggregate_window_secs: app.settings.aggregate_window_secs,
        control: app.control.view(),
        legend: export.map(|export| export.legend(&app.recipients())),
        channel: app.channel.view(TIMELINE_LIMIT, app.reported_high_water()),
        intakes,
        native: app.destinations.native(),
        traffic: app.traffic.state(),
        canary: app.canary_check(now_ms, &adp, &telemetry),
        canary_width_secs: app.canary_width(now_ms),
        recipients: app.control.recipients(),
        lanes,
        adp,
        telemetry,
        compiled,
    })
}

#[derive(Deserialize)]
struct CompileRequest {
    source: String,
}

async fn compile_source(State(app): State<App>, Json(request): Json<CompileRequest>) -> Json<CompileOutcome> {
    let status = app.latest_status();
    let active_plan_hash = status.as_ref().and_then(|status| status.active_plan_hash.clone());
    // The active plan the diff compares against: the one ADP reports now, fetched if the one in hand is another.
    let active_plan = match active_plan_hash
        .as_deref()
        .and_then(|hash| hash.parse::<PlanHash>().ok())
    {
        Some(hash) => app
            .control
            .refresh_active_plan(&app.api, &hash)
            .await
            .map(|plan| (hash, plan)),
        None => None,
    };
    // The source is checked against the export of the default program's one scope, which every artifact replaces; one
    // that names another scope is refused by composition, which names the scope ADP declares. ADP composes it with the
    // artifacts it runs for any other scope, which, with one scope declared, are none.
    // The editor prints programs without their base, which only the scope's name is read past here.
    let any_base = saluki_metric_plan_syntax::ParseOptions {
        base: Some(saluki_metric_plan::ir::ArtifactBase {
            program_hash: PlanHash::from_digest([0; 32]),
            outputs: Default::default(),
        }),
    };
    let scope = saluki_metric_plan_syntax::parse_with(&request.source, &any_base)
        .map(|parsed| parsed.artifact.scope.to_string())
        .unwrap_or_else(|_| POLICY_SCOPE.to_string());
    let exported = app.control.export_with_default_program();
    let others = status
        .as_ref()
        .map(|status| other_scope_artifacts(status, &app.channel.envelopes(), &scope))
        .unwrap_or_default();
    let labels = app.labels();
    let context = exported.as_ref().map(|(export, default_program)| CompileContext {
        export,
        default_program: default_program.as_deref(),
        others: &others,
        active: active_plan.as_ref().map(|(hash, plan)| ActivePlan {
            plan,
            hash: *hash,
            generation: status.as_ref().and_then(|status| status.active_generation),
            artifact_revision: status
                .as_ref()
                .and_then(|status| status.active_artifact.as_ref())
                .and_then(|artifact| artifact.revision),
        }),
        revision: app.channel.next_revision(app.reported_high_water()),
        labels: &labels,
        in_effect: status
            .as_ref()
            .and_then(|status| status.active_artifact.as_ref())
            .and_then(|artifact| artifact.artifact_hash.as_deref())
            .and_then(|hash| hash.parse().ok()),
    });
    let (outcome, artifact) = compile(&request.source, context.as_ref());
    *app.compiled.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = artifact.map(|artifact| Compiled {
        artifact,
        compiled_ms: unix_millis(),
        active_plan_hash,
    });
    Json(outcome)
}

#[derive(Deserialize)]
struct PublishRequest {
    artifact_hash: String,
}

async fn publish(State(app): State<App>, Json(request): Json<PublishRequest>) -> Response {
    // The approval is used up here, under the lock, so that a repeated or concurrent approval of the same compile
    // cannot publish it twice as two revisions.
    let taken = {
        let mut slot = app.compiled.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match slot.as_ref().map(|compiled| compiled.artifact.hash.to_string()) {
            Some(hash) if hash == request.artifact_hash => slot.take(),
            Some(hash) => {
                return error_response(
                    StatusCode::CONFLICT,
                    format!(
                        "The approved hash {} is not the hash of the last compiled artifact, {}. Compile again and \
                         approve the new hash.",
                        request.artifact_hash, hash
                    ),
                )
            }
            None => None,
        }
    };
    let Some(compiled) = taken else {
        let served = app
            .channel
            .configs()
            .into_iter()
            .find(|config| config.artifact_hash == request.artifact_hash);
        let message = match served {
            Some(current) => format!(
                "This approval was already used: the stub serves {} as revision {}. Compile again to publish it as \
                 another revision.",
                current.artifact_hash, current.revision
            ),
            None => "There is no compiled artifact to publish. Compile the program, review its hash, and publish that \
                     hash."
                .to_string(),
        };
        return error_response(StatusCode::CONFLICT, message);
    };
    let approved_against = compiled
        .active_plan_hash
        .clone()
        .map(|active_plan_hash| ApprovedAgainst { active_plan_hash });
    // The widths served until now, which the publish may replace, and those it serves from now on, whether or not the
    // state is read in between.
    app.note_served_widths(unix_millis());
    let published = app.channel.publish(
        compiled.artifact.document.clone(),
        approved_against,
        app.reported_high_water(),
        unix_millis(),
    );
    app.note_served_widths(unix_millis());
    match published {
        Ok(config) => Json(json!({
            "revision": config.revision,
            "artifact_hash": config.artifact_hash,
            "sha256": config.sha256,
        }))
        .into_response(),
        Err(error) => {
            // Nothing was published, so the approval still stands, unless another compile replaced it meanwhile.
            app.compiled
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get_or_insert(compiled);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", error))
        }
    }
}

/// Revokes the policy artifact: the stub then serves an empty policy snapshot, and ADP runs the default program again.
async fn revoke(State(app): State<App>) -> Response {
    // What the stub served until now was served until the revoke.
    app.note_served_widths(unix_millis());
    match app.channel.revoke(None, unix_millis()) {
        Ok(true) => Json(json!({ "revoked": true })).into_response(),
        Ok(false) => Json(json!({
            "revoked": false,
            "message": "The snapshot is already empty: nothing to revoke.",
        }))
        .into_response(),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", error)),
    }
}

async fn add_destination(State(app): State<App>) -> Response {
    match app.destinations.add() {
        Ok(recipient) => Json(DestinationView::from(&recipient)).into_response(),
        Err(error) => error_response(StatusCode::CONFLICT, format!("{:#}", error)),
    }
}

async fn remove_destination(State(app): State<App>, Path(name): Path<String>) -> Response {
    match app.destinations.remove(&name) {
        Ok(recipient) => Json(json!({
            "removing": true,
            "destination": DestinationView::from(&recipient),
        }))
        .into_response(),
        Err(RemoveError::NotFound) => error_response(
            StatusCode::NOT_FOUND,
            format!("There is no destination named `{}`.", name),
        ),
        Err(RemoveError::Write(error)) => error_response(StatusCode::INTERNAL_SERVER_ERROR, error),
        Err(error) => error_response(StatusCode::CONFLICT, error.to_string()),
    }
}

async fn native(State(app): State<App>, Json(change): Json<NativeChange>) -> Response {
    match app.destinations.set_native(change) {
        Ok(native) => Json(native).into_response(),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", error)),
    }
}

/// The language reference, with what this ADP accepts when its export is at hand.
async fn reference(State(app): State<App>) -> Json<crate::reference::Reference> {
    let labels = app.labels();
    Json(match app.control.refresh_export(&app.api).await {
        Surface::Available(export) => crate::reference::reference(Some(&export), &labels, None),
        Surface::NotExposed => crate::reference::reference(None, &labels, Some(EXPORT_NOT_EXPOSED_MESSAGE.to_string())),
        Surface::Failed(error) => crate::reference::reference(
            None,
            &labels,
            Some(format!("The export could not be loaded from ADP: {}", error)),
        ),
    })
}

async fn default_program(State(app): State<App>) -> Json<Value> {
    Json(match app.control.refresh_export(&app.api).await {
        Surface::Available(export) => json!({
            "available": true,
            "source": default_program_source(&export, &app.labels()),
            "scope": export.scope,
            "program_hash": export.program_hash().to_string(),
            "legend": export.legend(&app.recipients()),
        }),
        Surface::NotExposed => json!({ "available": false, "message": EXPORT_NOT_EXPOSED_MESSAGE }),
        Surface::Failed(error) => json!({
            "available": false,
            "message": format!("The default program could not be loaded from ADP: {}", error),
        }),
    })
}

#[derive(Deserialize)]
struct TrafficRequest {
    running: Option<bool>,
    rate: Option<f64>,
    mix: Option<TrafficMix>,
}

async fn traffic(State(app): State<App>, Json(request): Json<TrafficRequest>) -> Json<TrafficState> {
    Json(app.traffic.apply(TrafficChange {
        running: request.running,
        rate: request.rate,
        mix: request.mix,
    }))
}

#[derive(Deserialize)]
struct PauseRequest {
    paused: bool,
}

async fn pause_intake(State(app): State<App>, Path(name): Path<String>, Json(request): Json<PauseRequest>) -> Response {
    match app.destinations.find(&name) {
        Some(intake) => {
            intake.set_paused(request.paused);
            Json(json!({ "name": name, "paused": request.paused })).into_response()
        }
        None => error_response(StatusCode::NOT_FOUND, format!("There is no intake named `{}`.", name)),
    }
}

async fn intake_recent(State(app): State<App>, Path(name): Path<String>) -> Response {
    match app.destinations.find(&name) {
        Some(intake) => Json(json!({
            "series": intake.recent_series(RECENT_LIMIT),
            "requests": intake.recent_requests(RECENT_LIMIT),
        }))
        .into_response(),
        None => error_response(StatusCode::NOT_FOUND, format!("There is no intake named `{}`.", name)),
    }
}

#[derive(Deserialize)]
struct CanaryQuery {
    from_s: u64,
    to_s: u64,
}

async fn canary_range(State(app): State<App>, Query(query): Query<CanaryQuery>) -> Response {
    let now_ms = unix_millis();
    if let Err((oldest, latest)) = canary::readable((query.from_s, query.to_s), now_ms) {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "The range must be a nonempty range of seconds within the last {} seconds that every flush has had \
                 time to deliver: from {} at the earliest, to {} at the latest. A range that does not start and end \
                 at bucket boundaries counts parts of buckets.",
                CANARY_HISTORY_SECS, oldest, latest
            ),
        );
    }
    let telemetry = app.telemetry.snapshot(now_ms);
    let check = app.canary_check_range((query.from_s, query.to_s), &app.launch.view(), &telemetry);
    let received_by_timestamp = app
        .destinations
        .intakes()
        .iter()
        .find(|intake| intake.recipient().kind == RecipientKind::Primary)
        .map(|intake| intake.canaries_by_timestamp(query.from_s, query.to_s))
        .unwrap_or_default();
    Json(CanaryRange {
        check,
        sent_by_second: app.traffic.canaries_sent_by_second(query.from_s, query.to_s),
        received_by_timestamp,
    })
    .into_response()
}

/// The canary check over a range of seconds, with what was sent in each second of it and what the primary endpoint
/// received for each bucket start in it.
#[derive(Serialize)]
struct CanaryRange {
    #[serde(flatten)]
    check: CanaryCheck,

    /// The canaries sent in each second of the range, as `[second, canaries]`.
    sent_by_second: Vec<(u64, u64)>,

    /// The canaries the primary endpoint received with each point timestamp in the range, as `[timestamp, count]`.
    received_by_timestamp: Vec<(u64, f64)>,
}

async fn adp_route(State(app): State<App>, Path(route): Path<String>) -> Response {
    let (path, content_type) = match route.as_str() {
        "status" => (routes::STATUS, "application/json"),
        "export" => (routes::EXPORT, "application/json"),
        "plan" => (routes::PLAN, "application/json"),
        "default-program" => (routes::DEFAULT_PROGRAM, "application/json"),
        "explain" => (routes::EXPLAIN, "application/json"),
        "explain-text" => (routes::EXPLAIN_TEXT, "text/plain; charset=utf-8"),
        _ => {
            return error_response(
                StatusCode::NOT_FOUND,
                format!("There is no ADP route named `{}`.", route),
            )
        }
    };
    match app.api.control(path).await {
        Surface::Available(body) => ([(header::CONTENT_TYPE, content_type)], body).into_response(),
        Surface::NotExposed => error_response(
            StatusCode::NOT_FOUND,
            format!("ADP does not expose {} yet (it answered 404 Not Found).", path),
        ),
        Surface::Failed(error) => error_response(StatusCode::BAD_GATEWAY, error),
    }
}

#[cfg(test)]
mod tests;
