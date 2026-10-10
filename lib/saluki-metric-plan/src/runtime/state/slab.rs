//! A slab whose entries are named by tagged handles, so that a handle to a freed entry never names the entry that
//! reuses its place.

use std::fmt;

/// A handle to one instance of a state owner's state in an isolation slot's region: its place in the region's slab,
/// and the tag the place had when the instance was inserted.
///
/// Every insertion gives its place a new tag, so a handle kept after its instance was freed, by a drain cursor or a
/// program, is stale: it names nothing, even once another instance takes the place.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InstanceHandle {
    index: u32,
    tag: u32,
}

impl fmt::Display for InstanceHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.index, self.tag)
    }
}

/// One place of a slab.
struct Place<T> {
    tag: u32,
    entry: Option<T>,
}

/// Entries named by [tagged handles][InstanceHandle], kept in insertion places that are reused once freed.
///
/// Iteration visits entries in place order, which is the order the region drives its state owners in.
pub struct Slab<T> {
    places: Vec<Place<T>>,
    free: Vec<u32>,
}

impl<T> Default for Slab<T> {
    fn default() -> Self {
        Self {
            places: Vec::new(),
            free: Vec::new(),
        }
    }
}

impl<T> Slab<T> {
    /// Inserts an entry, returning its handle.
    ///
    /// The entry takes a freed place, if there is one, with a new tag.
    pub fn insert(&mut self, entry: T) -> InstanceHandle {
        match self.free.pop() {
            Some(index) => {
                let place = &mut self.places[index as usize];
                place.tag = place.tag.wrapping_add(1);
                place.entry = Some(entry);
                InstanceHandle { index, tag: place.tag }
            }
            None => {
                let index = u32::try_from(self.places.len()).expect("a region holds fewer than 2^32 instances");
                self.places.push(Place {
                    tag: 0,
                    entry: Some(entry),
                });
                InstanceHandle { index, tag: 0 }
            }
        }
    }

    /// Returns the entry a handle names, or `None` if the handle is stale.
    pub fn get(&self, handle: InstanceHandle) -> Option<&T> {
        let place = self.places.get(handle.index as usize)?;
        (place.tag == handle.tag).then_some(())?;
        place.entry.as_ref()
    }

    /// Returns the entry a handle names, mutably, or `None` if the handle is stale.
    pub fn get_mut(&mut self, handle: InstanceHandle) -> Option<&mut T> {
        let place = self.places.get_mut(handle.index as usize)?;
        (place.tag == handle.tag).then_some(())?;
        place.entry.as_mut()
    }

    /// Removes the entry a handle names, freeing its place, or returns `None` if the handle is stale.
    pub fn remove(&mut self, handle: InstanceHandle) -> Option<T> {
        let place = self.places.get_mut(handle.index as usize)?;
        if place.tag != handle.tag {
            return None;
        }
        let entry = place.entry.take()?;
        self.free.push(handle.index);
        Some(entry)
    }

    /// Returns the handle of every entry, in place order.
    pub fn handles(&self) -> Vec<InstanceHandle> {
        self.iter().map(|(handle, _)| handle).collect()
    }

    /// Returns every entry with its handle, in place order.
    pub fn iter(&self) -> impl Iterator<Item = (InstanceHandle, &T)> {
        self.places.iter().enumerate().filter_map(|(index, place)| {
            let entry = place.entry.as_ref()?;
            Some((
                InstanceHandle {
                    index: index as u32,
                    tag: place.tag,
                },
                entry,
            ))
        })
    }

    /// Returns every entry with its handle, mutably, in place order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (InstanceHandle, &mut T)> {
        self.places.iter_mut().enumerate().filter_map(|(index, place)| {
            let tag = place.tag;
            let entry = place.entry.as_mut()?;
            Some((
                InstanceHandle {
                    index: index as u32,
                    tag,
                },
                entry,
            ))
        })
    }

    /// Returns the number of entries.
    pub fn len(&self) -> usize {
        self.places.len() - self.free.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_to_a_removed_entry_never_names_the_entry_that_reuses_its_place() {
        let mut slab = Slab::default();
        let first = slab.insert("first");
        let second = slab.insert("second");
        assert_eq!(slab.len(), 2);
        assert_eq!(slab.get(first), Some(&"first"));

        assert_eq!(slab.remove(first), Some("first"));
        assert_eq!(slab.get(first), None, "a removed entry's handle is stale");
        assert_eq!(slab.remove(first), None, "a stale handle removes nothing");

        // The place is reused under a new tag: the old handle still names nothing.
        let reused = slab.insert("reused");
        assert_eq!(
            (first.to_string(), reused.to_string()),
            ("0#0".to_string(), "0#1".to_string())
        );
        assert_eq!(slab.get(first), None);
        assert_eq!(slab.get_mut(first), None);
        assert_eq!(slab.get(reused), Some(&"reused"));
        assert_eq!(slab.handles(), vec![reused, second], "iteration is in place order");
        assert_eq!(slab.len(), 2);

        for (_, entry) in slab.iter_mut() {
            *entry = "changed";
        }
        assert_eq!(
            slab.iter().map(|(_, entry)| *entry).collect::<Vec<_>>(),
            vec!["changed"; 2]
        );
        slab.remove(reused);
        slab.remove(second);
        assert_eq!(slab.len(), 0);
        assert_eq!(slab.handles(), vec![]);
    }
}
