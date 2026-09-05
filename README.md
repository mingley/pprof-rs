# rpprof

`rpprof` is a CPU and memory profiler for Rust programs. It is a fork of [`tikv/pprof-rs`](https://github.com/tikv/pprof-rs), created to apply needed dependency updates and bug fixes after upstream development remained stale for nearly a year.

`rpprof` refreshes dependency trees to address known advisories, fixes signal handling and unwinding edge cases, and adds observability for dropped samples.

[![Crates.io](https://img.shields.io/crates/v/rpprof.svg)](https://crates.io/crates/rpprof)
[![Documentation](https://docs.rs/rpprof/badge.svg)](https://docs.rs/rpprof)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

## Key Changes and Improvements

- **Dependency and Security Updates**: Refreshed dependency trees to resolve security advisories flagged by `cargo-deny` (including `memmap2`, `quick-xml`, `anyhow`, `bytes`, `rand`, `adler`, and `crossbeam-epoch`).
- **Zero-Configuration Unwinding**: `framehop-unwinder` is enabled by default. Uses pre-parsed DWARF unwind tables for fast, allocation-free, async-signal-safe stack unwinding—**no `-Cforce-frame-pointers=yes` or special release build flags required**.
- **Sampled Heap / Memory Profiling**: Built-in `rpprof::alloc::AllocProfiler` (`GlobalAlloc` wrapper) with Poisson-sampled allocation tracking (512 KiB default rate), capturing both live in-use memory and cumulative allocation flamegraphs with <1% overhead.
- **CPU & Wall-Clock Profiling**: Added `ClockType` supporting both on-CPU profiling (`ITIMER_PROF` / `SIGPROF`) and real Wall-clock profiling (`ITIMER_REAL` / `SIGALRM`) for off-CPU / I/O latency bottlenecks.
- **Direct Folded Stack & Speedscope Export**: Native `report.write_folded(&mut writer)` and `report.write_speedscope(&mut writer, name)` methods for direct compatibility with [Speedscope](https://www.speedscope.app/) and Brendan Gregg scripts.
- **One-Liner Profiling Helpers**: `rpprof::profile(duration)` and `rpprof::profile_wall(duration)` for effortless profiling in integration tests, CLI tools, and HTTP endpoints.
- **Allocation-Free Iterator**: Optimized internal hash collector iteration to eliminate 4,095 heap allocations (`Box<dyn Iterator>`) and recursion on every report build.
- **Thread Name Capture & Filtering**: Fixed macOS and musl thread name detection, and added `thread_blocklist` for async-signal-safe thread filtering by name.
- **Missed Sample Observability**: Built-in atomic counter (`guard.missed_samples()` / `Profiler::missed_samples()`) tracking sampling ticks dropped due to lock contention in the signal handler. Missed sample counts are also embedded in generated pprof protobuf profiles.
- **Stray SIGPROF Protection**: Sets `SIG_IGN` when unregistering the signal handler if the previous disposition was default, preventing trailing timer signals from aborting the process.
- **macOS Context Alignment**: Safe unaligned reads of `ucontext_t` fields in signal handlers to prevent debug assertions on macOS.
- **Async-Signal-Safe Address Validation**: Uses direct `libc` system calls in `addr_validate` without creating temporary slice references or relying on higher-level file descriptor abstractions.
- **Modern Ecosystem Support**: Compatible with Rust 2024 Edition, with updated dependency ranges across `nix`, `prost`, `object`, `inferno`, and `criterion`.
- **Dual Licensed**: MIT OR Apache-2.0.

## Usage

Add `rpprof` to your `Cargo.toml` (all unwinding, flamegraph, and protobuf features work out-of-the-box):

```toml
[dependencies]
rpprof = "0.18"
```

### Quick One-Liner CPU Profiling

```rust
use std::fs::File;
use std::time::Duration;

fn main() {
    // Profile CPU for 10 seconds
    let report = rpprof::profile(Duration::from_secs(10)).unwrap();

    // Export flamegraph SVG
    let file = File::create("flamegraph.svg").unwrap();
    report.flamegraph(file).unwrap();
}
```

### Sampled Heap / Memory Profiling

Wrap your global allocator with `AllocProfiler` to enable low-overhead, Poisson-sampled heap profiling:

```rust
use std::fs::File;

#[global_allocator]
static ALLOC: rpprof::alloc::AllocProfiler = rpprof::alloc::AllocProfiler::system();

fn main() {
    // Start tracking memory allocations
    rpprof::alloc::start();

    // Your workload here...
    do_heavy_allocations();

    // Snapshot heap profile
    let heap = rpprof::alloc::heap_report().unwrap();

    // Visualize live in-use memory
    let inuse_report = heap.to_inuse_report();
    let mut flame_file = File::create("heap_inuse.svg").unwrap();
    inuse_report.flamegraph(&mut flame_file).unwrap();

    // Export cumulative allocations to Speedscope
    let alloc_report = heap.to_alloc_report();
    let mut speedscope_file = File::create("heap_alloc.speedscope.json").unwrap();
    alloc_report.write_speedscope(&mut speedscope_file, "heap").unwrap();

    rpprof::alloc::stop();
}
```

### Basic CPU Profiler Guard

```rust
use std::fs::File;

fn main() {
    let guard = rpprof::ProfilerGuardBuilder::default()
        .frequency(100)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
        .unwrap();

    // Your workload here...
    do_work();

    if let Ok(report) = guard.report().build() {
        println!("Total samples: {}", report.total_samples());
        println!("Missed samples: {}", guard.missed_samples());

        // Generate a flamegraph
        let file = File::create("flamegraph.svg").unwrap();
        report.flamegraph(file).unwrap();
    }
}

fn do_work() {
    // ...
}
```

### Wall-Clock (Off-CPU) Profiling

To profile wall-clock time (identifying threads blocked on async I/O, database queries, mutex contention, or network requests) instead of only on-CPU time:

```rust
let guard = rpprof::ProfilerGuardBuilder::default()
    .frequency(100)
    .clock_type(rpprof::ClockType::Wall)
    .build()
    .unwrap();
```

### Folded Stacks & Speedscope Export

Export raw folded stack traces (usable with FlameGraph perl scripts, differential flame graphs, or speedscope):

```rust
if let Ok(report) = guard.report().build() {
    // Write folded stacks
    let mut file = std::fs::File::create("profile.folded").unwrap();
    report.write_folded(&mut file).unwrap();

    // Write Speedscope JSON format (openable at https://www.speedscope.app/)
    let mut speedscope_file = std::fs::File::create("profile.speedscope.json").unwrap();
    report.write_speedscope(&mut speedscope_file, "my_service").unwrap();
}
```

## Features

| Feature | Description |
|---|---|
| `cpp` *(default)* | Enables C++ symbol demangling via `symbolic-demangle`. |
| `flamegraph` | Enables SVG flamegraph generation via `inferno`. |
| `prost-codec` | Enables Google pprof protobuf profile format generation via `prost`. |
| `protobuf-codec` | Enables Google pprof protobuf profile format generation via `protobuf` crate. |
| `framehop-unwinder` | Stack unwinding using `framehop` (fast, allocation-free unwinding). |
| `frame-pointer` | Stack unwinding using frame pointers (requires building std with frame pointers). |
| `criterion` | Custom profiler integration for `criterion` benchmarks. |
| `perfmaps` | Support for `/tmp/perf-<pid>.map` symbol resolution. |

## Generating Flamegraphs

With the `flamegraph` feature enabled:

```rust
if let Ok(report) = guard.report().build() {
    let file = std::fs::File::create("flamegraph.svg").unwrap();
    report.flamegraph(file).unwrap();
}
```

Custom options (e.g. image width):

```rust
if let Ok(report) = guard.report().build() {
    let file = std::fs::File::create("flamegraph.svg").unwrap();
    let mut options = rpprof::flamegraph::Options::default();
    options.image_width = Some(2500);
    report.flamegraph_with_options(file, &mut options).unwrap();
}
```

## Google `pprof` Protobuf Output

With `prost-codec` enabled, `rpprof` outputs standard [`profile.proto`](https://github.com/google/pprof/blob/master/proto/profile.proto) protobuf profiles:

```rust
use std::fs::File;
use std::io::Write;
use rpprof::protos::Message;

if let Ok(report) = guard.report().build() {
    let mut file = File::create("profile.pb").unwrap();
    let profile = report.pprof().unwrap();

    let mut content = Vec::new();
    profile.encode(&mut content).unwrap();
    file.write_all(&content).unwrap();
}
```

You can then visualize or analyze the profile with `go tool pprof`:

```shell
pprof -http=:8080 profile.pb
```

## Frame Post-Processor

Before report generation, a `frames_post_processor` can normalize or rewrite frame and thread names:

```rust
if let Ok(report) = guard
    .report()
    .frames_post_processor(|frames| {
        if frames.thread_name.starts_with("worker-") {
            frames.thread_name = "worker".to_string();
        }
    })
    .build()
{
    let file = std::fs::File::create("flamegraph.svg").unwrap();
    report.flamegraph(file).unwrap();
}
```

## Criterion Integration

With the `criterion` and `flamegraph` features enabled:

```rust
use criterion::{criterion_group, criterion_main, Criterion};
use rpprof::criterion::{Output, PProfProfiler};

fn bench(c: &mut Criterion) {
    c.bench_function("my_benchmark", |b| b.iter(|| {
        // benchmark code...
    }));
}

criterion_group! {
    name = benches;
    config = Criterion::default().with_profiler(PProfProfiler::new(100, Output::Flamegraph(None)));
    targets = bench
}
criterion_main!(benches);
```

## Minimum Supported Rust Version (MSRV)

Rust **1.88.0** (Rust 2024 Edition) or higher. Target/development toolchain is Rust **1.98**.

## License

This project is licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.
