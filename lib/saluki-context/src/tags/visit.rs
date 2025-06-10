use saluki_common::collections::FastHashSet;

use super::Tag;

/// A value containing tags that can be visited.
pub trait Tagged {
    /// Visits the tags in this value.
    fn visit_tags<F>(&self, visitor: F)
    where
        F: FnMut(&Tag);

    /// Visits the tags in this value, only calling the visitor once for each unique tag.
    ///
    /// This method allocates in order to track which tags have been visited.
    fn visit_tags_deduped<F>(&self, mut visitor: F)
    where
        F: FnMut(&Tag),
    {
        let mut seen = FastHashSet::default();
        self.visit_tags(|tag| {
            if !seen.contains(tag) {
                seen.insert(tag.clone());
                visitor(tag);
            }
        });
    }
}

/// A tag visitor.
pub trait TagVisitor {
    /// Visits a tag.
    fn visit_tag(&mut self, tag: &Tag);
}

impl<F> TagVisitor for F
where
    F: FnMut(&Tag),
{
    fn visit_tag(&mut self, tag: &Tag) {
        self(tag)
    }
}
