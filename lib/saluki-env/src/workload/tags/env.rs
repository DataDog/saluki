use std::collections::HashMap;

use saluki_config::GenericConfiguration;

static DEFAULT_ALLOWED_ENV_VARS: &[&str] = &[
    "DD_ENV",
    "DD_VERSION",
    "DD_SERVICE",
    "CHRONOS_JOB_NAME",
    "CHRONOS_JOB_OWNER",
    "NOMAD_TASK_NAME",
    "NOMAD_JOB_NAME",
    "NOMAD_GROUP_NAME",
    "NOMAD_NAMESPACE",
    "NOMAD_DC",
    "MESOS_TASK_ID",
    "ECS_CONTAINER_METADATA_URI",
    "ECS_CONTAINER_METADATA_URI_V4",
    // Specifically for detecting Agent contains.
    "DOCKER_DD_AGENT",
];

pub struct EnvironmentVariableTagFilter {
    allowed_variables: Vec<String>,
}

impl EnvironmentVariableTagFilter {
    pub fn from_config(config: &GenericConfiguration) -> Self {
        let docker_env_vars_allowed = config.get_typed_or_default::<HashMap<String, String>>("docker_env_as_tags");
        let container_env_vars_allowed =
            config.get_typed_or_default::<HashMap<String, String>>("container_env_as_tags");

        let mut allowed_variables = Vec::new();
        for (name, _) in docker_env_vars_allowed {
            allowed_variables.push(name.to_uppercase());
        }

        for (name, _) in container_env_vars_allowed {
            allowed_variables.push(name.to_uppercase());
        }

        for name in DEFAULT_ALLOWED_ENV_VARS {
            allowed_variables.push(name.to_uppercase());
        }

        Self { allowed_variables }
    }

    pub fn is_allowed(&self, variable_name: &str) -> bool {
        let variable_name = variable_name.to_uppercase();
        self.allowed_variables.contains(&variable_name)
    }
}
