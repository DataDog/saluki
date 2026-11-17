//! Application version data carried by container tags.

use saluki_core::data_model::tags::Tag;

/// Prefix of the container tag carrying the Git commit SHA of the application.
const GIT_COMMIT_SHA_TAG_PREFIX: &str = "git.commit.sha:";

/// Prefix of the container tag carrying the tag of the container image.
const IMAGE_TAG_TAG_PREFIX: &str = "image_tag:";

/// Prefix of the container tag carrying the application version.
const VERSION_TAG_PREFIX: &str = "version:";

/// Application version data found in a container's tags.
///
/// Containers often carry their version data as tags even when the spans they emit carry none. Traces and stats use
/// these values as fallbacks for the version data that the traces themselves leave empty. An empty value means that no
/// tag supplied it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContainerVersionData<'a> {
    /// Git commit SHA, from the `git.commit.sha` tag.
    pub git_commit_sha: &'a str,

    /// Container image tag, from the `image_tag` tag.
    pub image_tag: &'a str,

    /// Application version, from the `version` tag.
    pub app_version: &'a str,
}

impl<'a> ContainerVersionData<'a> {
    /// Extracts version data from container tags.
    ///
    /// For each field, the first tag with a nonempty value wins.
    pub fn from_tags<I>(tags: I) -> Self
    where
        I: IntoIterator<Item = &'a Tag>,
    {
        let mut data = Self::default();
        for tag in tags {
            let tag = tag.as_str();
            fill_from_tag(&mut data.git_commit_sha, tag, GIT_COMMIT_SHA_TAG_PREFIX);
            fill_from_tag(&mut data.image_tag, tag, IMAGE_TAG_TAG_PREFIX);
            fill_from_tag(&mut data.app_version, tag, VERSION_TAG_PREFIX);

            if !data.git_commit_sha.is_empty() && !data.image_tag.is_empty() && !data.app_version.is_empty() {
                break;
            }
        }
        data
    }
}

/// Sets `field` to the value of `tag` if `field` is still empty and `tag` starts with `prefix`.
fn fill_from_tag<'a>(field: &mut &'a str, tag: &'a str, prefix: &str) {
    if field.is_empty() {
        if let Some(value) = tag.strip_prefix(prefix) {
            *field = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::tags::TagSet;

    use super::*;

    fn tags(tags: &[&'static str]) -> TagSet {
        tags.iter().copied().map(Tag::from_static).collect()
    }

    #[test]
    fn extracts_all_version_fields() {
        let tags = tags(&[
            "kube_namespace:default",
            "git.commit.sha:abc123",
            "image_tag:1.2.3",
            "version:v4",
        ]);
        assert_eq!(
            ContainerVersionData::from_tags(&tags),
            ContainerVersionData {
                git_commit_sha: "abc123",
                image_tag: "1.2.3",
                app_version: "v4",
            }
        );
    }

    #[test]
    fn missing_tags_leave_fields_empty() {
        assert_eq!(
            ContainerVersionData::from_tags(&tags(&[])),
            ContainerVersionData::default()
        );
        assert_eq!(
            ContainerVersionData::from_tags(&tags(&["image_name:web", "versions:1", "image_tagged:2"])),
            ContainerVersionData::default()
        );
    }

    #[test]
    fn first_nonempty_value_wins() {
        let tags = tags(&["version:", "version:v1", "version:v2", "image_tag:a", "image_tag:b"]);
        let data = ContainerVersionData::from_tags(&tags);
        assert_eq!(data.app_version, "v1");
        assert_eq!(data.image_tag, "a");
        assert_eq!(data.git_commit_sha, "");
    }
}
