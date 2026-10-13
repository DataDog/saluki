//! The demo server: the UI page and the JSON API it uses.
//!
//! - `GET /` (and `/app.js`, `/app.css`): the UI, embedded in the binary.
//! - `GET /api/state`: everything the UI shows, polled about once per second, the Lanes panel's `lanes` included.
//! - `POST /api/compile` `{source, resources?}`: parses a program, composes it into ADP's default program, with the
//!   artifacts ADP runs for the other scopes, and validates the composed plan as ADP will, against the export of the
//!   scope it replaces; returns diagnostics, the artifact, its hash, its canonical source, and the plan diff against
//!   the active plan. `resources`, the Resources tab's draft as text, gives the fingerprints of declared outputs the
//!   export does not declare yet.
//! - `POST /api/publish` `{artifact_hash}`: publishes the last compiled artifact as the next revision, if
//!   `artifact_hash` is its hash. An approval is used once: publishing again needs another compile.
//! - `POST /api/revoke` (optionally `{scope}`): empties the snapshot, so that ADP goes back to the default program, or
//!   removes one scope's config, so that ADP runs the default program's region there.
//! - `GET /api/resources/template`: the team D resources template, and the resources document the stub serves, if
//!   any, as the Resources tab's editor shows documents.
//! - `POST /api/resources/check` `{document}`: checks a resources draft (its hash, validation as ADP does it, and the
//!   fingerprints of the outputs it declares), and keeps it for approval.
//! - `POST /api/resources/publish` `{artifact_hash}`: publishes the last checked resources draft as the next resources
//!   revision, if `artifact_hash` is its hash: an approval of its own, used once.
//! - `POST /api/resources/revoke`: empties the resources snapshot, revoking everything it declared.
//! - `GET /api/default-program[?scope=<scope>]`: the export bundle's artifact printed as source, with its legend, or
//!   why there is none: of the policy scope, or of the scope named.
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
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use saluki_metric_plan::canonical::PlanHash;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    adp_api::{routes, AdpApi, Surface},
    canary::{self, CanaryCheck, CanaryElsewhere, CanaryState, Delivery},
    clock::unix_millis,
    compile::{compile_with_resources, default_program_source, CompileOutcome, CompiledArtifact},
    control::{ControlSurface, ControlView, EXPORT_NOT_EXPOSED_MESSAGE},
    export::Legend,
    intake::{IntakeRecorder, IntakeSummary},
    launcher::{Launch, LaunchView, Phase},
    preview::{other_scope_artifacts, ActivePlan, CompileContext},
    recipients::RecipientsView,
    resources::{self, document_text, team_d_template, DraftFingerprints, ResourcesCheck},
    settings::{DemoPorts, Recipient, RunSettings},
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

/// The last resources draft checked that can be published, which a resources publish approves by its hash.
#[derive(Clone, Debug)]
struct CheckedResources {
    document: Value,
    hash: PlanHash,
    checked_ms: u64,
}

/// Everything the demo server's handlers share.
#[derive(Clone)]
pub struct App {
    settings: Arc<RunSettings>,
    intakes: Vec<IntakeRecorder>,
    traffic: TrafficGenerator,
    telemetry: TelemetryScraper,
    channel: StubChannel,
    control: ControlSurface,
    launch: Launch,
    api: AdpApi,
    compiled: Arc<Mutex<Option<Compiled>>>,
    resources_checked: Arc<Mutex<Option<CheckedResources>>>,
    /// The bucket widths that the artifacts of the pre-aggregation scope the stub served set, by revision (see
    /// [`App::canary_widths`]).
    served_widths: Arc<Mutex<BTreeMap<u64, ServedWidths>>>,
}

/// The bucket widths one artifact of the pre-aggregation scope sets, and when the stub last served it.
#[derive(Clone, Debug)]
struct ServedWidths {
    widths: Vec<u64>,
    last_served_ms: u64,
}

/// The demo's components, as the server uses them.
pub struct Components {
    /// The run's settings.
    pub settings: Arc<RunSettings>,

    /// The recording intakes, in the order the UI shows them.
    pub intakes: Vec<IntakeRecorder>,

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
            intakes: components.intakes,
            traffic: components.traffic,
            telemetry: components.telemetry,
            channel: components.channel,
            control: components.control,
            launch: components.launch,
            api: components.api,
            compiled: Arc::new(Mutex::new(None)),
            resources_checked: Arc::new(Mutex::new(None)),
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

    /// Returns the resources revision high-water mark the privileged status route reported, which publishing resources
    /// counts.
    fn reported_resources_high_water(&self) -> Option<u64> {
        self.control
            .status()
            .and_then(|(_, status)| status.status.resources?.revision_high_water)
    }

    /// Returns the intake that stands in for team D's endpoint, which the resources template declares.
    fn team_d(&self) -> Option<Recipient> {
        self.recipients().into_iter().find(|recipient| recipient.declared)
    }

    fn recipients(&self) -> Vec<Recipient> {
        self.intakes.iter().map(|intake| *intake.recipient()).collect()
    }

    /// Notes, as served at `now_ms`, the bucket widths that the artifacts the stub serves for the pre-aggregation scope
    /// set (see [`canary::artifact_widths`]).
    fn note_served_widths(&self, now_ms: u64) {
        let served = self
            .channel
            .envelopes()
            .into_iter()
            .filter(|(scope, _)| scope == canary::WIDTH_SCOPE)
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
    /// to at `now_ms`, in increasing order: those set by every artifact of the pre-aggregation scope that the stub
    /// served within the last [`CANARY_HISTORY_SECS`], unless ADP refused it.
    ///
    /// A width stays for that long after its artifact is revoked or replaced: its buckets stay in the history the
    /// check reads, and the cut back to another width takes effect only at a common multiple of the two. Policy
    /// artifacts never set a width, since ADP refuses an aggregation in the policy scope.
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
        for intake in &self.intakes {
            let count = intake.canaries_between(from_s, to_s);
            if intake.recipient().name == "primary" {
                received = count;
            } else if count != 0.0 {
                elsewhere.push(CanaryElsewhere {
                    intake: intake.recipient().name,
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
        .route("/api/state", get(state))
        .route("/api/compile", post(compile_source))
        .route("/api/publish", post(publish))
        .route("/api/revoke", post(revoke))
        .route("/api/resources/template", get(resources_template))
        .route("/api/resources/check", post(check_resources))
        .route("/api/resources/publish", post(publish_resources))
        .route("/api/resources/revoke", post(revoke_resources))
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
    intakes: Vec<IntakeSummary>,
    telemetry: TelemetrySnapshot,
    traffic: TrafficState,
    canary: CanaryCheck,
    /// The width the canary check's ranges are aligned to, in seconds, or `null` while the bucket widths in use share
    /// no boundary the check can wait for.
    canary_width_secs: Option<u64>,
    compiled: Option<CompiledView>,
    resources_checked: Option<CompiledView>,
    recipients: RecipientsView,
    lanes: LanesView,
}

#[derive(Serialize)]
struct CompiledView {
    artifact_hash: String,
    compiled_ms: u64,
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
    let resources_checked = app
        .resources_checked
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(|checked| CompiledView {
            artifact_hash: checked.hash.to_string(),
            compiled_ms: checked.checked_ms,
        });
    let adp = app.launch.view();
    let telemetry = app.telemetry.snapshot(now_ms);
    let (layout, layout_error) = app.control.lane_layout();
    let lanes = lanes::view(layout.as_deref(), layout_error, &telemetry);
    Json(StateResponse {
        now_ms,
        run_dir: app.settings.run_dir.display().to_string(),
        ports: app.settings.ports.clone(),
        poll_interval_ms: app.settings.poll_interval.as_millis() as u64,
        aggregate_window_secs: app.settings.aggregate_window_secs,
        control: app.control.view(),
        legend: app.control.export().map(|export| export.legend(&app.recipients())),
        channel: app.channel.view(
            TIMELINE_LIMIT,
            app.reported_high_water(),
            app.reported_resources_high_water(),
        ),
        intakes: app.intakes.iter().map(|intake| intake.summary(now_ms)).collect(),
        traffic: app.traffic.state(),
        canary: app.canary_check(now_ms, &adp, &telemetry),
        canary_width_secs: app.canary_width(now_ms),
        recipients: app.control.recipients(),
        lanes,
        adp,
        telemetry,
        compiled,
        resources_checked,
    })
}

#[derive(Deserialize)]
struct CompileRequest {
    source: String,

    /// The Resources tab's draft, as text: the fingerprints of the outputs it declares stand in for those the export
    /// does not declare yet.
    resources: Option<String>,
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
    // The scope the source replaces, the policy scope's unless it names another: its export is the one to check
    // against, and ADP composes it with the artifacts it runs for the other scopes.
    let scope = saluki_metric_plan_syntax::parse(&request.source)
        .map(|parsed| parsed.artifact.scope.to_string())
        .unwrap_or_else(|_| POLICY_SCOPE.to_string());
    let exported = app.control.export_of_scope(&scope);
    let others = status
        .as_ref()
        .map(|status| other_scope_artifacts(status, &app.channel.envelopes(), &scope))
        .unwrap_or_default();
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
    });
    let draft = request
        .resources
        .as_deref()
        .map(DraftFingerprints::of_draft)
        .unwrap_or_default();
    let (outcome, artifact) = compile_with_resources(&request.source, context.as_ref(), &draft);
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

/// A revocation: of one scope's config, or of the whole snapshot when the request names no scope or has no body.
#[derive(Default, Deserialize)]
struct RevokeRequest {
    scope: Option<String>,
}

async fn revoke(State(app): State<App>, body: Bytes) -> Response {
    let request = if body.is_empty() {
        RevokeRequest::default()
    } else {
        match serde_json::from_slice::<RevokeRequest>(&body) {
            Ok(request) => request,
            Err(error) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "The revoke request is not a JSON object with an optional `scope`: {}. Send no body to revoke \
                         every scope.",
                        error
                    ),
                )
            }
        }
    };
    let scope = request.scope.as_deref();
    // What the stub served until now was served until the revoke.
    app.note_served_widths(unix_millis());
    match app.channel.revoke(scope, unix_millis()) {
        Ok(true) => Json(json!({ "revoked": true, "scope": scope })).into_response(),
        Ok(false) => {
            let message = match scope {
                Some(scope) => format!("The snapshot holds no config for scope `{}`: nothing to revoke.", scope),
                None => "The snapshot is already empty: nothing to revoke.".to_string(),
            };
            Json(json!({ "revoked": false, "scope": scope, "message": message })).into_response()
        }
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", error)),
    }
}

async fn resources_template(State(app): State<App>) -> Json<Value> {
    let template = app.team_d().map(|team_d| document_text(&team_d_template(&team_d)));
    let served = app.channel.resources().map(|(config, document)| {
        json!({
            "revision": config.revision,
            "artifact_hash": config.artifact_hash,
            "document": document_text(&document),
        })
    });
    Json(json!({ "template": template, "served": served }))
}

#[derive(Deserialize)]
struct CheckResourcesRequest {
    /// The draft, as text.
    document: String,
}

async fn check_resources(State(app): State<App>, Json(request): Json<CheckResourcesRequest>) -> Json<ResourcesCheck> {
    let (check, checked) = resources::check(&request.document);
    // The last check is the one a publish approves; a draft that cannot be published replaces it with nothing.
    *app.resources_checked
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = checked.map(|(document, hash)| CheckedResources {
        document,
        hash,
        checked_ms: unix_millis(),
    });
    Json(check)
}

async fn publish_resources(State(app): State<App>, Json(request): Json<PublishRequest>) -> Response {
    // Used up under the lock, as a policy approval is, so that it publishes one revision.
    let taken = {
        let mut slot = app
            .resources_checked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match slot.as_ref().map(|checked| checked.hash.to_string()) {
            Some(hash) if hash == request.artifact_hash => slot.take(),
            Some(hash) => {
                return error_response(
                    StatusCode::CONFLICT,
                    format!(
                        "The approved hash {} is not the hash of the last checked resources draft, {}. Check the \
                         draft again and approve the new hash.",
                        request.artifact_hash, hash
                    ),
                )
            }
            None => None,
        }
    };
    let Some(checked) = taken else {
        let message = match app.channel.resources() {
            Some((served, _)) if served.artifact_hash == request.artifact_hash => format!(
                "This approval was already used: the stub serves {} as resources revision {}. Check the draft again \
                 to publish it as another revision.",
                served.artifact_hash, served.revision
            ),
            _ => "There is no checked resources draft to publish. Check the draft, review its hash, and publish that \
                  hash."
                .to_string(),
        };
        return error_response(StatusCode::CONFLICT, message);
    };
    match app.channel.publish_resources(
        checked.document.clone(),
        app.reported_resources_high_water(),
        unix_millis(),
    ) {
        Ok(config) => Json(json!({
            "revision": config.revision,
            "artifact_hash": config.artifact_hash,
            "sha256": config.sha256,
        }))
        .into_response(),
        Err(error) => {
            // Nothing was published, so the approval still stands, unless another check replaced it meanwhile.
            app.resources_checked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get_or_insert(checked);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", error))
        }
    }
}

async fn revoke_resources(State(app): State<App>) -> Response {
    match app.channel.revoke_resources(unix_millis()) {
        Ok(true) => Json(json!({ "revoked": true })).into_response(),
        Ok(false) => Json(json!({
            "revoked": false,
            "message": "The resources snapshot is already empty: nothing to revoke.",
        }))
        .into_response(),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", error)),
    }
}

/// Which scope of the default program to load: the policy scope unless named.
#[derive(Deserialize)]
struct DefaultProgramQuery {
    scope: Option<String>,
}

async fn default_program(State(app): State<App>, Query(query): Query<DefaultProgramQuery>) -> Json<Value> {
    let scope = query.scope.as_deref().unwrap_or(POLICY_SCOPE);
    Json(match app.control.refresh_export_of(&app.api, scope).await {
        Surface::Available(export) => json!({
            "available": true,
            "source": default_program_source(&export),
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
    match app.intakes.iter().find(|intake| intake.recipient().name == name) {
        Some(intake) => {
            intake.set_paused(request.paused);
            Json(json!({ "name": name, "paused": request.paused })).into_response()
        }
        None => error_response(StatusCode::NOT_FOUND, format!("There is no intake named `{}`.", name)),
    }
}

async fn intake_recent(State(app): State<App>, Path(name): Path<String>) -> Response {
    match app.intakes.iter().find(|intake| intake.recipient().name == name) {
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
        .intakes
        .iter()
        .find(|intake| intake.recipient().name == "primary")
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
