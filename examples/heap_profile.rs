// Copyright 2026 rpprof Authors. Licensed under Apache-2.0 OR MIT.

use std::fs::File;

#[global_allocator]
static ALLOC: rpprof::alloc::AllocProfiler = rpprof::alloc::AllocProfiler::system();

fn main() {
    println!("Starting heap profiling...");
    rpprof::alloc::start();

    // Allocate some memory with distinguishable stack traces
    let _data = allocate_buffers();

    let heap = rpprof::alloc::heap_report().unwrap();
    println!("Recorded {} heap callsites", heap.records.len());
    println!("Total in-use bytes: {}", heap.total_inuse_bytes());
    println!("Total allocated bytes: {}", heap.total_alloc_bytes());

    // Export in-use flamegraph
    let inuse = heap.to_inuse_report();
    let file = File::create("heap_inuse.svg").unwrap();
    inuse.flamegraph(file).unwrap();
    println!("Generated heap_inuse.svg");

    // Export cumulative allocation profile to Speedscope
    let alloc = heap.to_alloc_report();
    let file = File::create("heap_alloc.speedscope.json").unwrap();
    alloc.write_speedscope(file, "heap_allocations").unwrap();
    println!("Generated heap_alloc.speedscope.json");

    rpprof::alloc::stop();
}

#[inline(never)]
fn allocate_buffers() -> Vec<Vec<u8>> {
    (0..100)
        .map(|i| vec![i as u8; 50 * 1024]) // 5 MB total allocations
        .collect()
}
