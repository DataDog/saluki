use std::collections::HashMap;

use super::builder::TagsBuilder;

#[allow(dead_code)]
#[derive(Eq, Hash, PartialEq)]
enum NameMatcher {
    Fixed(String),
    Glob(String),
}

impl NameMatcher {
    fn matches(&self, name: &str) -> bool {
        let name = name.to_lowercase();
        match self {
            Self::Fixed(fixed) => fixed == &name,
            Self::Glob(glob) => glob == &name,
        }
    }
}

#[allow(dead_code)]
enum TemplateSegment {
    Fixed(String),
    Replacement { variable_name: String },
}

struct Template {
    segments: Vec<TemplateSegment>,
}

impl Template {
    #[allow(dead_code)]
    pub fn from_string(_template: String) -> Self {
        // TODO: We need to do a regex match on anything with the format `%%(.*?)%%` to extract the template variables,
        // and then we'll build up segments by alternating between the non-matched portions and the matched portions.
        //
        // This will let us essentially iterate over all segments, either pushing a fixed string or the variable
        // replacement value, rather than modifying the string in place, over and over.
        //
        // In the average case, where the template is a single variable, this means we never have to allocate a clone
        // upfront to modify, and instead we only have the one allocation for doing our single replacement.
        todo!()
    }

    pub fn resolve(&self, replacement_value: &str) -> String {
        let mut result = String::new();

        for segment in &self.segments {
            match segment {
                TemplateSegment::Fixed(fixed) => result.push_str(fixed),
                TemplateSegment::Replacement { variable_name } => {
                    // We only insert the replacement value if this template variable is allowed to be used, otherwise
                    // we essentially ignore it, which removes it entirely from the resolved output.
                    //
                    // TODO: don't even create the segment if the variable name isn't allowed
                    if is_allowed_template_variable(variable_name) {
                        result.push_str(replacement_value);
                    }
                }
            }
        }

        result
    }
}

pub struct TagsExtractor {
    patterns: HashMap<NameMatcher, Vec<Template>>,
}

impl TagsExtractor {
    pub fn from_patterns(raw_patterns: HashMap<String, String>) -> Result<Self, String> {
        let mut patterns = HashMap::new();

        for (key, _value) in raw_patterns {
            let matcher = if key.contains('*') {
                // TODO: Actually compile glob matcher.
                todo!()
            } else {
                NameMatcher::Fixed(key)
            };

            // TODO: Parse pattern value as `Template`s, split by comma.
            let templates = Vec::new();

            patterns.insert(matcher, templates);
        }

        Ok(Self { patterns })
    }

    pub fn extract(&self, name: &str, value: &str, tags: &mut TagsBuilder) {
        for (matcher, templates) in &self.patterns {
            if !matcher.matches(name) {
                continue;
            }

            // Iterate through all of the templates for this pattern, generating the tag name to add by resolving the
            // template against the input tag name, while using the input tag value as the value for all tags generated
            // this way.
            for template in templates {
                let templated_name = template.resolve(name);
                tags.add_automatic(templated_name, value);
            }
        }
    }
}

fn is_allowed_template_variable(variable_name: &str) -> bool {
    matches!(variable_name, "label" | "annotation" | "env")
}
