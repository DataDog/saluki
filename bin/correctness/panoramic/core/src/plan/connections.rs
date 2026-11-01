//! Validates connections against each participant's configured endpoints.

use std::collections::BTreeMap;

use crate::{
    case::{declared_names, CaseFile},
    endpoint::{ConsumedEndpoint, Endpoint},
    layout::{describe_participant, Layout},
};

/// Checks each consumer's connections, then its required endpoints, all in name order.
pub(super) fn check_connections(
    file: &CaseFile, layout: &Layout, served: &BTreeMap<String, BTreeMap<String, Endpoint>>,
    consumed: &BTreeMap<String, BTreeMap<String, ConsumedEndpoint>>,
) -> Result<(), String> {
    for (name, entry) in &file.participants {
        let realization = layout.get(name).expect("loaded participants have a layout");
        let type_name = &entry.participant_type;
        let holder = describe_participant(name, type_name, realization);
        let consumes = &consumed[name];
        for (connection, target) in &entry.connect {
            let Some(endpoint) = consumes.get(connection) else {
                return Err(format!(
                    "{holder} connects '{connection}', which it does not consume; it consumes {}",
                    declared_names(&consumes.keys().map(String::as_str).collect::<Vec<_>>()),
                ));
            };
            let target_name = &target.participant;
            let target_endpoint = &target.endpoint;
            let target_serves = &served[target_name];
            let Some(served_endpoint) = target_serves.get(target_endpoint) else {
                let target_realization = layout.get(target_name).expect("connection targets have a layout");
                let target_type = &file.participants[target_name].participant_type;
                let target_holder = describe_participant(target_name, target_type, target_realization);
                return Err(format!(
                    "participant '{name}' connects '{connection}' to '{target_name}.{target_endpoint}', which {target_holder} does not serve; it serves {}",
                    declared_names(&target_serves.keys().map(String::as_str).collect::<Vec<_>>()),
                ));
            };
            if !endpoint.accepts.contains(&served_endpoint.kind) {
                return Err(format!(
                    "participant '{name}' connects '{connection}' to '{target_name}.{target_endpoint}', an endpoint of kind {}; '{connection}' of type '{type_name}' accepts {}",
                    served_endpoint.kind.name(),
                    declared_names(&endpoint.accepts.iter().map(|kind| kind.name()).collect::<Vec<_>>()),
                ));
            }
        }
        for (connection, endpoint) in consumes {
            if endpoint.required && !entry.connect.contains_key(connection) {
                return Err(format!(
                    "{holder} does not connect required endpoint '{connection}'; add '{connection}: <participant>.<endpoint>' to its 'connect'"
                ));
            }
        }
    }
    Ok(())
}
