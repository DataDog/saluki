//! Resources and output bindings: the capability-binding rule.
//!
//! A plan carries the resources its outputs deliver through as parameters, and the plan crate does not read what they
//! mean, so these rules check what it can: that each resource is declared once, on a delivery host the catalog
//! declares, with parameters the host's schema accepts; that each output binding's fingerprint is the one its selector
//! gives, and its recipients are declared on its host and accept what it produces, as the host decides; and that
//! outputs and recipients a resources artifact declares stay apart from native ones.

use std::collections::BTreeSet;

use super::{catalog::Catalog, Finding, Location, Rule, ValidationReport};
use crate::{
    extension::DeliveryPolicy,
    ir::{
        decode_params, HostId, OutputBinding, OutputDecl, ParamError, ParamSpec, Params, PayloadClasses, Plan,
        RecipientDecl, ResourceProvenance, Selector,
    },
    resources::{credential_name, fingerprint, CREDENTIALS_NAMESPACE},
};

pub(super) fn check(plan: &Plan, catalog: &dyn Catalog, report: &mut ValidationReport) {
    check_declarations(plan, catalog, report);
    for output in &plan.outputs {
        match &output.binding {
            Some(binding) => check_binding(plan, output, binding, catalog, report),
            // The prefix is reserved for outputs a resources artifact declares, which always deliver through a host.
            None if output.id.is_declared() => report.push(Finding::new(
                Rule::OutputBinding,
                Location::Output(output.id.clone()),
                "an output named `resources_<name>` is declared by a resources artifact and delivers through a \
                 delivery host, but this one has no binding; native outputs never use the prefix",
            )),
            None => {}
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
        match &recipient.provenance {
            ResourceProvenance::Native => {
                if !recipient.delivery.is_native() {
                    push(at(
                        "it has a bounded delivery policy, which only a recipient a resources artifact declares may \
                         have; native recipients keep their host's native policy"
                            .to_string(),
                    ));
                }
            }
            ResourceProvenance::Declared(_) => check_declared_recipient(recipient, catalog, &mut push),
        }
    }
}

/// Checks what only a recipient a resources artifact declares must satisfy.
fn check_declared_recipient(recipient: &RecipientDecl, catalog: &dyn Catalog, push: &mut impl FnMut(String)) {
    let at = |message: String| format!("recipient `{}`: {}", recipient.id, message);
    if catalog
        .host(&recipient.host)
        .is_some_and(|schema| !schema.declared_recipients)
    {
        push(at(format!(
            "it is declared by a resources artifact, but host `{}` does not accept declared recipients",
            recipient.host
        )));
    }
    if credential_name(&recipient.credential).is_none() {
        push(at(format!(
            "it is declared by a resources artifact, so its credential must be a `{}<name>` reference, not `{}`",
            CREDENTIALS_NAMESPACE, recipient.credential
        )));
    }
    if recipient.classes != PayloadClasses::MetricsOnly {
        push(at(
            "it is declared by a resources artifact, so it receives metrics only: its payload classes must be \
             `metrics_only`"
                .to_string(),
        ));
    }
    if let DeliveryPolicy::Bounded(bounded) = recipient.delivery {
        if bounded.queue_bytes == Some(0) {
            push(at("its queue is bounded to 0 bytes, which holds nothing".to_string()));
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

    let expected = fingerprint(&binding.host, output.accepts, &binding.selector);
    if output.fingerprint != expected {
        push(format!(
            "its fingerprint is {}, but its binding gives {}: a bound output's fingerprint is version 2, the digest of \
             its delivery host, what it accepts, and its selector",
            output.fingerprint, expected
        ));
    }

    let declared_selector = matches!(binding.selector, Selector::Declared(_));
    let declared_provenance = matches!(binding.provenance, ResourceProvenance::Declared(_));
    if output.id.is_declared() != declared_selector || output.id.is_declared() != declared_provenance {
        push(
            "an output a resources artifact declares has an identifier `resources_<name>`, a declared selector, and \
             the artifact's provenance, and a native output has none of them; this output mixes them"
                .to_string(),
        );
    }
    if let Selector::Declared(selected) = &binding.selector {
        let mut sorted = selected.clone();
        sorted.sort();
        sorted.dedup();
        if &sorted != selected || *selected != output.recipients {
            push(
                "its declared selector must list its recipients, sorted and without repeats, as `recipients` does"
                    .to_string(),
            );
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
        if declared_selector && !matches!(recipient.provenance, ResourceProvenance::Declared(_)) {
            push(format!(
                "it is declared by a resources artifact, so it delivers to declared recipients only, but `{}` is a \
                 native recipient",
                id
            ));
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
        canonical::PlanHash,
        ir::{
            ArtifactRevision, BindingName, CredentialRef, HostBinding, InputName, NativeComponentId, OutputId,
            ParamType, RecipientId, Resources, RoleName, ScopeName,
        },
        model::PortType,
        operators::OperatorRegistry,
        validate::{CatalogBinding, CatalogOutput, HostSchema, Severity, Validator},
    };

    const ENCODER: &[ParamSpec] = &[ParamSpec::optional("series", ParamType::Enum(&["v2", "v3"]))];
    const SETTINGS: &[ParamSpec] = &[ParamSpec::optional("timeout", ParamType::Duration)];
    const PROTOCOL: &[ParamSpec] = &[ParamSpec::optional("v3_series", ParamType::Bool)];

    /// A catalog that declares `dd_out` (which accepts declared recipients) and `mrf_dd_out`, the outputs of the plan
    /// it validates, and every host when asked to, refusing V3 series for recipients that do not take them.
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

        fn scope_input(&self, _: &ScopeName, _: &InputName) -> Option<PortType> {
            None
        }

        fn host(&self, host: &HostId) -> Option<HostSchema> {
            let schema = |declared_recipients| HostSchema {
                encoder: ENCODER,
                settings: SETTINGS,
                protocol: PROTOCOL,
                declared_recipients,
            };
            match host.as_str() {
                "dd_out" => Some(schema(true)),
                "mrf_dd_out" => Some(schema(false)),
                _ => None,
            }
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

    fn revision() -> ArtifactRevision {
        ArtifactRevision {
            revision: 1,
            hash: PlanHash::of_bytes(b"resources"),
        }
    }

    fn recipient(id: &str, provenance: ResourceProvenance) -> RecipientDecl {
        let declared = matches!(provenance, ResourceProvenance::Declared(_));
        RecipientDecl {
            id: RecipientId::new(id).unwrap(),
            host: HostId::new("dd_out").unwrap(),
            endpoint: "https://intake.example.com".to_string(),
            credential: CredentialRef::new(if declared {
                "credentials/team_d"
            } else {
                "native:api_key"
            })
            .unwrap(),
            classes: if declared {
                PayloadClasses::MetricsOnly
            } else {
                PayloadClasses::All
            },
            protocol: Params::new(),
            delivery: DeliveryPolicy::Native,
            provenance,
        }
    }

    fn bound(id: &str, selector: Selector, recipients: &[&str], provenance: ResourceProvenance) -> OutputDecl {
        let host = HostId::new("dd_out").unwrap();
        OutputDecl {
            id: OutputId::new(id).unwrap(),
            accepts: PortType::ANY,
            fingerprint: fingerprint(&host, PortType::ANY, &selector),
            recipients: recipients.iter().map(|id| RecipientId::new(*id).unwrap()).collect(),
            delivery: DeliveryPolicy::Native,
            binding: Some(OutputBinding {
                host,
                selector,
                encoder: Params::new(),
                telemetry: NativeComponentId::new(id).unwrap(),
                provenance,
            }),
        }
    }

    const PRIMARY: &str = "dd_out/primary/0/https://app.example.com";
    const TEAM_D: &str = "dd_out/declared/team_d/https://intake.example.com";

    /// A plan whose main output and declared output `resources_team_d` deliver through `dd_out`.
    fn plan() -> Plan {
        let main = bound(
            "dd_metrics_encode",
            Selector::Role(RoleName::new("main").unwrap()),
            &[PRIMARY],
            ResourceProvenance::Native,
        );
        let team_d = bound(
            "resources_team_d",
            Selector::Declared(vec![RecipientId::new(TEAM_D).unwrap()]),
            &[TEAM_D],
            ResourceProvenance::Declared(revision()),
        );
        let value = json!({
            "ir_version": 1,
            "nodes": [
                {"id": "in", "op": "source", "params": {"binding": "otlp"}, "provenance": {"native": "in"}},
                {"id": "copy", "op": "tee", "params": {"ports": 2}},
                {"id": "main", "op": "emit", "params": {"output": "dd_metrics_encode"}},
                {"id": "team", "op": "emit", "params": {"output": "resources_team_d"}}
            ],
            "edges": [
                {"from": "in.metrics", "to": "copy.in"},
                {"from": "copy.0", "to": "main.in"},
                {"from": "copy.1", "to": "team.in"}
            ],
            "outputs": [main, team_d],
            "resources": Resources {
                hosts: vec![HostBinding { host: HostId::new("dd_out").unwrap(), settings: Params::new() }],
                recipients: vec![
                    recipient(PRIMARY, ResourceProvenance::Native),
                    recipient(TEAM_D, ResourceProvenance::Declared(revision())),
                ],
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
                        fingerprint: output.fingerprint,
                        recipients: output.recipients.clone(),
                        legacy_fingerprint: None,
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
    fn a_plan_whose_outputs_bind_to_declared_resources_validates() {
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
    fn declared_recipients_receive_metrics_only_with_operator_credentials_and_native_ones_keep_native_delivery() {
        let mut bounded = plan();
        bounded.resources.recipients[0].delivery = DeliveryPolicy::Bounded(crate::extension::BoundedDelivery {
            queue_bytes: Some(1024),
            spill: crate::extension::Spill::Never,
            on_retire: crate::extension::Closure::Discard,
        });
        rejected(
            &bounded,
            Rule::ResourceDeclarations,
            "only a recipient a resources artifact declares may have",
        );

        let mut classes = plan();
        classes.resources.recipients[1].classes = PayloadClasses::All;
        rejected(&classes, Rule::ResourceDeclarations, "it receives metrics only");

        let mut credential = plan();
        credential.resources.recipients[1].credential = CredentialRef::new("native:api_key").unwrap();
        rejected(
            &credential,
            Rule::ResourceDeclarations,
            "its credential must be a `credentials/<name>` reference",
        );

        let mut failover = plan();
        for recipient in &mut failover.resources.recipients {
            recipient.host = HostId::new("mrf_dd_out").unwrap();
        }
        rejected(
            &failover,
            Rule::ResourceDeclarations,
            "host `mrf_dd_out` does not accept declared recipients",
        );
    }

    #[test]
    fn an_output_binding_has_its_selectors_fingerprint_and_recipients_declared_on_its_host() {
        let mut fingerprinted = plan();
        fingerprinted.outputs[0].fingerprint = PlanHash::of_bytes(b"version 1");
        // The catalog agrees, so only the binding rule reports it.
        rejected(&fingerprinted, Rule::OutputBinding, "but its binding gives sha256:");

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
        binding.host = host.clone();
        output.fingerprint = fingerprint(&host, output.accepts, &binding.selector);
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

    #[test]
    fn declared_outputs_deliver_to_declared_recipients_only_and_native_ones_are_not_declared() {
        // A declared output naming a native recipient.
        let mut native = plan();
        let selector = Selector::Declared(vec![RecipientId::new(PRIMARY).unwrap()]);
        let output = &mut native.outputs[1];
        output.recipients = vec![RecipientId::new(PRIMARY).unwrap()];
        output.fingerprint = fingerprint(&HostId::new("dd_out").unwrap(), output.accepts, &selector);
        output.binding.as_mut().unwrap().selector = selector;
        rejected(
            &native,
            Rule::OutputBinding,
            "so it delivers to declared recipients only",
        );

        // A declared selector that does not list the output's recipients.
        let mut listed = plan();
        listed.outputs[1].recipients.push(RecipientId::new(PRIMARY).unwrap());
        rejected(
            &listed,
            Rule::OutputBinding,
            "its declared selector must list its recipients",
        );

        // A native output with a declared provenance, and a declared output with a role.
        let mut mixed = plan();
        mixed.outputs[0].binding.as_mut().unwrap().provenance = ResourceProvenance::Declared(revision());
        rejected(&mixed, Rule::OutputBinding, "this output mixes them");
        let mut role = plan();
        let selector = Selector::Role(RoleName::new("main").unwrap());
        let output = &mut role.outputs[1];
        output.fingerprint = fingerprint(&HostId::new("dd_out").unwrap(), output.accepts, &selector);
        output.binding.as_mut().unwrap().selector = selector;
        rejected(&role, Rule::OutputBinding, "this output mixes them");

        // The prefix is reserved: a fixed destination cannot have it.
        let mut unbound = plan();
        unbound.outputs[1].binding = None;
        rejected(&unbound, Rule::OutputBinding, "but this one has no binding");
    }
}
