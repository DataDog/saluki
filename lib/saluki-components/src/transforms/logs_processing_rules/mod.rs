//! Logs processing rules transform.
//!
//! Applies an ordered list of processing rules to the message of each log, which can drop the log or mask parts of
//! its message. Only the message changes; the status, hostname, service, source, tags, and additional properties are
//! left as they are.
//!
//! The rules run in order, and each one sees the message as the rules before it left it:
//!
//! - `exclude_at_match` drops the log if its pattern matches.
//! - `include_at_match` drops the log if its pattern doesn't match.
//! - `mask_sequences` replaces every match of its pattern with its placeholder.
//!
//! The first rule that drops a log stops the processing of that log.
//!
//! Patterns and placeholders use the syntax of the `regex` crate, where `$1` and `${name}` refer to capture groups.
//! The rules are expected to have been validated already: building the transform only fails when a pattern doesn't
//! compile.

use std::borrow::Cow;

use async_trait::async_trait;
use metrics::Counter;
use regex::Regex;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{transforms::*, BuildContext},
    data_model::event::log::Log,
    observability::ComponentMetricsExt as _,
    topology::EventsBuffer,
};
use saluki_error::{ErrorContext as _, GenericError};
use saluki_metrics::MetricsBuilder;

/// A log processing rule.
#[derive(Clone, Debug, PartialEq)]
pub enum LogsProcessingRule {
    /// Drops logs whose content matches `pattern`.
    ExcludeAtMatch {
        /// Name of the rule.
        name: String,
        /// Regular expression matched against the content.
        pattern: String,
    },

    /// Drops logs whose content doesn't match `pattern`.
    IncludeAtMatch {
        /// Name of the rule.
        name: String,
        /// Regular expression matched against the content.
        pattern: String,
    },

    /// Replaces every match of `pattern` in the content with `replace_placeholder`.
    MaskSequences {
        /// Name of the rule.
        name: String,
        /// Regular expression matched against the content.
        pattern: String,
        /// Replacement for each match, where `$1` and `${name}` refer to capture groups.
        replace_placeholder: String,
    },
}

/// Logs processing rules transform configuration.
pub struct LogsProcessingRulesConfiguration {
    rules: Vec<LogsProcessingRule>,
}

impl LogsProcessingRulesConfiguration {
    /// Creates a new `LogsProcessingRulesConfiguration` that applies the given rules, in order.
    pub fn new(rules: Vec<LogsProcessingRule>) -> Self {
        Self { rules }
    }
}

#[async_trait]
impl SynchronousTransformBuilder for LogsProcessingRulesConfiguration {
    async fn build(&self, context: BuildContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        let builder = MetricsBuilder::from_component_context(context.component_context());
        Ok(Box::new(LogsProcessingRules {
            rules: compile_rules(&self.rules)?,
            events_dropped: builder.register_counter_with_tags(
                "component_events_dropped_total",
                ["intentional:true", "drop_reason:processing_rule"],
            ),
        }))
    }
}

impl MemoryBounds for LogsProcessingRulesConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // The memory that the compiled regular expressions hold internally isn't known, so it isn't counted.
        let placeholder_bytes = self
            .rules
            .iter()
            .map(|rule| match rule {
                LogsProcessingRule::MaskSequences {
                    replace_placeholder, ..
                } => replace_placeholder.len(),
                _ => 0,
            })
            .sum();
        builder
            .minimum()
            .with_single_value::<LogsProcessingRules>("component struct")
            .with_array::<CompiledRule>("compiled rules", self.rules.len())
            .with_fixed_amount("placeholders", placeholder_bytes);
    }
}

/// A rule with its pattern compiled.
#[derive(Debug)]
enum CompiledRule {
    Exclude(Regex),
    Include(Regex),
    Mask { regex: Regex, placeholder: String },
}

/// Compiles the rules, in order.
///
/// # Errors
///
/// Returns an error naming the rule if a pattern doesn't compile.
fn compile_rules(rules: &[LogsProcessingRule]) -> Result<Vec<CompiledRule>, GenericError> {
    let compile = |name: &str, pattern: &str| {
        Regex::new(pattern)
            .with_error_context(|| format!("Invalid pattern '{pattern}' for logs processing rule '{name}'."))
    };

    rules
        .iter()
        .map(|rule| match rule {
            LogsProcessingRule::ExcludeAtMatch { name, pattern } => Ok(CompiledRule::Exclude(compile(name, pattern)?)),
            LogsProcessingRule::IncludeAtMatch { name, pattern } => Ok(CompiledRule::Include(compile(name, pattern)?)),
            LogsProcessingRule::MaskSequences {
                name,
                pattern,
                replace_placeholder,
            } => Ok(CompiledRule::Mask {
                regex: compile(name, pattern)?,
                placeholder: replace_placeholder.clone(),
            }),
        })
        .collect()
}

/// Logs processing rules transform.
struct LogsProcessingRules {
    rules: Vec<CompiledRule>,
    events_dropped: Counter,
}

impl LogsProcessingRules {
    /// Applies the rules to `log`, returning `false` if a rule drops it.
    fn process(&self, log: &mut Log) -> bool {
        let mut content = Cow::Borrowed(log.message());
        for rule in &self.rules {
            match rule {
                CompiledRule::Exclude(regex) => {
                    if regex.is_match(&content) {
                        return false;
                    }
                }
                CompiledRule::Include(regex) => {
                    if !regex.is_match(&content) {
                        return false;
                    }
                }
                CompiledRule::Mask { regex, placeholder } => {
                    let masked = match regex.replace_all(&content, placeholder.as_str()) {
                        Cow::Borrowed(_) => None,
                        Cow::Owned(masked) => Some(masked),
                    };
                    if let Some(masked) = masked {
                        content = Cow::Owned(masked);
                    }
                }
            }
        }

        // A mask can replace a match with the same text, so an owned result isn't necessarily a changed one.
        let changed = match content {
            Cow::Owned(content) if content != log.message() => Some(content),
            _ => None,
        };
        if let Some(content) = changed {
            log.set_message(content);
        }
        true
    }
}

impl SynchronousTransform for LogsProcessingRules {
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer) {
        if self.rules.is_empty() {
            return;
        }

        let mut dropped = 0;
        buffer.retain_events::<Log, _>(|log| {
            let keep = self.process(log);
            if !keep {
                dropped += 1;
            }
            keep
        });
        if dropped > 0 {
            self.events_dropped.increment(dropped);
        }
    }
}

#[cfg(test)]
mod tests;
