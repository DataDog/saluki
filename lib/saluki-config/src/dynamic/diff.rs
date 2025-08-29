//! Functions for diffing configuration values.

use super::event::ConfigChangeEvent;

/// Diffs two configuration values and returns a list of changes.
pub fn diff_config(old_config: &figment::value::Value, new_config: &figment::value::Value) -> Vec<ConfigChangeEvent> {
    let mut changes = Vec::new();
    diff_recursive(old_config, new_config, "", &mut changes);
    changes
}

fn diff_recursive(
    old_config: &figment::value::Value, new_config: &figment::value::Value, path: &str,
    changes: &mut Vec<ConfigChangeEvent>,
) {
    if let (Some(old_dict), Some(new_dict)) = (old_config.as_dict(), new_config.as_dict()) {
        for (key, new_value) in new_dict {
            let current_path = if path.is_empty() {
                key.clone()
            } else {
                format!("{}.{}", path, key)
            };

            match old_dict.get(key) {
                Some(old_value) => {
                    if old_value != new_value {
                        if new_value.as_dict().is_some() && old_value.as_dict().is_some() {
                            diff_recursive(old_value, new_value, &current_path, changes);
                        } else {
                            changes.push(ConfigChangeEvent::Modified {
                                key: current_path,
                                old_value: serde_json::to_value(old_value).unwrap(),
                                new_value: serde_json::to_value(new_value).unwrap(),
                            });
                        }
                    }
                }
                None => {
                    changes.push(ConfigChangeEvent::Added {
                        key: current_path,
                        value: serde_json::to_value(new_value).unwrap(),
                    });
                }
            }
        }
    }
}
