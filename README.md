# rpprof

`rpprof` is a modern, actively maintained CPU and memory profiler for Rust programs. It is a fork of [`tikv/pprof-rs`](https://github.com/tikv/pprof-rs) created to provide timely updates, bug fixes, and active maintenance because the upstream repository is largely inactive/unresponsive and needs some love.

`rpprof` brings dependency trees up to date with zero `cargo-deny` security advisories, fixes long-standing signal handling and unwinding bugs, and adds first-class observability for dropped samples.

[![Crates.io](https://img.shields.io/crates/v/rpprof.svg)](https://crates.io/crates/rpprof)
[![Documentation](https://docs.rs/rpprof/badge.svg)](https://docs.rs/rpprof)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

## Key Improvements over legacy `pprof-rs`

- **Zero Cargo-Deny Vulnerabilities**: Fully updated dependency tree addressing known advisories (including `memmap2`, `quick-xml`, `anyhow`, `bytes`, `rand`, `adler`, and `crossbeam-epoch`).
- **Missed Sample Observability**: Built-in atomic counter (`guard.missed_samples()` / `Profiler::missed_samples()`) tracking sampling ticks dropped due to lock contention in the signal handler. Missed samples are also automatically embedded as comments in generated pprof protobuf profiles.
- **Stray SIGPROF Protection**: Fixed signal handler unregistration so trailing SIGPROF signals from kernel timers do not terminate the process with `SIGPROF` / `Profiling timer expired`.
- **macOS Unaligned Context Safety**: Safe unaligned reads of `ucontext_t` fields in signal handlers preventing `SIGABRT` crashes on macOS debug builds.
- **Async-Signal-Safe Address Validation**: Refactored `addr_validate` to use direct `libc` system calls without creating invalid Rust slice references or depending on `nix`'s file descriptor API.
- **Modern Ecosystem**: Broad, unified dependency ranges (`nix >= 0.27, < 0.31`, `prost >= 0.12, < 0.15`, `object >= 0.32, < 0.38`, `inferno 0.12`, `criterion 0.8`).
- **Dual Licensed**: MIT OR Apache-2.0.
- **Automated CI/CD**: Multi-platform GitHub Actions testing, clippy, formatting, cargo-deny enforcement, and one-click releases to crates.io.

## Usage

Add `rpprof` to your `Cargo.toml`:

```toml
[dependencies]
rpprof = { version = "0.16", features = ["flamegraph", "prost-codec"] }
```

### Basic Profiling

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

Rust **1.88.0** (Rust 2024 Edition) or higher.

## License

This project is licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.
