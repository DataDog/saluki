//! The demo server: the UI page and the JSON API it uses.
//!
//! - `GET /` (and `/app.js`, `/app.css`): the UI, embedded in the binary.
//! - `GET /api/state`: everything the UI shows, polled about once per second.
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
//! - `POST /api/traffic` `{running?, rate?}`: starts, pauses, or changes the rate of the traffic generator.
//! - `POST /api/intakes/{name}/pause` `{paused}`: pauses an intake, which then answers `503 Service Unavailable`, or
//!   resumes it.
//! - `GET /api/intakes/{name}/recent`: the most recent series and requests an intake received.
//! - `GET /api/canary?from_s=<s>&to_s=<s>`: the canary conservation check over a range of seconds (Unix seconds,
//!   within the last five minutes), such as the whole of a scripted run.
//! - `GET /api/adp/{status,export,plan,default-program,explain,explain-text}`: ADP's control surface routes, passed
//!   through.

use std::sync::{Arc, Mutex};

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
    canary::{self, CanaryCheck, CanaryElsewhere},
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
        protocol::{ApprovedAgainst, ChannelStatus, POLICY_SCOPE},
        ChannelView, StubChannel,
    },
    telemetry::{TelemetryScraper, TelemetrySnapshot},
    traffic::{TrafficChange, TrafficGenerator, TrafficState, CANARY_HISTORY_SECS},
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

    /// Runs the canary conservation check at `now_ms`.
    fn canary_check(&self, now_ms: u64, launch: &LaunchView, telemetry: &TelemetrySnapshot) -> CanaryCheck {
        let range = canary::window(now_ms, self.settings.aggregate_window_secs);
        self.canary_check_range(range, launch, telemetry)
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
        // ADP counts as up while it answers its API, and, when the demo launched it, only once it reported ready.
        let up_since_ms = match launch.phase {
            Phase::Starting | Phase::Exited => None,
            Phase::Ready | Phase::External => telemetry.up_since_ms,
        };
        canary::check(
            range,
            self.traffic.canaries_sent_between(from_s, to_s),
            received,
            elsewhere,
            self.traffic.state().rate,
            up_since_ms,
        )
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
    compiled: Option<CompiledView>,
    resources_checked: Option<CompiledView>,
    recipients: RecipientsView,
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
        recipients: app.control.recipients(),
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
    match app.channel.publish(
        compiled.artifact.document.clone(),
        approved_against,
        app.reported_high_water(),
        unix_millis(),
    ) {
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
}

async fn traffic(State(app): State<App>, Json(request): Json<TrafficRequest>) -> Json<TrafficState> {
    Json(app.traffic.apply(TrafficChange {
        running: request.running,
        rate: request.rate,
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
    let latest = canary::window(now_ms, app.settings.aggregate_window_secs).1;
    let oldest = (now_ms / 1000).saturating_sub(CANARY_HISTORY_SECS);
    if query.from_s >= query.to_s || query.from_s < oldest || query.to_s > latest {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "The range must be a nonempty range of seconds within the last {} seconds that every flush has had \
                 time to deliver: from {} at the earliest, to {} at the latest.",
                CANARY_HISTORY_SECS, oldest, latest
            ),
        );
    }
    let telemetry = app.telemetry.snapshot(now_ms);
    Json(app.canary_check_range((query.from_s, query.to_s), &app.launch.view(), &telemetry)).into_response()
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
