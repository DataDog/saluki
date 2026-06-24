//! Generic interning for dictionary deduplication.

use std::{collections::HashMap, hash::Hash};

/// Generic interning structure for dictionary deduplication.
///
/// Assigns unique 1-based IDs to values, returning the same ID for duplicate values.
/// ID 0 is reserved for "empty/none" in the V3 format.
#[derive(Debug)]
pub struct Interner<K: Eq + Hash> {
    index: HashMap<K, i64>,
    last_id: i64,
}

impl<K: Eq + Hash> Default for Interner<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash> Interner<K> {
    /// Creates a new empty interner.
    pub fn new() -> Self {
        Self {
            index: HashMap::new(),
            last_id: 0,
        }
    }

    /// Gets the ID for a key, inserting it if not present.
    ///
    /// Returns `(id, is_new)` where `is_new` is true if the key was newly inserted.
    /// IDs are 1-based (0 is reserved for empty/none values).
    pub fn get_or_insert(&mut self, key: K) -> (i64, bool) {
        if let Some(&id) = self.index.get(&key) {
            (id, false)
        } else {
            self.last_id += 1;
            self.index.insert(key, self.last_id);
            (self.last_id, true)
        }
    }

    /// Returns the number of interned values.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Returns true if no values have been interned.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interner_basic() {
        let mut interner: Interner<String> = Interner::new();

        // First insertion returns ID 1 and is_new=true
        let (id1, is_new1) = interner.get_or_insert("hello".to_string());
        assert_eq!(id1, 1);
        assert!(is_new1);

        // Second insertion of same value returns same ID and is_new=false
        let (id2, is_new2) = interner.get_or_insert("hello".to_string());
        assert_eq!(id2, 1);
        assert!(!is_new2);

        // New value gets next ID
        let (id3, is_new3) = interner.get_or_insert("world".to_string());
        assert_eq!(id3, 2);
        assert!(is_new3);

        assert_eq!(interner.len(), 2);
    }

    #[test]
    fn test_interner_tuples() {
        let mut interner: Interner<(i32, i32, i32)> = Interner::new();

        let (id1, _) = interner.get_or_insert((1, 2, 3));
        let (id2, _) = interner.get_or_insert((1, 2, 3));
        let (id3, _) = interner.get_or_insert((4, 5, 6));

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }
}
