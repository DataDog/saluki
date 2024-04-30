mod builder;
pub use self::builder::TagsBuilder;

mod env;
pub use self::env::EnvironmentVariableTagFilter;

mod extract;
pub use self::extract::TagsExtractor;

pub static STANDARD_ENV_VAR_TAGS: &[(&str, &str)] =
    &[("DD_ENV", "env"), ("DD_VERSION", "version"), ("DD_SERVICE", "service")];

pub const AUTODISCOVERY_TAGS_LABEL_KEY: &str = "com.datadoghq.ad.tags";
