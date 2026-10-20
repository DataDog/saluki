//! Resources and output bindings: the capability-binding rule.
//!
//! A plan carries the resources its outputs deliver through as parameters, and the plan crate does not read what they
//! mean, so these rules check what it can: that each resource is declared once, on a delivery host the catalog
//! declares, with parameters the host's schema accepts; and that each output binding's recipients are declared on its
//! host and accept what it produces, as the host decides.

use std::collections::BTreeSet;

use super::{catalog::Catalog, Finding, Location, Rule, ValidationReport};
use crate::ir::{decode_params, HostId, OutputBinding, OutputDecl, ParamError, ParamSpec, Params, Plan, Selector};

pub(super) fn check(plan: &Plan, catalog: &dyn Catalog, report: &mut ValidationReport) {
    check_declarations(plan, catalog, report);
    for output in &plan.outputs {
        if let Some(binding) = &output.binding {
            check_binding(plan, output, binding, catalog, report);
        }
    }
}

/// Says why parameters do not match a schema.
fn describe_errors(errors: &[ParamError]) -> String {
    errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")
}

/// Checks parameters against the schema the catalog gives for `host`, if it gives one, returning what is wrong; and,
/// for a catalog that declares every host, that it declares this one.
fn check_host(
    host: &HostId, params: &Params, schema: impl Fn(&super::HostSchema) -> &'static [ParamSpec], catalog: &dyn Catalog,
) -> Result<(), String> {
    match catalog.host(host) {
        Some(declared) => decode_params(schema(&declared), params).map(|_| ()).map_err(|errors| {
            format!(
                "its parameters do not match the schema of host `{}`: {}",
                host,
                describe_errors(&errors)
            )
        }),
        None if catalog.declares_hosts() => Err(format!("the catalog declares no delivery host `{}`", host)),
        None => Ok(()),
    }
}

fn check_declarations(plan: &Plan, catalog: &dyn Catalog, report: &mut ValidationReport) {
    let resources = &plan.resources;
    let mut push = |message: String| report.push(Finding::new(Rule::ResourceDeclarations, Location::Plan, message));

    let mut hosts = BTreeSet::new();
    for binding in &resources.hosts {
        if !hosts.insert(&binding.host) {
            push(format!(
                "the settings of host `{}` are declared more than once; declare each host's settings once",
                binding.host
            ));
            continue;
        }
        if let Err(problem) = check_host(&binding.host, &binding.settings, |schema| schema.settings, catalog) {
            push(format!("the settings of host `{}`: {}", binding.host, problem));
        }
    }

    let mut ids = BTreeSet::new();
    for recipient in &resources.recipients {
        let at = |message: String| format!("recipient `{}`: {}", recipient.id, message);
        if !ids.insert(&recipient.id) {
            push(at(
                "it is declared more than once; declare each recipient once".to_string()
            ));
            continue;
        }
        if let Err(problem) = check_host(&recipient.host, &recipient.protocol, |schema| schema.protocol, catalog) {
            push(at(problem));
        }
        if !recipient.delivery.is_native() {
            push(at(
                "it has a bounded delivery policy, which nothing declares yet; recipients keep their host's native \
                 policy"
                    .to_string(),
            ));
        }
    }
}

fn check_binding(
    plan: &Plan, output: &OutputDecl, binding: &OutputBinding, catalog: &dyn Catalog, report: &mut ValidationReport,
) {
    let mut push = |message: String| {
        report.push(Finding::new(
            Rule::OutputBinding,
            Location::Output(output.id.clone()),
            message,
        ))
    };

    if let Err(problem) = check_host(&binding.host, &binding.encoder, |schema| schema.encoder, catalog) {
        push(format!("its encoder: {}", problem));
    }

    if let Selector::Recipients(selected) = &binding.selector {
        let mut sorted = selected.clone();
        sorted.sort();
        sorted.dedup();
        if &sorted != selected || *selected != output.recipients {
            push("its selector must list its recipients, sorted and without repeats, as `recipients` does".to_string());
        }
    }

    for id in &output.recipients {
        let Some(recipient) = plan.resources.recipient(id) else {
            push(format!(
                "it delivers to recipient `{}`, which the plan's resources do not declare",
                id
            ));
            continue;
        };
        if recipient.host != binding.host {
            push(format!(
                "it delivers through host `{}`, but its recipient `{}` is on host `{}`",
                binding.host, id, recipient.host
            ));
            continue;
        }
        if let Some(reason) = catalog.refuses(output, recipient) {
            push(format!(
                "its recipient `{}` cannot receive what it produces: {}",
                id, reason
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        extension::DeliveryPolicy,
        ir::{
            BindingName, CredentialRef, HostBinding, NativeComponentId, OutputId, ParamType, PayloadClasses,
            RecipientDecl, RecipientId, Resources, RoleName,
        },
        model::PortType,
        operators::OperatorRegistry,
        validate::{CatalogBinding, CatalogOutput, HostSchema, Severity, Validator},
    };

    const ENCODER: &[ParamSpec] = &[ParamSpec::optional("series", ParamType::Enum(&["v2", "v3"]))];
    const SETTINGS: &[ParamSpec] = &[ParamSpec::optional("timeout", ParamType::Duration)];
    const PROTOCOL: &[ParamSpec] = &[ParamSpec::optional("v3_series", ParamType::Bool)];

    /// A catalog that declares `dd_out` and `mrf_dd_out`, the outputs of the plan it validates, and every host when
    /// asked to, refusing V3 series for recipients that do not take them.
    struct Hosts {
        outputs: Vec<(OutputId, CatalogOutput)>,
        every_host: bool,
    }

    impl Catalog for Hosts {
        fn output(&self, id: &OutputId) -> Option<&CatalogOutput> {
            self.outputs
                .iter()
                .find(|(output, _)| output == id)
                .map(|(_, output)| output)
        }

        fn binding(&self, _: &BindingName) -> Option<&CatalogBinding> {
            None
        }

        fn host(&self, host: &HostId) -> Option<HostSchema> {
            let schema = HostSchema {
                encoder: ENCODER,
                settings: SETTINGS,
                protocol: PROTOCOL,
            };
            matches!(host.as_str(), "dd_out" | "mrf_dd_out").then_some(schema)
        }

        fn declares_hosts(&self) -> bool {
            self.every_host
        }

        fn refuses(&self, output: &OutputDecl, recipient: &RecipientDecl) -> Option<String> {
            let v3 = |params: &Params, name: &str| params.get(name).is_some_and(|value| value == "v3" || value == true);
            let binding = output.binding.as_ref()?;
            (v3(&binding.encoder, "series") && !v3(&recipient.protocol, "v3_series"))
                .then(|| "it takes no V3 series".to_string())
        }
    }

    fn recipient(id: &str) -> RecipientDecl {
        RecipientDecl {
            id: RecipientId::new(id).unwrap(),
            host: HostId::new("dd_out").unwrap(),
            endpoint: "https://intake.example.com".to_string(),
            credential: CredentialRef::new("native:api_key").unwrap(),
            classes: PayloadClasses::All,
            protocol: Params::new(),
            delivery: DeliveryPolicy::Native,
        }
    }

    fn bound(id: &str, selector: Selector, recipients: &[&str]) -> OutputDecl {
        let host = HostId::new("dd_out").unwrap();
        OutputDecl {
            id: OutputId::new(id).unwrap(),
            accepts: PortType::ANY,
            recipients: recipients.iter().map(|id| RecipientId::new(*id).unwrap()).collect(),
            delivery: DeliveryPolicy::Native,
            binding: Some(OutputBinding {
                host,
                selector,
                encoder: Params::new(),
                telemetry: NativeComponentId::new(id).unwrap(),
            }),
        }
    }

    const PRIMARY: &str = "dd_out/primary/0/https://app.example.com";
    const TEAM_B: &str = "dd_out/additional/0/https://intake.example.com";

    /// A plan whose main output, and an implied output for team B's endpoint, deliver through `dd_out`.
    fn plan() -> Plan {
        let main = bound(
            "dd_metrics_encode",
            Selector::Role(RoleName::new("main").unwrap()),
            &[PRIMARY],
        );
        let team_b = bound(
            "implied_team_b_e7b845f5",
            Selector::Recipients(vec![RecipientId::new(TEAM_B).unwrap()]),
            &[TEAM_B],
        );
        let value = json!({
            "ir_version": 1,
            "nodes": [
                {"id": "in", "op": "source", "params": {"binding": "otlp"}},
                {"id": "copy", "op": "tee", "params": {"ports": 2}},
                {"id": "main", "op": "emit", "params": {"output": "dd_metrics_encode"}},
                {"id": "team", "op": "emit", "params": {"output": "implied_team_b_e7b845f5"}}
            ],
            "edges": [
                {"from": "in.metrics", "to": "copy.in"},
                {"from": "copy.0", "to": "main.in"},
                {"from": "copy.1", "to": "team.in"}
            ],
            "outputs": [main, team_b],
            "resources": Resources {
                hosts: vec![HostBinding { host: HostId::new("dd_out").unwrap(), settings: Params::new() }],
                recipients: vec![recipient(PRIMARY), recipient(TEAM_B)],
            },
        });
        Plan::from_value(value).unwrap()
    }

    fn catalog(plan: &Plan) -> Hosts {
        Hosts {
            outputs: plan
                .outputs
                .iter()
                .map(|output| {
                    let declared = CatalogOutput {
                        accepts: output.accepts,
                        recipients: output.recipients.clone(),
                    };
                    (output.id.clone(), declared)
                })
                .collect(),
            every_host: true,
        }
    }

    /// Returns the messages of the plan's resource findings, by rule.
    fn findings(plan: &Plan, catalog: &Hosts) -> Vec<(Rule, String)> {
        let registry = OperatorRegistry::native();
        Validator::new(&registry, catalog)
            .validate(plan)
            .findings()
            .iter()
            .filter(|finding| finding.severity == Severity::Error)
            .map(|finding| (finding.rule, finding.message.clone()))
            .collect()
    }

    #[track_caller]
    fn rejected(plan: &Plan, rule: Rule, expected: &str) {
        let found = findings(plan, &catalog(plan));
        assert!(
            found
                .iter()
                .any(|(found, message)| *found == rule && message.contains(expected)),
            "expected {} with `{}`, found {:?}",
            rule,
            expected,
            found
        );
    }

    #[test]
    fn a_plan_whose_outputs_bind_to_its_resources_validates() {
        let plan = plan();
        assert_eq!(findings(&plan, &catalog(&plan)), Vec::new());
    }

    #[test]
    fn resources_are_declared_once_on_a_host_whose_schema_accepts_their_parameters() {
        let mut twice = plan();
        twice.resources.hosts.push(twice.resources.hosts[0].clone());
        rejected(
            &twice,
            Rule::ResourceDeclarations,
            "the settings of host `dd_out` are declared more than once",
        );

        let mut settings = plan();
        settings.resources.hosts[0].settings = Params::from_iter([("timeout".parse().unwrap(), json!("ten"))]);
        rejected(
            &settings,
            Rule::ResourceDeclarations,
            "do not match the schema of host `dd_out`",
        );

        let mut unknown = plan();
        unknown.resources.hosts[0].host = HostId::new("elsewhere_out").unwrap();
        rejected(
            &unknown,
            Rule::ResourceDeclarations,
            "the catalog declares no delivery host `elsewhere_out`",
        );
        // A catalog that does not declare every host, such as an exported one, leaves the host to placement.
        let mut partial = catalog(&unknown);
        partial.every_host = false;
        assert!(findings(&unknown, &partial)
            .iter()
            .all(|(rule, _)| *rule != Rule::ResourceDeclarations));

        let mut repeated = plan();
        repeated
            .resources
            .recipients
            .push(repeated.resources.recipients[0].clone());
        rejected(&repeated, Rule::ResourceDeclarations, "it is declared more than once");

        let mut protocol = plan();
        protocol.resources.recipients[0].protocol = Params::from_iter([("v3_series".parse().unwrap(), json!(1))]);
        rejected(
            &protocol,
            Rule::ResourceDeclarations,
            "do not match the schema of host `dd_out`",
        );
    }

    #[test]
    fn recipients_keep_their_hosts_native_delivery_policy() {
        let mut bounded = plan();
        bounded.resources.recipients[0].delivery = DeliveryPolicy::Bounded(crate::extension::BoundedDelivery {
            queue_bytes: Some(1024),
            spill: crate::extension::Spill::Never,
            on_retire: crate::extension::Closure::Discard,
        });
        rejected(
            &bounded,
            Rule::ResourceDeclarations,
            "it has a bounded delivery policy, which nothing declares yet",
        );
    }

    #[test]
    fn an_output_binding_has_its_encoder_and_recipients_declared_on_its_host() {
        let mut encoder = plan();
        encoder.outputs[0].binding.as_mut().unwrap().encoder =
            Params::from_iter([("series".parse().unwrap(), json!("v4"))]);
        rejected(
            &encoder,
            Rule::OutputBinding,
            "its encoder: its parameters do not match the schema",
        );

        let mut undeclared = plan();
        undeclared.resources.recipients.remove(0);
        rejected(
            &undeclared,
            Rule::OutputBinding,
            &format!(
                "it delivers to recipient `{}`, which the plan's resources do not declare",
                PRIMARY
            ),
        );

        let mut elsewhere = plan();
        let host = HostId::new("mrf_dd_out").unwrap();
        let output = &mut elsewhere.outputs[0];
        let binding = output.binding.as_mut().unwrap();
        binding.host = host;
        rejected(
            &elsewhere,
            Rule::OutputBinding,
            "it delivers through host `mrf_dd_out`, but its recipient",
        );

        let mut refused = plan();
        refused.outputs[0].binding.as_mut().unwrap().encoder =
            Params::from_iter([("series".parse().unwrap(), json!("v3"))]);
        rejected(
            &refused,
            Rule::OutputBinding,
            "cannot receive what it produces: it takes no V3 series",
        );
    }
}
