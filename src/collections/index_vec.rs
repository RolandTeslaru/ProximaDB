//! A list indexed by one kind of id: `list[id]` works, a bare number or another kind of id doesn't compile.

use std::marker::PhantomData;
use std::ops::{Index, IndexMut};

/// An id that numbers positions in an `IndexVec`.
pub trait Idx: Copy {
    fn from_index(i: usize) -> Self;
    fn index(self) -> usize;
}

pub struct IndexVec<I: Idx, T> {
    items: Vec<T>,
    _id: PhantomData<fn(I)>,
}

impl<I: Idx, T> IndexVec<I, T> {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            _id: PhantomData,
        }
    }

    /// One item per id, `0..n`.
    pub fn from_fn(n: usize, f: impl FnMut(I) -> T) -> Self {
        (0..n).map(I::from_index).map(f).collect()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Appends an item and returns its id.
    pub fn push(&mut self, item: T) -> I {
        self.items.push(item);
        I::from_index(self.items.len() - 1)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.items.iter()
    }

    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.items.iter_mut()
    }

    /// Every id with its item.
    pub fn iter_ids(&self) -> impl Iterator<Item = (I, &T)> {
        self.items
            .iter()
            .enumerate()
            .map(|(i, t)| (I::from_index(i), t))
    }

    /// Every id, `0..len`.
    pub fn ids(&self) -> impl Iterator<Item = I> + use<I, T> {
        (0..self.items.len()).map(I::from_index)
    }

    /// The id of the first item that satisfies `f`.
    pub fn position(&self, f: impl FnMut(&T) -> bool) -> Option<I> {
        self.items.iter().position(f).map(I::from_index)
    }
}

impl<I: Idx, T: Clone> IndexVec<I, T> {
    /// `n` copies of `item`.
    pub fn filled(n: usize, item: T) -> Self {
        Self {
            items: vec![item; n],
            _id: PhantomData,
        }
    }
}

impl<I: Idx, T> Default for IndexVec<I, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I: Idx, T: Clone> Clone for IndexVec<I, T> {
    fn clone(&self) -> Self {
        Self {
            items: self.items.clone(),
            _id: PhantomData,
        }
    }
}

impl<I: Idx, T> Index<I> for IndexVec<I, T> {
    type Output = T;

    #[inline]
    fn index(&self, i: I) -> &T {
        &self.items[i.index()]
    }
}

impl<I: Idx, T> IndexMut<I> for IndexVec<I, T> {
    #[inline]
    fn index_mut(&mut self, i: I) -> &mut T {
        &mut self.items[i.index()]
    }
}

impl<I: Idx, T> FromIterator<T> for IndexVec<I, T> {
    fn from_iter<It: IntoIterator<Item = T>>(iter: It) -> Self {
        Self {
            items: iter.into_iter().collect(),
            _id: PhantomData,
        }
    }
}

impl<'a, I: Idx, T> IntoIterator for &'a IndexVec<I, T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl<I: Idx, T> IntoIterator for IndexVec<I, T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}
