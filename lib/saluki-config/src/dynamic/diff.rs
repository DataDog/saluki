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
                            changes.push(ConfigChangeEvent {
                                key: current_path,
                                old_value: Some(serde_json::to_value(old_value).unwrap()),
                                new_value: Some(serde_json::to_value(new_value).unwrap()),
                            });
                        }
                    }
                }
                None => {
                    changes.push(ConfigChangeEvent {
                        key: current_path,
                        old_value: None,
                        new_value: Some(serde_json::to_value(new_value).unwrap()),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use figment::{providers::Serialized, value::Value, Figment};
    use serde_json::json;

    use super::*;

    fn to_figment_value(json: serde_json::Value) -> Value {
        let serialized = Serialized::defaults(json);
        let value: Value = Figment::from(serialized).extract().unwrap();
        value
    }

    #[test]
    fn test_diff_config_basic() {
        let old_json = json!({
            "a": "original",
            "nested": {
                "b": 100
            },
            "unchanged": true
        });

        let new_json = json!({
            "a": "updated", // modified
            "nested": {
                "b": 200, // nested modified
                "c": "new"  // nested added
            },
            "unchanged": true,
            "d": "added" // added
        });

        let old_config = to_figment_value(old_json);
        let new_config = to_figment_value(new_json);

        let changes = diff_config(&old_config, &new_config);

        // We expect 4 changes in total.
        assert_eq!(changes.len(), 4);

        println!("changes: {:?}", changes);

        assert!(changes.contains(&ConfigChangeEvent {
            key: "a".to_string(),
            old_value: Some("original".into()),
            new_value: Some("updated".into())
        }));
        assert!(changes.contains(&ConfigChangeEvent {
            key: "nested.b".to_string(),
            old_value: Some(100.into()),
            new_value: Some(200.into())
        }));
        assert!(changes.contains(&ConfigChangeEvent {
            key: "nested.c".to_string(),
            old_value: None,
            new_value: Some("new".into())
        }));
        assert!(changes.contains(&ConfigChangeEvent {
            key: "d".to_string(),
            old_value: None,
            new_value: Some("added".into())
        }));
    }

    #[test]
    fn test_diff_config_no_change() {
        let old_json = json!({
            "a": "original",
            "nested": {
                "b": 100
            },
        });

        let new_json = old_json.clone();

        let old_config = to_figment_value(old_json);
        let new_config = to_figment_value(new_json);

        let changes = diff_config(&old_config, &new_config);

        assert!(changes.is_empty());
    }
}
