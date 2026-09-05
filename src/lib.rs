// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

//! `rpprof` is a modern CPU, wall-clock, and heap profiler for Rust programs.
//!
//! It provides low-overhead, async-signal-safe profiling designed for both
//! production services and local benchmarking.
//!
//! # Features & Highlights
//!
//! - **Zero-Configuration Unwinding**: Fast, allocation-free DWARF stack unwinding via
//!   `framehop` enabled by default—**no `-Cforce-frame-pointers=yes` or special compiler flags required**.
//! - **CPU & Wall-Clock Profiling**: Choose between CPU time ([`ClockType::Cpu`]) and real
//!   wall-clock time ([`ClockType::Wall`]) to capture both compute bottlenecks and off-CPU latency
//!   (async I/O, database queries, lock contention).
//! - **Sampled Heap Profiling**: [`alloc::AllocProfiler`] wraps your global allocator to track live
//!   in-use memory and cumulative allocation churn with `<1%` overhead.
//! - **Multiple Output Formats**: Export directly to SVG flamegraphs ([`Report::flamegraph`]),
//!   [Speedscope](https://www.speedscope.app/) JSON ([`Report::write_speedscope`]), raw folded stacks
//!   ([`Report::write_folded`]), and Google pprof protobuf ([`Report::pprof`]).
//!
//! # Quick Start: One-Liner CPU Profiling
//!
//! ```rust,no_run
//! use std::time::Duration;
//! use std::fs::File;
//!
//! // Profile CPU for 10 seconds
//! let report = rpprof::profile(Duration::from_secs(10)).unwrap();
//!
//! // Save an interactive SVG flamegraph
//! let file = File::create("flamegraph.svg").unwrap();
//! report.flamegraph(file).unwrap();
//! ```
//!
//! # Guard-Based Profiling
//!
//! For scoped profiling over specific operations or custom configurations:
//!
//! ```rust
//! let guard = rpprof::ProfilerGuardBuilder::default()
//!     .frequency(100)
//!     .blocklist(&["libc", "libgcc", "pthread", "vdso"])
//!     .build()
//!     .unwrap();
//!
//! // Run workload...
//!
//! if let Ok(report) = guard.report().build() {
//!     println!("Total samples: {}", report.total_samples());
//!     println!("Missed ticks: {}", guard.missed_samples());
//!
//!     // Write folded stacks
//!     let mut output = Vec::new();
//!     report.write_folded(&mut output).unwrap();
//! };
//! ```
//!
//! # Wall-Clock (Off-CPU) Profiling
//!
//! To capture time spent waiting on async tasks, I/O, locks, or network calls:
//!
//! ```rust
//! let guard = rpprof::ProfilerGuardBuilder::default()
//!     .frequency(100)
//!     .clock_type(rpprof::ClockType::Wall)
//!     .build()
//!     .unwrap();
//! ```
//!
//! # Sampled Heap / Memory Profiling
//!
//! Configure [`alloc::AllocProfiler`] as your `#[global_allocator]`:
//!
//! ```rust,no_run
//! use std::fs::File;
//!
//! #[global_allocator]
//! static ALLOC: rpprof::alloc::AllocProfiler = rpprof::alloc::AllocProfiler::system();
//!
//! fn main() {
//!     rpprof::alloc::start();
//!
//!     // Run allocation-heavy workload...
//!
//!     let heap = rpprof::alloc::heap_report().unwrap();
//!     let inuse_report = heap.to_inuse_report();
//!     let mut flame = File::create("heap_inuse.svg").unwrap();
//!     inuse_report.flamegraph(&mut flame).unwrap();
//!
//!     rpprof::alloc::stop();
//! }
//! ```

/// Define the MAX supported stack depth. TODO: make this variable mutable.
#[cfg(feature = "large-depth")]
pub const MAX_DEPTH: usize = 1024;

#[cfg(all(feature = "huge-depth", not(feature = "large-depth")))]
pub const MAX_DEPTH: usize = 512;

#[cfg(not(any(feature = "large-depth", feature = "huge-depth")))]
pub const MAX_DEPTH: usize = 128;

/// Define the MAX supported thread name length. TODO: make this variable mutable.
pub const MAX_THREAD_NAME: usize = 16;

mod addr_validate;

mod backtrace;
mod collector;
mod error;
mod frames;
#[cfg(feature = "perfmaps")]
mod perfmap;
mod profiler;
mod report;
mod timer;

pub use self::addr_validate::validate;
pub use self::collector::{Collector, HashCounter};
pub use self::error::{Error, Result};
pub use self::frames::{Frames, Symbol};
pub use self::profiler::{ProfilerGuard, ProfilerGuardBuilder};
pub use self::report::{Report, ReportBuilder, UnresolvedReport};
pub use self::timer::ClockType;

#[cfg(feature = "flamegraph")]
pub use inferno::flamegraph;

#[allow(clippy::all)]
#[cfg(all(feature = "prost-codec", not(feature = "protobuf-codec")))]
pub mod protos {
    pub use prost::Message;

    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/proto/perftools.profiles.rs"
    ));
}

#[cfg(feature = "protobuf-codec")]
pub mod protos {
    pub use protobuf::Message;

    include!(concat!(env!("OUT_DIR"), "/mod.rs"));

    pub use self::profile::*;
}

#[cfg(feature = "criterion")]
pub mod criterion;

pub mod alloc;

/// Collects a CPU profile at 99Hz for the specified duration and returns the resulting report.
///
/// # Example
///
/// ```rust,no_run
/// use std::time::Duration;
///
/// let report = rpprof::profile(Duration::from_secs(5)).unwrap();
/// let mut file = std::fs::File::create("profile.folded").unwrap();
/// report.write_folded(&mut file).unwrap();
/// ```
pub fn profile(duration: std::time::Duration) -> Result<Report> {
    profile_with_frequency(99, duration)
}

/// Collects a CPU profile with a custom frequency in Hz for the specified duration.
pub fn profile_with_frequency(frequency: i32, duration: std::time::Duration) -> Result<Report> {
    let guard = ProfilerGuardBuilder::default()
        .frequency(frequency)
        .build()?;
    std::thread::sleep(duration);
    guard.report().build()
}

/// Collects a real Wall-clock profile at 99Hz for the specified duration.
///
/// Useful for identifying time spent blocked on I/O, database queries, mutexes, or network calls.
pub fn profile_wall(duration: std::time::Duration) -> Result<Report> {
    let guard = ProfilerGuardBuilder::default()
        .frequency(99)
        .clock_type(ClockType::Wall)
        .build()?;
    std::thread::sleep(duration);
    guard.report().build()
}

#[cfg(test)]
mod top_level_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_top_level_profile_helpers() {
        let report = profile(Duration::from_millis(50)).unwrap();
        assert_eq!(report.clock_type(), ClockType::Cpu);

        let wall_report = profile_wall(Duration::from_millis(50)).unwrap();
        assert_eq!(wall_report.clock_type(), ClockType::Wall);
    }
}
