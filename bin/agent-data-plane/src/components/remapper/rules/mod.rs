use super::RemapperRule;

mod aggregation;
mod dogstatsd;
mod transaction;

pub fn get_datadog_agent_remappings() -> Vec<RemapperRule> {
    let mut rules = Vec::new();
    rules.extend(self::dogstatsd::get_dogstatsd_remappings());
    rules.extend(self::aggregation::get_aggregation_remappings());
    rules.extend(self::transaction::get_transaction_remappings());
    rules
}
