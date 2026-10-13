//! The control surface watcher: polls ADP's programmable-metrics status every second, and keeps the export bundle,
//! the default program, and the active plan current.
//!
//! The export is fetched again when the default program hash ADP's status reports changes from what it reported when
//! the export in hand was fetched, when the resources artifact its active generation was lowered with changes (the
//! catalog then declares other outputs, which leaves the default program hash as it is), and at least every
//! [`EXPORT_MAX_AGE`] otherwise, so the editor's checks follow configuration changes. The watcher compares the
//! reported values with their own earlier values, never with the export's `base.program_hash`, since the protocol does
//! not say that the two are computed alike.
//!
//! The recipients every status lists go into the Recipients panel's record ([`RecipientLog`]).
//!
//! The default program (`GET /programmable-metrics/default-program`), which the editor composes artifacts into, is
//! fetched with every export, and kept only if it is the program the export was taken from: its base program hash is
//! the export's. One that is not (the configuration changed between the two requests) makes the watcher fetch both
//! again. The active plan (`GET /programmable-metrics/plan`), which the editor diffs against, is fetched when the
//! active plan hash the status reports changes.
//!
//! The status is read field by field (see [`AdpStatus::read`]), and the status in hand is kept, with when it was
//! received, when a later request fails, so that the UI can show it as last known.
//!
//! The exports of the default program's other scopes ([`OTHER_SCOPES`]) are fetched with the policy scope's, and kept
//! per scope with why one could not be read, if it could not; the editor checks an artifact against the export of the
//! scope it replaces ([`ControlSurface::export_of_scope`]).
//!
//! The lanes ADP's metrics path was laid out on at boot, which the Lanes panel shows, are read from the physical
//! sections of the explain document (`GET /programmable-metrics/explain`) whenever the active generation ADP reports
//! differs from the one they were read at, which a restart with other lane counts also does
//! ([`ControlSurface::lane_layout`]).

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use saluki_metric_plan::{
    canonical::{PlanHash, ResolvedValues},
    export::base,
    ir::{Plan, ScopeName},
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    adp_api::{routes, AdpApi, Surface, SurfaceState},
    clock::unix_millis,
    export::ExportBundle,
    recipients::{RecipientLog, RecipientsView},
    stub::protocol::{AdpStatus, POLICY_SCOPE},
    telemetry::lanes::LaneLayout,
};

/// How often the watcher asks for ADP's status.
pub const STATUS_INTERVAL: Duration = Duration::from_secs(1);

/// How old the export may get before the watcher fetches it again, in milliseconds.
pub const EXPORT_MAX_AGE: u64 = 30_000;

/// How long the watcher waits before asking again for an export that was not available, in milliseconds.
const EXPORT_RETRY: u64 = 5_000;

/// The message the UI shows while ADP does not serve the control surface.
pub const NOT_EXPOSED_MESSAGE: &str = "ADP does not expose the programmable-metrics control surface yet: \
     GET /programmable-metrics/status answered 404 Not Found.";

/// What the not-exposed state means for the demo, which the UI shows once, with [`NOT_EXPOSED_MESSAGE`].
pub const NOT_EXPOSED_DETAIL: &str = "The control surface, the export bundle, and the artifact channel arrive with \
     stage 5. Until then the editor parses and prints programs, a publish only changes the stub's snapshot, and the \
     telemetry viewer shows what reaches each intake.";

/// The message the editor shows when it asks for the default program while ADP does not serve the export.
pub const EXPORT_NOT_EXPOSED_MESSAGE: &str = "No default program to load: ADP does not serve its export yet \
     (GET /programmable-metrics/export answered 404 Not Found).";

/// The scopes of the demo's default program besides the policy scope, whose exports the watcher keeps too.
pub const OTHER_SCOPES: &[&str] = &["dogstatsd_pre_aggregation"];

/// The export of one scope besides the policy scope, as the watcher keeps it.
#[derive(Clone, Default)]
struct ScopeExport {
    state: Option<SurfaceState>,
    export: Option<Arc<ExportBundle>>,
    error: Option<String>,
    fetched_ms: Option<u64>,
}

/// What the watcher knows of one scope's export, as the UI shows it.
#[derive(Clone, Debug, Serialize)]
pub struct ScopeExportView {
    /// The scope.
    pub scope: String,

    /// Whether the export route answered for the scope.
    pub state: SurfaceState,

    /// The base program hash of the export in hand.
    pub program_hash: Option<String>,

    /// Why the export route, or reading the bundle, failed, if it did.
    pub error: Option<String>,

    /// When the export in hand was fetched.
    pub fetched_ms: Option<u64>,
}

#[derive(Default)]
struct ControlState {
    status_state: Option<SurfaceState>,
    status: Option<AdpStatus>,
    status_notes: Vec<String>,
    status_error: Option<String>,
    status_checked_ms: Option<u64>,
    status_received_ms: Option<u64>,
    export_state: Option<SurfaceState>,
    export: Option<Arc<ExportBundle>>,
    export_error: Option<String>,
    export_checked_ms: Option<u64>,
    export_fetched_ms: Option<u64>,
    /// The default program hash ADP's status reported when the export in hand was fetched.
    export_reported_hash: Option<String>,
    /// The resources artifact ADP's status reported active when the export in hand was fetched.
    export_reported_resources: Option<String>,
    /// The recipients ADP's statuses listed.
    recipients: RecipientLog,
    /// The default program the export in hand was taken from.
    default_program: Option<Arc<Plan>>,
    default_program_state: Option<SurfaceState>,
    default_program_error: Option<String>,
    /// The active plan, with its hash.
    active_plan: Option<(PlanHash, Arc<Plan>)>,
    active_plan_error: Option<String>,
    /// The exports of the other scopes, by scope.
    scope_exports: BTreeMap<String, ScopeExport>,
    /// The lanes ADP's metrics path was laid out on, from its explain document.
    lane_layout: Option<Arc<LaneLayout>>,
    /// Why the lane layout could not be read, if it could not.
    lane_layout_error: Option<String>,
    /// The active generation and the time of the last request for the lane layout.
    lane_layout_asked: Option<(Option<u64>, u64)>,
    /// The active generation the status reported when the lane layout in hand was read.
    lane_layout_generation: Option<u64>,
}

impl ControlState {
    fn reported_hash(&self) -> Option<String> {
        self.status
            .as_ref()
            .and_then(|status| status.status.default_program_hash.clone())
    }

    /// Returns the resources artifact ADP's status reports active, as `<revision> <hash>`, or `None` when none is.
    fn reported_resources(&self) -> Option<String> {
        let artifact = self
            .status
            .as_ref()?
            .status
            .resources
            .as_ref()?
            .active_artifact
            .as_ref()?;
        Some(format!(
            "{} {}",
            artifact.revision.unwrap_or_default(),
            artifact.artifact_hash.as_deref().unwrap_or_default()
        ))
    }
}

/// What the watcher knows, as the UI shows it.
#[derive(Clone, Debug, Serialize)]
pub struct ControlView {
    /// Whether the status route answered.
    pub status_state: SurfaceState,

    /// A message for the UI when the control surface is not available.
    pub message: Option<String>,

    /// What the message means for the demo, when there is more to say.
    pub detail: Option<&'static str>,

    /// ADP's latest status: the last one received, which is older than the last request when that request failed.
    pub status: Option<AdpStatus>,

    /// The fields of the status that could not be read.
    pub status_notes: Vec<String>,

    /// Why the status route failed, if it did.
    pub status_error: Option<String>,

    /// When the status route was last asked.
    pub status_checked_ms: Option<u64>,

    /// When the status in hand was received.
    pub status_received_ms: Option<u64>,

    /// Whether the export route answered.
    pub export_state: SurfaceState,

    /// Why the export route or reading the bundle failed, if it did.
    pub export_error: Option<String>,

    /// When the export in hand was fetched.
    pub export_fetched_ms: Option<u64>,

    /// The default program hash of the export in hand.
    pub export_program_hash: Option<String>,

    /// Whether the default program route answered with the program the export in hand was taken from.
    pub default_program_state: SurfaceState,

    /// Why the default program route, or reading its plan, failed, if it did.
    pub default_program_error: Option<String>,

    /// The hash of the active plan in hand.
    pub active_plan_hash: Option<String>,

    /// Why the plan route, or reading its plan, failed, if it did.
    pub active_plan_error: Option<String>,

    /// The exports of the other scopes, in scope order.
    pub scope_exports: Vec<ScopeExportView>,
}

/// The control surface watcher's record.
///
/// Cheap to clone: clones share the record.
#[derive(Clone, Default)]
pub struct ControlSurface {
    state: Arc<Mutex<ControlState>>,
}

impl ControlSurface {
    /// Creates an empty record.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns ADP's latest status, if the status route answered, with when it was received.
    pub fn status(&self) -> Option<(u64, AdpStatus)> {
        let state = self.lock();
        Some((state.status_received_ms?, state.status.clone()?))
    }

    /// Returns the export bundle in hand, if any.
    pub fn export(&self) -> Option<Arc<ExportBundle>> {
        self.lock().export.clone()
    }

    /// Returns the Recipients panel's record of the recipients ADP's statuses listed.
    pub fn recipients(&self) -> RecipientsView {
        self.lock().recipients.view()
    }

    /// Returns the export bundle in hand with the default program it was taken from, when ADP served that program.
    pub fn export_with_default_program(&self) -> Option<(Arc<ExportBundle>, Option<Arc<Plan>>)> {
        let state = self.lock();
        let export = state.export.clone()?;
        Some((export, state.default_program.clone()))
    }

    /// Returns the export bundle in hand of `scope` with the default program it was taken from, when ADP served that
    /// program: the policy scope's, or that of another scope ([`OTHER_SCOPES`]), whose default program is the one kept
    /// with the policy scope's export if its base of `scope` is the export's.
    ///
    /// A scope the default program does not declare gets the policy scope's: composition then says which scopes it
    /// declares.
    pub fn export_of_scope(&self, scope: &str) -> Option<(Arc<ExportBundle>, Option<Arc<Plan>>)> {
        if scope == POLICY_SCOPE || !OTHER_SCOPES.contains(&scope) {
            return self.export_with_default_program();
        }
        let state = self.lock();
        let export = state.scope_exports.get(scope)?.export.clone()?;
        let default_program = state
            .default_program
            .clone()
            .filter(|plan| matching_base(plan, &export).is_ok());
        Some((export, default_program))
    }

    /// Returns the active plan in hand, if its hash is `hash`.
    pub fn active_plan(&self, hash: &PlanHash) -> Option<Arc<Plan>> {
        let state = self.lock();
        let (held, plan) = state.active_plan.as_ref()?;
        (held == hash).then(|| Arc::clone(plan))
    }

    /// Returns what the watcher knows.
    pub fn view(&self) -> ControlView {
        let state = self.lock();
        let status_state = state.status_state.unwrap_or(SurfaceState::Unknown);
        ControlView {
            status_state,
            message: match status_state {
                SurfaceState::NotExposed => Some(NOT_EXPOSED_MESSAGE.to_string()),
                SurfaceState::Failed => Some(format!(
                    "ADP's privileged API did not answer: {}",
                    state.status_error.as_deref().unwrap_or("unknown error")
                )),
                SurfaceState::Unknown | SurfaceState::Available => None,
            },
            detail: (status_state == SurfaceState::NotExposed).then_some(NOT_EXPOSED_DETAIL),
            status: state.status.clone(),
            status_notes: state.status_notes.clone(),
            status_error: state.status_error.clone(),
            status_checked_ms: state.status_checked_ms,
            status_received_ms: state.status_received_ms,
            export_state: state.export_state.unwrap_or(SurfaceState::Unknown),
            export_error: state.export_error.clone(),
            export_fetched_ms: state.export_fetched_ms,
            export_program_hash: state.export.as_ref().map(|export| export.program_hash().to_string()),
            default_program_state: state.default_program_state.unwrap_or(SurfaceState::Unknown),
            default_program_error: state.default_program_error.clone(),
            active_plan_hash: state.active_plan.as_ref().map(|(hash, _)| hash.to_string()),
            active_plan_error: state.active_plan_error.clone(),
            scope_exports: state
                .scope_exports
                .iter()
                .map(|(scope, export)| ScopeExportView {
                    scope: scope.clone(),
                    state: export.state.unwrap_or(SurfaceState::Unknown),
                    program_hash: export.export.as_ref().map(|export| export.program_hash().to_string()),
                    error: export.error.clone(),
                    fetched_ms: export.fetched_ms,
                })
                .collect(),
        }
    }

    /// Records the answer of the export route for a scope other than the policy scope.
    pub fn record_scope_export(&self, scope: &str, answer: Surface<Value>, at_ms: u64) -> Surface<Arc<ExportBundle>> {
        let answer = answer.map(|value| ExportBundle::from_value(value).map(Arc::new));
        let mut state = self.lock();
        let kept = state.scope_exports.entry(scope.to_string()).or_default();
        kept.state = Some(answer.state());
        match &answer {
            Surface::Available(export) => {
                kept.export = Some(Arc::clone(export));
                kept.fetched_ms = Some(at_ms);
                kept.error = None;
            }
            Surface::NotExposed => {
                kept.export = None;
                kept.error = None;
            }
            Surface::Failed(error) => kept.error = Some(error.clone()),
        }
        answer
    }

    /// Fetches the export of a scope now and records it: the policy scope's with its default program, or that of
    /// another scope.
    pub async fn refresh_export_of(&self, api: &AdpApi, scope: &str) -> Surface<Arc<ExportBundle>> {
        if scope == POLICY_SCOPE {
            return self.refresh_export(api).await;
        }
        let answer = api.control_json::<Value>(&routes::export_of(scope)).await;
        self.record_scope_export(scope, answer, unix_millis())
    }

    /// Records the answer of the status route, reading the status document field by field.
    pub fn record_status(&self, answer: Surface<Value>, at_ms: u64) {
        let mut state = self.lock();
        state.status_state = Some(answer.state());
        state.status_checked_ms = Some(at_ms);
        match answer {
            Surface::Available(document) => {
                let (status, notes) = AdpStatus::read(&document);
                if !notes.is_empty() && notes != state.status_notes {
                    tracing::warn!(notes = %notes.join("; "), "Parts of ADP's status could not be read.");
                }
                state.recipients.observe(status.recipients.as_deref(), at_ms);
                state.status = Some(status);
                state.status_notes = notes;
                state.status_received_ms = Some(at_ms);
                state.status_error = None;
            }
            Surface::NotExposed => {
                state.status = None;
                state.status_notes.clear();
                state.status_received_ms = None;
                state.status_error = None;
            }
            Surface::Failed(error) => state.status_error = Some(error),
        }
    }

    /// Records the answer of the export route.
    pub fn record_export(&self, answer: Surface<Value>, at_ms: u64) -> Surface<Arc<ExportBundle>> {
        let answer = answer.map(|value| ExportBundle::from_value(value).map(Arc::new));
        let mut state = self.lock();
        state.export_state = Some(answer.state());
        state.export_checked_ms = Some(at_ms);
        match &answer {
            Surface::Available(export) => {
                state.export = Some(Arc::clone(export));
                state.export_fetched_ms = Some(at_ms);
                state.export_reported_hash = state.reported_hash();
                state.export_reported_resources = state.reported_resources();
                state.export_error = None;
            }
            Surface::NotExposed => {
                state.export = None;
                state.default_program = None;
                state.export_error = None;
            }
            Surface::Failed(error) => state.export_error = Some(error.clone()),
        }
        answer
    }

    /// Records the answer of the default program route, keeping the program only if the export in hand was taken from
    /// it, and returns whether it was kept.
    ///
    /// A program whose base differs from the export's was lowered from another configuration than the export (the
    /// configuration changed between the two requests): it is refused, and the export is marked to be fetched again.
    pub fn record_default_program(&self, answer: Surface<Value>) -> SurfaceState {
        let mut state = self.lock();
        let answer = answer.map(|value| {
            Plan::from_value(value).map_err(|error| format!("ADP's default program cannot be read: {}", error))
        });
        let answer = match (answer, state.export.clone()) {
            (Surface::Available(plan), Some(export)) => match matching_base(&plan, &export) {
                Ok(()) => Surface::Available(plan),
                Err(error) => {
                    // Fetched again on the next tick, with the export.
                    state.export_reported_hash = None;
                    Surface::Failed(error)
                }
            },
            (Surface::Available(_), None) => {
                Surface::Failed("There is no export in hand to match the default program with.".to_string())
            }
            (other, _) => other,
        };
        let answer_state = answer.state();
        state.default_program_state = Some(answer_state);
        match answer {
            Surface::Available(plan) => {
                state.default_program = Some(Arc::new(plan));
                state.default_program_error = None;
            }
            Surface::NotExposed => {
                state.default_program = None;
                state.default_program_error = None;
            }
            Surface::Failed(error) => {
                state.default_program = None;
                state.default_program_error = Some(error);
            }
        }
        answer_state
    }

    /// Records the answer of the plan route: the active plan, kept with its hash, which is returned.
    pub fn record_active_plan(&self, answer: Surface<Value>) -> Option<(PlanHash, Arc<Plan>)> {
        let answer = answer.map(|value| {
            let plan =
                Plan::from_value(value).map_err(|error| format!("ADP's active plan cannot be read: {}", error))?;
            let hash = plan
                .hash(ResolvedValues::Include)
                .map_err(|error| format!("ADP's active plan cannot be hashed: {}", error))?;
            Ok((hash, Arc::new(plan)))
        });
        let mut state = self.lock();
        match answer {
            Surface::Available(active) => {
                state.active_plan = Some(active.clone());
                state.active_plan_error = None;
                Some(active)
            }
            Surface::NotExposed => {
                state.active_plan = None;
                state.active_plan_error = None;
                None
            }
            Surface::Failed(error) => {
                state.active_plan_error = Some(error);
                None
            }
        }
    }

    /// Returns `true` if the export should be fetched at `now_ms`.
    pub fn export_wanted(&self, now_ms: u64) -> bool {
        let state = self.lock();
        match (state.export.is_some(), state.export_fetched_ms, state.export_checked_ms) {
            (true, Some(fetched), _) => {
                let reported = state.reported_hash();
                (reported.is_some() && reported != state.export_reported_hash)
                    || state.reported_resources() != state.export_reported_resources
                    || now_ms.saturating_sub(fetched) >= EXPORT_MAX_AGE
            }
            (_, _, Some(checked)) => now_ms.saturating_sub(checked) >= EXPORT_RETRY,
            _ => true,
        }
    }

    /// Fetches the export now, and the default program it was taken from, and records them, with the exports of the
    /// other scopes.
    pub async fn refresh_export(&self, api: &AdpApi) -> Surface<Arc<ExportBundle>> {
        let answer = api.control_json::<Value>(routes::EXPORT).await;
        let export = self.record_export(answer, unix_millis());
        if let Surface::Available(_) = export {
            let answer = api.control_json::<Value>(routes::DEFAULT_PROGRAM).await;
            self.record_default_program(answer);
            for scope in OTHER_SCOPES {
                let answer = api.control_json::<Value>(&routes::export_of(scope)).await;
                self.record_scope_export(scope, answer, unix_millis());
            }
        }
        export
    }

    /// Returns the active plan whose hash is `hash`: the one in hand, or, if that is another, the one ADP serves now,
    /// which is `None` if it is not `hash` either (another generation became active meanwhile).
    pub async fn refresh_active_plan(&self, api: &AdpApi, hash: &PlanHash) -> Option<Arc<Plan>> {
        if let Some(plan) = self.active_plan(hash) {
            return Some(plan);
        }
        let answer = api.control_json::<Value>(routes::PLAN).await;
        self.record_active_plan(answer)
            .filter(|(served, _)| served == hash)
            .map(|(_, plan)| plan)
    }

    /// Returns the lanes ADP's metrics path was laid out on, if its explain document said, with why it could not be
    /// read, if it could not.
    pub fn lane_layout(&self) -> (Option<Arc<LaneLayout>>, Option<String>) {
        let state = self.lock();
        (state.lane_layout.clone(), state.lane_layout_error.clone())
    }

    /// Returns `true` if the lane layout should be read at `now_ms`: ADP reports an active generation other than the
    /// one the layout in hand was read at (a new generation, or a restart, which may have changed the lanes), and the
    /// layout was not asked for at this generation within the last [`EXPORT_RETRY`].
    pub fn lane_layout_wanted(&self, now_ms: u64) -> bool {
        let state = self.lock();
        let Some(generation) = state.status.as_ref().and_then(|status| status.status.active_generation) else {
            return false;
        };
        if state.lane_layout.is_some() && state.lane_layout_generation == Some(generation) {
            return false;
        }
        match state.lane_layout_asked {
            Some((asked, at_ms)) => asked != Some(generation) || now_ms.saturating_sub(at_ms) >= EXPORT_RETRY,
            None => true,
        }
    }

    /// Records the answer of the explain route, asked at `at_ms`, as the lane layout.
    pub fn record_lane_layout(&self, answer: Surface<Value>, at_ms: u64) {
        let mut state = self.lock();
        let generation = state.status.as_ref().and_then(|status| status.status.active_generation);
        state.lane_layout_asked = Some((generation, at_ms));
        match answer.map(|explain| LaneLayout::of_explain(&explain)) {
            Surface::Available(layout) => {
                state.lane_layout = layout.map(Arc::new);
                state.lane_layout_generation = generation;
                state.lane_layout_error = None;
            }
            // An ADP that serves no explain document, or no active generation (503), has no layout to show.
            Surface::NotExposed => {
                state.lane_layout = None;
                state.lane_layout_error = None;
            }
            Surface::Failed(error) => state.lane_layout_error = Some(error),
        }
    }

    /// Returns the active plan hash ADP's status reports, if it reports one that can be read.
    pub fn reported_active_plan_hash(&self) -> Option<PlanHash> {
        self.lock()
            .status
            .as_ref()?
            .status
            .active_plan_hash
            .as_deref()?
            .parse()
            .ok()
    }

    /// Polls the status route every [`STATUS_INTERVAL`], fetching the export when wanted, until the task is dropped.
    pub async fn run(self, api: AdpApi) {
        let mut ticks = tokio::time::interval(STATUS_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let status = api.control_json::<Value>(routes::STATUS).await;
            self.record_status(status, unix_millis());
            if self.export_wanted(unix_millis()) {
                self.refresh_export(&api).await;
            }
            if let Some(active) = self.reported_active_plan_hash() {
                self.refresh_active_plan(&api, &active).await;
            }
            if self.lane_layout_wanted(unix_millis()) {
                let explain = api.control_json::<Value>(routes::EXPLAIN).await;
                self.record_lane_layout(explain, unix_millis());
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ControlState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Checks that `plan` is the default program `export` was taken from: its base program hash is the export's.
fn matching_base(plan: &Plan, export: &ExportBundle) -> Result<(), String> {
    let scope = ScopeName::new(export.scope.as_str())
        .map_err(|error| format!("The export's scope `{}` is not a scope name: {}", export.scope, error))?;
    let base = base(plan, &scope)
        .map_err(|error| format!("ADP's default program cannot be checked against its export: {}", error))?;
    if &base.program_hash == export.program_hash() {
        Ok(())
    } else {
        Err(format!(
            "ADP's default program has base {}, but the export in hand was taken from {}: the configuration changed \
             between the two requests. Both are fetched again.",
            base.program_hash,
            export.program_hash()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::tests::DEMO_EXPORT;

    fn status(default_program_hash: &str) -> Value {
        serde_json::json!({ "default_program_hash": default_program_hash })
    }

    #[test]
    fn a_missing_control_surface_is_reported_plainly() {
        let control = ControlSurface::new();
        assert_eq!(control.view().status_state, SurfaceState::Unknown);
        assert_eq!(control.view().message, None);

        control.record_status(Surface::NotExposed, 1_000);
        control.record_export(Surface::NotExposed, 1_000);
        let view = control.view();
        assert_eq!(view.status_state, SurfaceState::NotExposed);
        assert_eq!(view.message.as_deref(), Some(NOT_EXPOSED_MESSAGE));
        assert_eq!(view.detail, Some(NOT_EXPOSED_DETAIL));
        assert!(control.export().is_none());

        control.record_status(Surface::Failed("connection refused".to_string()), 2_000);
        let view = control.view();
        assert_eq!(view.status_state, SurfaceState::Failed);
        assert!(view.message.unwrap().contains("connection refused"));
        assert_eq!(view.detail, None);
    }

    #[test]
    fn the_status_in_hand_is_kept_with_when_it_arrived_and_unreadable_fields_are_noted() {
        let control = ControlSurface::new();
        control.record_status(
            Surface::Available(serde_json::json!({"revision_high_water": 4, "active_generation": "3"})),
            1_000,
        );
        let (received_ms, status) = control.status().unwrap();
        assert_eq!(received_ms, 1_000);
        assert_eq!(status.status.revision_high_water, Some(4));
        assert_eq!(status.status.active_generation, None);
        assert_eq!(control.view().status_notes.len(), 1);

        // A failed request keeps the last status, and its age shows that it is only the last known one.
        control.record_status(Surface::Failed("connection refused".to_string()), 5_000);
        let view = control.view();
        assert_eq!(
            (view.status_checked_ms, view.status_received_ms),
            (Some(5_000), Some(1_000))
        );
        assert_eq!(control.status().unwrap().0, 1_000);
    }

    #[test]
    fn the_default_program_is_kept_with_the_export_it_belongs_to_and_the_active_plan_by_its_hash() {
        let (plan, export) = crate::preview::tests::fixture_documents();
        let control = ControlSurface::new();
        let program = || Surface::Available(serde_json::to_value(&plan).unwrap());

        // Without an export there is nothing to match it with.
        assert_eq!(control.record_default_program(program()), SurfaceState::Failed);
        assert!(control.export_with_default_program().is_none());

        control.record_export(Surface::Available(export), 1_000);
        assert_eq!(control.record_default_program(program()), SurfaceState::Available);
        let (_, default_program) = control.export_with_default_program().unwrap();
        assert_eq!(*default_program.unwrap(), plan);
        assert_eq!(control.view().default_program_state, SurfaceState::Available);

        // An ADP that does not serve the route: structural checks only, without an error.
        assert_eq!(
            control.record_default_program(Surface::NotExposed),
            SurfaceState::NotExposed
        );
        assert!(control.export_with_default_program().unwrap().1.is_none());
        assert_eq!(control.view().default_program_error, None);

        // The active plan is kept by the hash it has, which is what callers ask for.
        let hash = plan.hash(ResolvedValues::Include).unwrap();
        let (kept, _) = control.record_active_plan(program()).unwrap();
        assert_eq!(kept, hash);
        assert!(control.active_plan(&hash).is_some());
        assert!(control.active_plan(&PlanHash::of_bytes(b"another plan")).is_none());
        assert_eq!(control.view().active_plan_hash, Some(hash.to_string()));
        let unreadable = control.record_active_plan(Surface::Available(serde_json::json!({"nodes": 3})));
        assert!(unreadable.is_none());
        assert!(control.view().active_plan_error.unwrap().contains("cannot be read"));
        // A failed request keeps the plan in hand.
        assert!(control.active_plan(&hash).is_some());
    }

    #[test]
    fn the_export_is_fetched_again_when_the_reported_default_program_changes_or_it_ages() {
        let control = ControlSurface::new();
        assert!(control.export_wanted(0));

        // Not exposed: asked again after the retry delay only.
        control.record_export(Surface::NotExposed, 10_000);
        assert!(!control.export_wanted(10_000 + EXPORT_RETRY - 1));
        assert!(control.export_wanted(10_000 + EXPORT_RETRY));

        // ADP reports a default program hash computed otherwise than the export's base hash: that alone is no reason
        // to fetch again, only a change of the reported hash is.
        let reported = format!("sha256:{}", "b".repeat(64));
        control.record_status(Surface::Available(status(&reported)), 19_500);
        let exported = control.record_export(Surface::Available(serde_json::from_str(DEMO_EXPORT).unwrap()), 20_000);
        let Surface::Available(export) = exported else {
            panic!("the demo export reads: {:?}", exported.state());
        };
        assert_ne!(export.program_hash().to_string(), reported);
        control.record_status(Surface::Available(status(&reported)), 20_500);
        assert!(!control.export_wanted(21_000));
        assert!(control.export_wanted(20_000 + EXPORT_MAX_AGE));

        // The default program changed: fetch at once, and not again once fetched.
        let other = format!("sha256:{}", "a".repeat(64));
        control.record_status(Surface::Available(status(&other)), 21_000);
        assert!(control.export_wanted(21_000));
        control.record_export(Surface::Available(serde_json::from_str(DEMO_EXPORT).unwrap()), 21_100);
        assert!(!control.export_wanted(21_200));

        // An unreadable bundle is a failure with the reason, and keeps the export in hand.
        let failed = control.record_export(Surface::Available(serde_json::json!({"export_version": 9})), 22_000);
        assert_eq!(failed.state(), SurfaceState::Failed);
        assert!(control.view().export_error.unwrap().contains("export_version 9"));
        assert!(control.export().is_some());
    }

    #[test]
    fn the_export_is_fetched_again_when_the_active_resources_artifact_changes() {
        // Declaring an output changes the catalog, not the default program hash.
        let reported = format!("sha256:{}", "b".repeat(64));
        let with_resources = |active: Value| {
            let mut status = status(&reported);
            status["resources"] = serde_json::json!({ "revision_high_water": 2, "active_artifact": active });
            status
        };
        let export = || Surface::Available(serde_json::from_str(DEMO_EXPORT).unwrap());
        let control = ControlSurface::new();
        control.record_status(Surface::Available(status(&reported)), 1_000);
        control.record_export(export(), 1_000);
        assert!(!control.export_wanted(1_100));

        let team_d = serde_json::json!({ "id": "demo-resources", "revision": 2, "artifact_hash": "sha256:02" });
        control.record_status(Surface::Available(with_resources(team_d.clone())), 2_000);
        assert!(control.export_wanted(2_000));
        control.record_export(export(), 2_100);
        assert!(!control.export_wanted(2_200));
        control.record_status(Surface::Available(with_resources(team_d)), 3_000);
        assert!(!control.export_wanted(3_000));

        // A revocation leaves no resources artifact active: fetch again.
        control.record_status(Surface::Available(with_resources(Value::Null)), 4_000);
        assert!(control.export_wanted(4_000));
    }

    #[test]
    fn the_recipients_each_status_lists_go_into_the_panels_record() {
        let control = ControlSurface::new();
        control.record_status(Surface::Available(status("sha256:00")), 1_000);
        assert!(!control.recipients().reported);
        let listed = serde_json::json!({
            "default_program_hash": "sha256:00",
            "recipients": [{ "host": "dd_out", "id": "dd_out/primary/0", "slot": 0, "incarnation": 1, "state": "live",
                             "provenance": "native", "queue_bytes": 512 }]
        });
        control.record_status(Surface::Available(listed), 2_000);
        let view = control.recipients();
        assert!(view.reported);
        assert_eq!(
            (view.recipients[0].state.as_str(), view.recipients[0].queue_bytes),
            ("active", Some(512))
        );
    }

    #[test]
    fn an_artifact_is_checked_against_the_export_of_the_scope_it_replaces() {
        const PRE: &str = "dogstatsd_pre_aggregation";
        let (plan, export) = crate::preview::tests::fixture_documents();
        let control = ControlSurface::new();
        control.record_export(Surface::Available(export.clone()), 1_000);
        control.record_default_program(Surface::Available(serde_json::to_value(&plan).unwrap()));
        let export_of = |scope| {
            control
                .export_of_scope(scope)
                .map(|(export, default_program)| (export.scope.to_string(), default_program.is_some()))
        };
        assert_eq!(export_of(POLICY_SCOPE), Some((POLICY_SCOPE.to_string(), true)));
        // A scope the default program does not declare gets the policy scope's: composition names the declared ones.
        assert_eq!(export_of("no_such_scope"), Some((POLICY_SCOPE.to_string(), true)));

        // Another scope's export, once in hand, with the default program only if the export was taken from it.
        assert_eq!(export_of(PRE), None);
        let mut bundle = export;
        bundle["scope"] = PRE.into();
        bundle["artifact"]["scope"] = PRE.into();
        control.record_scope_export(PRE, Surface::Available(bundle.clone()), 2_000);
        assert_eq!(export_of(PRE), Some((PRE.to_string(), false)));
        let base = base(&plan, &PRE.parse().unwrap()).unwrap();
        bundle["artifact"]["base"]["program_hash"] = base.program_hash.to_string().into();
        control.record_scope_export(PRE, Surface::Available(bundle), 3_000);
        assert_eq!(export_of(PRE), Some((PRE.to_string(), true)));
    }

    #[test]
    fn every_other_scopes_export_is_kept_on_its_own_with_why_it_cannot_be_read() {
        let control = ControlSurface::new();
        let mut bundle = serde_json::from_str::<Value>(DEMO_EXPORT).unwrap();
        bundle["scope"] = "dogstatsd_pre_aggregation".into();
        bundle["artifact"]["scope"] = "dogstatsd_pre_aggregation".into();
        let kept = control.record_scope_export("dogstatsd_pre_aggregation", Surface::Available(bundle), 1_000);
        assert_eq!(kept.state(), SurfaceState::Available);
        // The policy scope's export is not this one.
        assert!(control.export().is_none());
        let failed = control.record_scope_export(
            "dogstatsd_pre_aggregation",
            Surface::Available(serde_json::json!({"export_version": 9})),
            2_000,
        );
        assert_eq!(failed.state(), SurfaceState::Failed);
        let views = control.view().scope_exports;
        assert_eq!(views.len(), 1);
        let view = &views[0];
        assert_eq!(
            (view.scope.as_str(), view.state),
            ("dogstatsd_pre_aggregation", SurfaceState::Failed)
        );
        // The export in hand stays, with when it was fetched, and the error says why the latest could not be read.
        assert_eq!(view.fetched_ms, Some(1_000));
        assert!(view.program_hash.is_some());
        assert!(view.error.as_deref().unwrap().contains("export_version 9"));
        assert_eq!(
            routes::export_of("dogstatsd_pre_aggregation"),
            "/programmable-metrics/export?scope=dogstatsd_pre_aggregation"
        );
        assert_eq!(routes::export_of(POLICY_SCOPE), routes::EXPORT);
    }

    #[test]
    fn the_lane_layout_is_read_once_per_active_generation_and_again_after_a_failure_only_after_a_while() {
        let control = ControlSurface::new();
        let status = |generation: u64| Surface::Available(serde_json::json!({ "active_generation": generation }));
        let explain = |state_lanes: u16| {
            serde_json::json!({"physical": {
                "layout": {"dogstatsd_lanes": 2, "state_lanes": state_lanes, "post_lanes": 3, "pipelining": false},
                "fixed": [], "slots": [], "edges": [], "fusion": [], "segments": [], "output_bindings": []
            }})
        };
        let state_lanes = |control: &ControlSurface| {
            control
                .lane_layout()
                .0
                .and_then(|layout| layout.policy.clone())
                .map(|policy| policy.state_lanes)
        };

        // No active generation: nothing to explain yet.
        assert!(!control.lane_layout_wanted(1_000));
        control.record_status(status(1), 1_000);
        assert!(control.lane_layout_wanted(1_000));
        control.record_lane_layout(Surface::Available(explain(2)), 1_000);
        assert_eq!(state_lanes(&control), Some(2));
        assert!(!control.lane_layout_wanted(100_000));

        // A new generation (or a restart) reads it again; a failure keeps the layout in hand, says why, and is retried
        // after a while, or at once for another generation.
        control.record_status(status(2), 2_000);
        assert!(control.lane_layout_wanted(2_000));
        control.record_lane_layout(Surface::Failed("ADP answered 503".to_string()), 2_000);
        assert_eq!(state_lanes(&control), Some(2));
        assert_eq!(control.lane_layout().1.as_deref(), Some("ADP answered 503"));
        assert!(!control.lane_layout_wanted(2_000 + EXPORT_RETRY - 1));
        assert!(control.lane_layout_wanted(2_000 + EXPORT_RETRY));
        control.record_status(status(3), 3_000);
        assert!(control.lane_layout_wanted(3_000));
        control.record_lane_layout(Surface::Available(explain(4)), 3_000);
        assert_eq!((state_lanes(&control), control.lane_layout().1), (Some(4), None));

        // An explain document without physical sections has no layout.
        control.record_status(status(4), 4_000);
        control.record_lane_layout(Surface::Available(serde_json::json!({"explain_version": 1})), 4_000);
        assert!(control.lane_layout().0.is_none());
    }
}
