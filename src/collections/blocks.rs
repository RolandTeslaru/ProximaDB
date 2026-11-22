//! Lock-free growable storage for edits: one writer, any number of readers, nothing ever moves.
//!
//! - `BlockList<T>`: an append-only list in fixed-size blocks. Entry i is in block i / BLOCK at position i % BLOCK.
//! - `AtomicBlocks`: a growable array of `AtomicU64` with a fill value, for entries that change after they are
//!   written (side-store heads, bits of growable bitsets).
//!
//! Readers only read entries below the published length (or atomics), and the writer only writes entries above it,
//! so a reader never sees a half-written entry.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

pub const BLOCK_BITS: u32 = 16;
pub const BLOCK: usize = 1 << BLOCK_BITS;
const MAX_BLOCKS: usize = 1 << 14; // 16,384 blocks × 65,536 = about 1 billion entries per list

#[inline]
fn split(i: u64) -> (usize, usize) {
    ((i >> BLOCK_BITS) as usize, (i as usize) & (BLOCK - 1))
}

struct Slot<T>(UnsafeCell<MaybeUninit<T>>);

/// Append-only list in fixed-size blocks.
pub struct BlockList<T: Copy> {
    dir: Box<[AtomicPtr<Slot<T>>]>,
    len: AtomicU64,
}

unsafe impl<T: Copy + Send> Sync for BlockList<T> {}
unsafe impl<T: Copy + Send> Send for BlockList<T> {}

impl<T: Copy> Default for BlockList<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy> BlockList<T> {
    pub fn new() -> Self {
        Self {
            dir: (0..MAX_BLOCKS)
                .map(|_| AtomicPtr::new(null_mut()))
                .collect(),
            len: AtomicU64::new(0),
        }
    }

    /// Entries visible to readers.
    #[inline]
    pub fn len(&self) -> u64 {
        self.len.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends one entry and returns its index. Only the single writer calls this.
    pub fn push(&self, value: T) -> u64 {
        let i = self.len.load(Ordering::Relaxed);
        let (b, o) = split(i);
        assert!(b < MAX_BLOCKS, "block list full");
        let mut block = self.dir[b].load(Ordering::Acquire);
        if block.is_null() {
            let fresh: Box<[Slot<T>]> = (0..BLOCK)
                .map(|_| Slot(UnsafeCell::new(MaybeUninit::uninit())))
                .collect();
            block = Box::into_raw(fresh) as *mut Slot<T>;
            self.dir[b].store(block, Ordering::Release);
        }
        // Safety: slot i is beyond the published length, so no reader touches it until the store below.
        unsafe { (*(*block.add(o)).0.get()).write(value) };
        self.len.store(i + 1, Ordering::Release);
        i
    }

    #[inline]
    pub fn get(&self, i: u64) -> Option<T> {
        if i >= self.len() {
            return None;
        }
        let (b, o) = split(i);
        let block = self.dir[b].load(Ordering::Acquire);
        // Safety: i is below the published length, so the slot was written before the length was stored.
        Some(unsafe { (*(*block.add(o)).0.get()).assume_init() })
    }

    pub fn bytes(&self) -> usize {
        let blocks = self
            .dir
            .iter()
            .filter(|p| !p.load(Ordering::Relaxed).is_null())
            .count();
        blocks * BLOCK * size_of::<T>() + self.dir.len() * 8
    }
}

impl<T: Copy> Drop for BlockList<T> {
    fn drop(&mut self) {
        for p in self.dir.iter() {
            let block = p.load(Ordering::Relaxed);
            if !block.is_null() {
                // Safety: every block was made from a boxed slice of BLOCK slots; T is Copy, so nothing to drop inside.
                unsafe {
                    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                        block, BLOCK,
                    )))
                };
            }
        }
    }
}

/// A growable array of `AtomicU64`, every entry starting at `fill`. Blocks are added by the writer as needed.
pub struct AtomicBlocks {
    dir: Box<[AtomicPtr<AtomicU64>]>,
    fill: u64,
}

impl AtomicBlocks {
    pub fn new(fill: u64) -> Self {
        Self {
            dir: (0..MAX_BLOCKS)
                .map(|_| AtomicPtr::new(null_mut()))
                .collect(),
            fill,
        }
    }

    #[inline]
    pub fn load(&self, i: u64) -> u64 {
        let (b, o) = split(i);
        if b >= MAX_BLOCKS {
            return self.fill;
        }
        let block = self.dir[b].load(Ordering::Acquire);
        if block.is_null() {
            return self.fill;
        }
        // Safety: a non-null block points to BLOCK initialised atomics.
        unsafe { (*block.add(o)).load(Ordering::Acquire) }
    }

    fn slot(&self, i: u64) -> &AtomicU64 {
        let (b, o) = split(i);
        assert!(b < MAX_BLOCKS, "atomic blocks full");
        let mut block = self.dir[b].load(Ordering::Acquire);
        if block.is_null() {
            let fresh: Box<[AtomicU64]> = (0..BLOCK).map(|_| AtomicU64::new(self.fill)).collect();
            block = Box::into_raw(fresh) as *mut AtomicU64;
            self.dir[b].store(block, Ordering::Release);
        }
        // Safety: as in `load`.
        unsafe { &*block.add(o) }
    }

    /// Only the single writer calls this.
    pub fn store(&self, i: u64, value: u64) {
        self.slot(i).store(value, Ordering::Release);
    }

    /// Only the single writer calls this.
    pub fn fetch_or(&self, i: u64, value: u64) {
        self.slot(i).fetch_or(value, Ordering::Release);
    }

    pub fn bytes(&self) -> usize {
        let blocks = self
            .dir
            .iter()
            .filter(|p| !p.load(Ordering::Relaxed).is_null())
            .count();
        blocks * BLOCK * 8 + self.dir.len() * 8
    }
}

impl Drop for AtomicBlocks {
    fn drop(&mut self) {
        for p in self.dir.iter() {
            let block = p.load(Ordering::Relaxed);
            if !block.is_null() {
                // Safety: every block was made from a boxed slice of BLOCK atomics.
                unsafe {
                    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                        block, BLOCK,
                    )))
                };
            }
        }
    }
}

/// A growable set of nodes, one bit per node, written by the single writer and read by anyone.
pub struct GrowBitset {
    words: AtomicBlocks,
}

impl Default for GrowBitset {
    fn default() -> Self {
        Self::new()
    }
}

impl GrowBitset {
    pub fn new() -> Self {
        Self {
            words: AtomicBlocks::new(0),
        }
    }

    #[inline]
    pub fn get(&self, i: u64) -> bool {
        (self.words.load(i >> 6) >> (i & 63)) & 1 == 1
    }

    pub fn set(&self, i: u64) {
        self.words.fetch_or(i >> 6, 1 << (i & 63));
    }

    pub fn bytes(&self) -> usize {
        self.words.bytes()
    }
}
