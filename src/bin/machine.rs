//! The machine's memory ceilings: sequential bandwidth, independent random reads, and the latency of one random
//! read, on 1 to all threads.
//!
//!     cargo run --release --bin machine

use std::hint::black_box;
use std::time::Instant;

use rayon::prelude::*;

const BYTES: usize = 1 << 30; // 1 GiB, far beyond every cache

fn pool(threads: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap()
}

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn best_of(runs: usize, mut f: impl FnMut() -> f64) -> f64 {
    (0..runs).map(|_| f()).fold(f64::INFINITY, f64::min)
}

fn main() {
    let n = BYTES / 8;
    let data: Vec<u64> = (0..n as u64).collect();
    let max_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let mut thread_counts = vec![1, 2, 4, 8];
    if max_threads > 8 {
        thread_counts.push(max_threads);
    }

    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let reads = 64 << 20; // 64 M random reads
    let index: Vec<u32> = (0..reads)
        .map(|_| (xorshift(&mut state) % n as u64) as u32)
        .collect();

    println!("# Machine memory ceilings ({} GiB array)\n", BYTES >> 30);
    println!(
        "| Threads | Sequential read GB/s | Independent random reads, M/s | = GB/s of 64-byte lines |\n|---|---|---|---|"
    );
    for &t in &thread_counts {
        let p = pool(t);
        let seq = best_of(3, || {
            p.install(|| {
                let s = Instant::now();
                let sum: u64 = data
                    .par_chunks(1 << 16)
                    .map(|c| c.iter().fold(0u64, |a, &x| a.wrapping_add(x)))
                    .sum();
                black_box(sum);
                s.elapsed().as_secs_f64()
            })
        });
        let rnd = best_of(3, || {
            p.install(|| {
                let s = Instant::now();
                let sum: u64 = index
                    .par_chunks(1 << 14)
                    .map(|c| {
                        c.iter()
                            .fold(0u64, |a, &i| a.wrapping_add(data[i as usize]))
                    })
                    .sum();
                black_box(sum);
                s.elapsed().as_secs_f64()
            })
        });
        let per_s = reads as f64 / rnd;
        println!(
            "| {t} | {:.1} | {:.0} | {:.1} |",
            BYTES as f64 / seq / 1e9,
            per_s / 1e6,
            per_s * 64.0 / 1e9
        );
    }

    // Latency: each read depends on the previous one (a random cycle through one slot per cache line).
    println!("\n| Working set | Latency of one dependent random read (ns) |\n|---|---|");
    for (label, bytes) in [
        ("8 MB (fits in cache)", 8usize << 20),
        ("1 GiB (main memory)", BYTES),
    ] {
        let lines = bytes / 64;
        let mut order: Vec<u32> = (0..lines as u32).collect();
        for i in (1..lines).rev() {
            let j = (xorshift(&mut state) % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
        let mut next = vec![0u32; lines * 16]; // 16 u32 = 64 bytes per line
        for k in 0..lines {
            next[order[k] as usize * 16] = order[(k + 1) % lines] * 16;
        }
        let steps = 20_000_000u64;
        let ns = best_of(3, || {
            let s = Instant::now();
            let mut at = 0u32;
            for _ in 0..steps {
                at = next[at as usize];
            }
            black_box(at);
            s.elapsed().as_secs_f64() * 1e9 / steps as f64
        });
        println!("| {label} | {ns:.1} |");
    }
}
