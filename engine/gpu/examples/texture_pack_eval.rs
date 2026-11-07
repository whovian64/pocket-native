//! ROM-free ETC1 upload kernel comparison against the previous packer.
//! Run: cargo run --release -p zakuro-gpu --example texture_pack_eval
//! Timings are host microbenchmarks, not gameplay or target-device results.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;
use zakuro_gpu::texture::etc1_blocks;

struct CountingAllocator;
static TRACK: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static REALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static REQUESTED: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// This example runs kernels synchronously on one thread. The byte counters
// describe allocator requests, not RSS or the allocator's internal overhead.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if TRACK.load(Relaxed) && !pointer.is_null() {
            ALLOCATIONS.fetch_add(1, Relaxed);
            REQUESTED.fetch_add(layout.size(), Relaxed);
            let live = LIVE.fetch_add(layout.size(), Relaxed) + layout.size();
            PEAK.fetch_max(live, Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if TRACK.load(Relaxed) {
            LIVE.fetch_sub(layout.size(), Relaxed);
        }
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, layout, size) };
        if TRACK.load(Relaxed) && !replacement.is_null() {
            REALLOCATIONS.fetch_add(1, Relaxed);
            REQUESTED.fetch_add(size, Relaxed);
            // realloc replaces the old allocation. Any transient overlap
            // inside the allocator is unknown and is excluded here.
            LIVE.fetch_sub(layout.size(), Relaxed);
            let live = LIVE.fetch_add(size, Relaxed) + size;
            PEAK.fetch_max(live, Relaxed);
        }
        replacement
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;
type Kernel = fn(&[u8], u32, u32, bool) -> Vec<u8>;

/// The old two-plane algorithm, retained only as a comparison reference.
fn legacy_blocks(data: &[u8], width: u32, height: u32, alpha: bool) -> Vec<u8> {
    let size = if alpha { 16 } else { 8 };
    let mut out = Vec::with_capacity((width * height / 2) as usize);
    let mut alphas = vec![0u8; if alpha { (width * height) as usize } else { 0 }];
    for by in 0..height / 4 {
        for bx in 0..width / 4 {
            let tile = (by / 2) * (width / 8).max(1) + bx / 2;
            let at = ((tile * 4 + bx % 2 + 2 * (by % 2)) * size) as usize;
            let mut block = [0u8; 16];
            if let Some(source) = data.get(at..at + size as usize) {
                block[..size as usize].copy_from_slice(source);
            }
            let (texels, color) = block[..size as usize].split_at_mut(size as usize - 8);
            color.reverse();
            out.extend_from_slice(color);
            if alpha {
                let word = u64::from_le_bytes(texels.try_into().unwrap());
                for y in 0..4 {
                    for x in 0..4 {
                        let nibble = ((word >> ((x * 4 + y) * 4)) & 0xF) as u8;
                        alphas[((by * 4 + y) * width + bx * 4 + x) as usize] = nibble * 17;
                    }
                }
            }
        }
    }
    out.extend(alphas);
    out
}

fn allocations(kernel: Kernel, data: &[u8], width: u32, height: u32, alpha: bool) -> [usize; 4] {
    for counter in [&ALLOCATIONS, &REALLOCATIONS, &REQUESTED, &LIVE, &PEAK] {
        counter.store(0, Relaxed);
    }
    TRACK.store(true, Relaxed);
    let output = black_box(kernel(black_box(data), width, height, alpha));
    let counts = [
        ALLOCATIONS.load(Relaxed),
        REALLOCATIONS.load(Relaxed),
        REQUESTED.load(Relaxed),
        PEAK.load(Relaxed),
    ];
    drop(output);
    TRACK.store(false, Relaxed);
    assert_eq!(LIVE.load(Relaxed), 0);
    counts
}

fn nanos(
    kernel: Kernel,
    data: &[u8],
    width: u32,
    height: u32,
    alpha: bool,
    iterations: usize,
) -> f64 {
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(kernel(
            black_box(data),
            black_box(width),
            black_box(height),
            black_box(alpha),
        ));
    }
    start.elapsed().as_secs_f64() * 1e9 / iterations as f64
}

fn main() {
    let mut comparisons = 0;
    for (width, height) in [
        (0, 0),
        (0, 8),
        (8, 0),
        (1, 1),
        (3, 7),
        (4, 4),
        (5, 9),
        (8, 8),
        (12, 8),
        (16, 16),
        (64, 16),
        (16, 64),
        (128, 128),
    ] {
        let data: Vec<u8> = (0..width * height * 2 + 17)
            .map(|i| ((i * 37 + 11) ^ (i >> 3)) as u8)
            .collect();
        for alpha in [false, true] {
            for length in [0, 1, 7, 8, 15, 16, 17, data.len() / 2, data.len()] {
                let input = &data[..length.min(data.len())];
                assert_eq!(
                    legacy_blocks(input, width, height, alpha),
                    etc1_blocks(input, width, height, alpha),
                    "{width}x{height}, alpha {alpha}, length {length}"
                );
                comparisons += 1;
            }
        }
    }
    eprintln!("{comparisons} byte-for-byte comparisons passed. Timings are host kernel medians; resource counters are requested heap bytes, not RSS.");
    println!("width,height,alpha,before_ns,after_ns,speedup,before_allocs,before_reallocs,before_requested_bytes,before_peak_live_bytes,after_allocs,after_reallocs,after_requested_bytes,after_peak_live_bytes");
    for (width, height) in [(8, 8), (64, 64), (256, 256), (512, 512), (1024, 1024)] {
        let data: Vec<u8> = (0..width * height * 2)
            .map(|i| ((i * 37 + 11) ^ (i >> 3)) as u8)
            .collect();
        let iterations = (2_097_152 / (width * height) as usize).max(8);
        for alpha in [false, true] {
            let mut before = Vec::new();
            let mut after = Vec::new();
            for sample in 0..21 {
                // Alternate order to reduce temperature/frequency/order bias.
                if sample % 2 == 0 {
                    before.push(nanos(
                        legacy_blocks,
                        &data,
                        width,
                        height,
                        alpha,
                        iterations,
                    ));
                    after.push(nanos(etc1_blocks, &data, width, height, alpha, iterations));
                } else {
                    after.push(nanos(etc1_blocks, &data, width, height, alpha, iterations));
                    before.push(nanos(
                        legacy_blocks,
                        &data,
                        width,
                        height,
                        alpha,
                        iterations,
                    ));
                }
            }
            before.sort_by(f64::total_cmp);
            after.sort_by(f64::total_cmp);
            let [ba, br, bb, bp] = allocations(legacy_blocks, &data, width, height, alpha);
            let [aa, ar, ab, ap] = allocations(etc1_blocks, &data, width, height, alpha);
            println!("{width},{height},{alpha},{:.1},{:.1},{:.3},{ba},{br},{bb},{bp},{aa},{ar},{ab},{ap}", before[10], after[10], before[10] / after[10]);
        }
    }
}
