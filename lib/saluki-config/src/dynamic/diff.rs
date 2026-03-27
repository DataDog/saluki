//! Functions for diffing configuration values.

use facet_value::Value;

use super::event::ConfigChangeEvent;

/// Diffs two configuration values and returns a list of changes.
pub fn diff_config(old_config: &Value, new_config: &Value) -> Vec<ConfigChangeEvent> {
    let mut changes = Vec::new();
    diff_recursive(old_config, new_config, "", &mut changes);
    changes
}

fn diff_recursive(old_config: &Value, new_config: &Value, path: &str, changes: &mut Vec<ConfigChangeEvent>) {
    if let (Some(old_obj), Some(new_obj)) = (old_config.as_object(), new_config.as_object()) {
        for (key, new_value) in new_obj.iter() {
            let key_str = key.as_str();
            let current_path = if path.is_empty() {
                key_str.to_string()
            } else {
                format!("{}.{}", path, key_str)
            };

            match old_obj.get(key_str) {
                Some(old_value) => {
                    if old_value != new_value {
                        if new_value.as_object().is_some() && old_value.as_object().is_some() {
                            diff_recursive(old_value, new_value, &current_path, changes);
                        } else {
                            changes.push(ConfigChangeEvent {
                                key: current_path,
                                old_value: Some(old_value.clone()),
                                new_value: Some(new_value.clone()),
                            });
                        }
                    }
                }
                None => {
                    changes.push(ConfigChangeEvent {
                        key: current_path,
                        old_value: None,
                        new_value: Some(new_value.clone()),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use facet_value::value;

    use super::*;

    #[test]
    fn test_diff_config_basic() {
        let old = value!({
            "a": "original",
            "nested": {
                "b": 100
            },
            "unchanged": true
        });

        let new = value!({
            "a": "updated",
            "nested": {
                "b": 200,
                "c": "new"
            },
            "unchanged": true,
            "d": "added"
        });

        let changes = diff_config(&old, &new);

        // We expect 4 changes in total.
        assert_eq!(changes.len(), 4);

        assert!(changes.iter().any(|c| c.key == "a"
            && c.old_value.as_ref().and_then(|v| v.as_string()).map(|s| s.as_str()) == Some("original")
            && c.new_value.as_ref().and_then(|v| v.as_string()).map(|s| s.as_str()) == Some("updated")));
        assert!(changes.iter().any(|c| c.key == "nested.b"
            && c.old_value
                .as_ref()
                .and_then(|v| v.as_number())
                .and_then(|n| n.to_i64())
                == Some(100)
            && c.new_value
                .as_ref()
                .and_then(|v| v.as_number())
                .and_then(|n| n.to_i64())
                == Some(200)));
        assert!(changes.iter().any(|c| c.key == "nested.c"
            && c.old_value.is_none()
            && c.new_value.as_ref().and_then(|v| v.as_string()).map(|s| s.as_str()) == Some("new")));
        assert!(changes.iter().any(|c| c.key == "d"
            && c.old_value.is_none()
            && c.new_value.as_ref().and_then(|v| v.as_string()).map(|s| s.as_str()) == Some("added")));
    }

    #[test]
    fn test_diff_config_no_change() {
        let old = value!({
            "a": "original",
            "nested": {
                "b": 100
            }
        });

        let new = value!({
            "a": "original",
            "nested": {
                "b": 100
            }
        });

        let changes = diff_config(&old, &new);
        assert!(changes.is_empty());
    }
}
