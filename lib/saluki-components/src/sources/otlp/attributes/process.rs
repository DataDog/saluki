//! OTLP `process` semantic conventions.

use opentelemetry_semantic_conventions::resource::{
    PROCESS_COMMAND, PROCESS_COMMAND_LINE, PROCESS_EXECUTABLE_NAME, PROCESS_EXECUTABLE_PATH,
};

#[derive(Debug, Default)]
pub(super) struct ProcessAttributes {
    pub executable_name: String,
    pub executable_path: String,
    pub command: String,
    pub command_line: String,
    pub pid: i64,
    pub owner: String,
}

impl ProcessAttributes {
    /// Extracts a list of Datadog tags from the process attributes.
    pub fn extract_tags(&self) -> Vec<String> {
        let mut tags = Vec::new();

        if !self.executable_name.is_empty() {
            tags.push(format!("{}:{}", PROCESS_EXECUTABLE_NAME, self.executable_name));
        } else if !self.executable_path.is_empty() {
            tags.push(format!("{}:{}", PROCESS_EXECUTABLE_PATH, self.executable_path));
        } else if !self.command.is_empty() {
            tags.push(format!("{}:{}", PROCESS_COMMAND, self.command));
        } else if !self.command_line.is_empty() {
            tags.push(format!("{}:{}", PROCESS_COMMAND_LINE, self.command_line));
        }

        tags
    }
}
