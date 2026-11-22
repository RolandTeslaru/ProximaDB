//! Sets of nodes, one bit per node.

use std::simd::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};

type Lanes = Simd<u64, 4>; // 4 × 64 bits = 256 nodes per operation

/// A set of nodes, one bit per node (64 nodes per `u64`). Used for "is a User", "visited", "in the current step",
/// and the yes/no lists built from conditions.
#[derive(Clone, Debug)]
pub struct Bitset {
    words: Vec<u64>,
    len: usize,
}

impl Bitset {
    pub fn new(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)],
            len,
        }
    }

    pub fn from_words(words: Vec<u64>, len: usize) -> Self {
        debug_assert_eq!(words.len(), len.div_ceil(64));
        Self { words, len }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, i: usize) -> bool {
        (self.words[i >> 6] >> (i & 63)) & 1 == 1
    }

    #[inline]
    pub fn set(&mut self, i: usize) {
        self.words[i >> 6] |= 1 << (i & 63);
    }

    #[inline]
    pub fn unset(&mut self, i: usize) {
        self.words[i >> 6] &= !(1 << (i & 63));
    }

    /// Adds one more node at the end (for appended nodes).
    pub fn push(&mut self, value: bool) {
        if self.len % 64 == 0 {
            self.words.push(0);
        }
        self.len += 1;
        if value {
            self.set(self.len - 1);
        }
    }

    pub fn words(&self) -> &[u64] {
        &self.words
    }

    pub fn words_mut(&mut self) -> &mut [u64] {
        &mut self.words
    }

    pub fn bytes(&self) -> usize {
        self.words.capacity() * 8
    }

    pub fn count(&self) -> u64 {
        self.words.iter().map(|w| w.count_ones() as u64).sum()
    }

    pub fn clear(&mut self) {
        self.words.fill(0);
    }

    /// self = self AND other, 256 nodes per instruction.
    pub fn and_assign(&mut self, other: &Bitset) {
        self.zip_with(other, |a, b| a & b, |a, b| a & b);
    }

    /// self = self OR other.
    pub fn or_assign(&mut self, other: &Bitset) {
        self.zip_with(other, |a, b| a | b, |a, b| a | b);
    }

    /// self = self AND NOT other.
    pub fn and_not_assign(&mut self, other: &Bitset) {
        self.zip_with(other, |a, b| a & !b, |a, b| a & !b);
    }

    /// self = NOT self (within `len`).
    pub fn not_assign(&mut self) {
        for w in &mut self.words {
            *w = !*w;
        }
        self.trim();
    }

    fn zip_with(
        &mut self,
        other: &Bitset,
        lanes: impl Fn(Lanes, Lanes) -> Lanes,
        word: impl Fn(u64, u64) -> u64,
    ) {
        assert_eq!(self.len, other.len);
        let (mine, my_rest) = self.words.as_chunks_mut::<4>();
        let (theirs, their_rest) = other.words.as_chunks::<4>();

        for (x, y) in mine.iter_mut().zip(theirs) {
            lanes(Lanes::from_array(*x), Lanes::from_array(*y)).copy_to_slice(x);
        }

        for (x, y) in my_rest.iter_mut().zip(their_rest) {
            *x = word(*x, *y);
        }
    }

    fn trim(&mut self) {
        let extra = self.words.len() * 64 - self.len;

        if extra > 0
            && let Some(last) = self.words.last_mut()
        {
            *last &= u64::MAX >> extra;
        }
    }

    /// The positions of the 1-bits, in order; each word stays in a register while its bits are pulled out.
    pub fn ones(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(w, &word)| {
            let mut bits = word;
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let i = w * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(i)
            })
        })
    }
}

/// A `Bitset` several threads can mark at once.
pub struct AtomicBitset {
    words: Vec<AtomicU64>,
    len: usize,
}

impl AtomicBitset {
    pub fn new(len: usize) -> Self {
        Self {
            words: (0..len.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
            len,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, i: usize) -> bool {
        (self.words[i >> 6].load(Ordering::Relaxed) >> (i & 63)) & 1 == 1
    }

    /// Sets the bit; true only for the caller that set it first.
    #[inline]
    pub fn test_and_set(&self, i: usize) -> bool {
        let word = &self.words[i >> 6];
        let mask = 1u64 << (i & 63);
        if word.load(Ordering::Relaxed) & mask != 0 {
            return false;
        }
        word.fetch_or(mask, Ordering::Relaxed) & mask == 0
    }

    #[inline]
    pub fn word(&self, w: usize) -> u64 {
        self.words[w].load(Ordering::Relaxed)
    }

    pub fn word_count(&self) -> usize {
        self.words.len()
    }

    pub fn clear(&self) {
        for w in &self.words {
            w.store(0, Ordering::Relaxed);
        }
    }
}
