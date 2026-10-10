//! The control surface watcher: polls ADP's programmable-metrics status every second, and keeps the export bundle,
//! the default program, and the active plan current.
//!
//! The export is fetched again when the default program hash ADP's status reports changes from what it reported when
//! the export in hand was fetched, and at least every [`EXPORT_MAX_AGE`] otherwise, so the editor's checks follow
//! configuration changes. The watcher compares the reported hash with its own earlier value, never with the export's
//! `base.program_hash`, since the protocol does not say that the two are computed alike.
//!
//! The default program (`GET /programmable-metrics/default-program`), which the editor composes artifacts into, is
//! fetched with every export, and kept only if it is the program the export was taken from: its base program hash is
//! the export's. One that is not (the configuration changed between the two requests) makes the watcher fetch both
//! again. The active plan (`GET /programmable-metrics/plan`), which the editor diffs against, is fetched when the
//! active plan hash the status reports changes.
//!
//! The status is read field by field (see [`AdpStatus::read`]), and the status in hand is kept, with when it was
//! received, when a later request fails, so that the UI can show it as last known.

use std::{
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
    stub::protocol::AdpStatus,
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
    /// The default program the export in hand was taken from.
    default_program: Option<Arc<Plan>>,
    default_program_state: Option<SurfaceState>,
    default_program_error: Option<String>,
    /// The active plan, with its hash.
    active_plan: Option<(PlanHash, Arc<Plan>)>,
    active_plan_error: Option<String>,
}

impl ControlState {
    fn reported_hash(&self) -> Option<String> {
        self.status
            .as_ref()
            .and_then(|status| status.status.default_program_hash.clone())
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

    /// Returns the export bundle in hand with the default program it was taken from, when ADP served that program.
    pub fn export_with_default_program(&self) -> Option<(Arc<ExportBundle>, Option<Arc<Plan>>)> {
        let state = self.lock();
        let export = state.export.clone()?;
        Some((export, state.default_program.clone()))
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
        }
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
                    || now_ms.saturating_sub(fetched) >= EXPORT_MAX_AGE
            }
            (_, _, Some(checked)) => now_ms.saturating_sub(checked) >= EXPORT_RETRY,
            _ => true,
        }
    }

    /// Fetches the export now, and the default program it was taken from, and records them.
    pub async fn refresh_export(&self, api: &AdpApi) -> Surface<Arc<ExportBundle>> {
        let answer = api.control_json::<Value>(routes::EXPORT).await;
        let export = self.record_export(answer, unix_millis());
        if let Surface::Available(_) = export {
            let answer = api.control_json::<Value>(routes::DEFAULT_PROGRAM).await;
            self.record_default_program(answer);
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
}
