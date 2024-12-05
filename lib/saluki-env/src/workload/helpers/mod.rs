use std::{fmt, slice, vec};

#[cfg(target_os = "linux")]
pub mod cgroups;
pub mod containerd;

/// Container that can hold one or many values of a given type.
#[derive(Clone)]
pub enum OneOrMany<T> {
    /// Single value.
    One(T),

    /// Multiple values.
    Many(Vec<T>),
}

impl<T> fmt::Debug for OneOrMany<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::One(x) => write!(f, "{:?}", x),
            Self::Many(xs) => {
                write!(f, "[")?;
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{:?}", x)?;
                }
                write!(f, "]")
            }
        }
    }
}

impl<T> From<T> for OneOrMany<T> {
    fn from(value: T) -> Self {
        Self::One(value)
    }
}

impl<T> From<Vec<T>> for OneOrMany<T> {
    fn from(values: Vec<T>) -> Self {
        Self::Many(values)
    }
}

impl<'a, T> IntoIterator for &'a OneOrMany<T> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        match self {
            OneOrMany::One(value) => slice::from_ref(value).iter(),
            OneOrMany::Many(values) => values.iter(),
        }
    }
}

impl<'a, T> IntoIterator for &'a mut OneOrMany<T> {
    type Item = &'a mut T;
    type IntoIter = slice::IterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        match self {
            OneOrMany::One(value) => slice::from_mut(value).iter_mut(),
            OneOrMany::Many(values) => values.iter_mut(),
        }
    }
}

impl<T> IntoIterator for OneOrMany<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        let state = match self {
            Self::One(value) => IterState::One(Some(value)),
            Self::Many(values) => IterState::Many(values.into_iter()),
        };

        IntoIter { state }
    }
}

enum IterState<T> {
    One(Option<T>),
    Many(vec::IntoIter<T>),
}

/// An iterator that moves out of a `OneOrMany`.
pub struct IntoIter<T> {
    state: IterState<T>,
}

impl<T> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        match &mut self.state {
            IterState::One(value) => value.take(),
            IterState::Many(values) => values.next(),
        }
    }
}
