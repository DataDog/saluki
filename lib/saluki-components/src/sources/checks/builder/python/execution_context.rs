use std::collections::HashMap;

// Cache execution information from datadog agent for Python checks
#[derive(Clone)]
pub struct ExecutionContext {
    pub hostname: String,
    pub http_headers: HashMap<String, String>,
    pub tracemalloc_enabled: bool,
}