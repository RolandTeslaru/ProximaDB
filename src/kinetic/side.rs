//! The side store: connections added since the last rebuild, per link type table.
//!
//! - `heads`: per node, the position of its first chunk (or NONE);
//! - `pool`: chunks of up to 8 entries (target node, link number, sequence number), each linked to the node's next
//!   chunk. A chunk's entries are written before its count is raised, so a reader that reads the count first only
//!   ever sees whole entries. Removing an entry stores the removing action's sequence number in its `removed` slot.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

use crate::collections::blocks::AtomicBlocks;
use crate::core::{link, node};

pub const NONE: u64 = u64::MAX;
pub const CHUNK: usize = 8;
const POOL_BLOCK_BITS: u32 = 12;
const POOL_BLOCK: usize = 1 << POOL_BLOCK_BITS; // 4,096 chunks per block
const POOL_MAX_BLOCKS: usize = 1 << 14; // about 67 M chunks, 537 M entries

#[derive(Clone, Copy)]
pub struct Entry {
    pub target: node::GlobalId,
    pub link: link::Id,
    pub seq: u64,
}

pub struct Chunk {
    count: AtomicU32,
    next: AtomicU64,
    removed: [AtomicU64; CHUNK],
    entries: [UnsafeCell<MaybeUninit<Entry>>; CHUNK],
}

impl Chunk {
    fn new() -> Self {
        Self {
            count: AtomicU32::new(0),
            next: AtomicU64::new(NONE),
            removed: std::array::from_fn(|_| AtomicU64::new(NONE)),
            entries: std::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit())),
        }
    }
}

pub struct SideTable {
    heads: AtomicBlocks,
    pool: Box<[AtomicPtr<Chunk>]>,
    chunks: AtomicU64,
}

unsafe impl Sync for SideTable {}
unsafe impl Send for SideTable {}

impl Default for SideTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SideTable {
    pub fn new() -> Self {
        Self {
            heads: AtomicBlocks::new(NONE),
            pool: (0..POOL_MAX_BLOCKS)
                .map(|_| AtomicPtr::new(null_mut()))
                .collect(),
            chunks: AtomicU64::new(0),
        }
    }

    #[inline]
    fn chunk(&self, i: u64) -> &Chunk {
        let block = self.pool[(i >> POOL_BLOCK_BITS) as usize].load(Ordering::Acquire);
        // Safety: chunk i was allocated (its block exists) before any head or `next` pointing at it was stored.
        unsafe { &*block.add(i as usize & (POOL_BLOCK - 1)) }
    }

    /// Writer only: a new, empty chunk.
    fn alloc(&self) -> u64 {
        let i = self.chunks.load(Ordering::Relaxed);
        let b = (i >> POOL_BLOCK_BITS) as usize;
        assert!(b < POOL_MAX_BLOCKS, "side store full");
        if self.pool[b].load(Ordering::Acquire).is_null() {
            let fresh: Box<[Chunk]> = (0..POOL_BLOCK).map(|_| Chunk::new()).collect();
            self.pool[b].store(Box::into_raw(fresh) as *mut Chunk, Ordering::Release);
        }
        self.chunks.store(i + 1, Ordering::Release);
        i
    }

    /// Writer only: adds an entry to `node`'s chain. `tail` is the writer's own record of the node's last chunk.
    pub fn push(&self, node: node::GlobalId, entry: Entry, tail: &mut Option<u64>) {
        let last = match *tail {
            Some(c) if (self.chunk(c).count.load(Ordering::Relaxed) as usize) < CHUNK => c,
            previous => {
                let c = self.alloc();
                match previous {
                    Some(p) => self.chunk(p).next.store(c, Ordering::Release),
                    None => self.heads.store(node.0, c),
                }
                *tail = Some(c);
                c
            }
        };
        let chunk = self.chunk(last);
        let k = chunk.count.load(Ordering::Relaxed) as usize;
        // Safety: slot k is beyond the published count, so no reader reads it until the count is raised.
        unsafe { (*chunk.entries[k].get()).write(entry) };
        chunk.count.store(k as u32 + 1, Ordering::Release);
    }

    /// Writer only: marks the first live entry of `node` pointing to `target` as removed by action `seq`.
    pub fn remove(
        &self,
        node: node::GlobalId,
        target: node::GlobalId,
        seq: u64,
    ) -> Option<link::Id> {
        let mut c = self.heads.load(node.0);
        while c != NONE {
            let chunk = self.chunk(c);
            let n = chunk.count.load(Ordering::Acquire) as usize;
            for k in 0..n {
                // Safety: k < count, so the entry was fully written.
                let e = unsafe { (*chunk.entries[k].get()).assume_init() };
                if e.target == target && chunk.removed[k].load(Ordering::Relaxed) == NONE {
                    chunk.removed[k].store(seq, Ordering::Release);
                    return Some(e.link);
                }
            }
            c = chunk.next.load(Ordering::Acquire);
        }
        None
    }

    /// Calls `f(target, link)` for every entry of `node` added at or before `upto` and not removed by then.
    #[inline]
    pub fn for_each(
        &self,
        node: node::GlobalId,
        upto: u64,
        mut f: impl FnMut(node::GlobalId, link::Id),
    ) {
        let mut c = self.heads.load(node.0);
        while c != NONE {
            let chunk = self.chunk(c);
            let n = chunk.count.load(Ordering::Acquire) as usize;
            for k in 0..n {
                // Safety: k < count, so the entry was fully written.
                let e = unsafe { (*chunk.entries[k].get()).assume_init() };
                let removed = chunk.removed[k].load(Ordering::Acquire);

                // NONE is u64::MAX, which is also a valid sequence number: compare it apart.
                if e.seq <= upto && (removed == NONE || removed > upto) {
                    f(e.target, e.link);
                }
            }
            c = chunk.next.load(Ordering::Acquire);
        }
    }

    #[inline]
    pub fn has_any(&self, node: node::GlobalId) -> bool {
        self.heads.load(node.0) != NONE
    }

    pub fn chunks(&self) -> u64 {
        self.chunks.load(Ordering::Acquire)
    }

    pub fn bytes(&self) -> usize {
        let blocks = self
            .pool
            .iter()
            .filter(|p| !p.load(Ordering::Relaxed).is_null())
            .count();
        self.heads.bytes() + blocks * POOL_BLOCK * size_of::<Chunk>() + self.pool.len() * 8
    }
}

impl Drop for SideTable {
    fn drop(&mut self) {
        for p in self.pool.iter() {
            let block = p.load(Ordering::Relaxed);
            if !block.is_null() {
                // Safety: every block was made from a boxed slice of POOL_BLOCK chunks.
                unsafe {
                    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                        block, POOL_BLOCK,
                    )))
                };
            }
        }
    }
}
