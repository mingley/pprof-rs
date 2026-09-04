// Copyright 2026 rpprof Authors. Licensed under Apache-2.0 OR MIT.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::SystemTime;

use crate::Result;
use crate::frames::{Frames, Symbol};
use crate::report::Report;
use crate::timer::{ClockType, ReportTiming};

pub const DEFAULT_SAMPLE_RATE: usize = 512 * 1024; // 512 KiB default sample rate

static IS_PROFILING: AtomicBool = AtomicBool::new(false);
static SAMPLE_RATE: AtomicUsize = AtomicUsize::new(DEFAULT_SAMPLE_RATE);

const NUM_SHARDS: usize = 32;

struct Shard {
    live: HashMap<usize, (u64, usize)>,
}

impl Shard {
    fn new() -> Self {
        Self {
            live: HashMap::new(),
        }
    }
}

struct HeapState {
    shards: Vec<Mutex<Shard>>,
    callsite_map: RwLock<HashMap<Vec<usize>, u64>>,
    callsites: RwLock<HashMap<u64, CallsiteData>>,
    next_callsite_id: AtomicU64,
}

struct CallsiteData {
    ips: Vec<usize>,
    alloc_objects: AtomicU64,
    alloc_bytes: AtomicU64,
    inuse_objects: AtomicI64,
    inuse_bytes: AtomicI64,
}

impl HeapState {
    fn new() -> Self {
        let mut shards = Vec::with_capacity(NUM_SHARDS);
        for _ in 0..NUM_SHARDS {
            shards.push(Mutex::new(Shard::new()));
        }

        Self {
            shards,
            callsite_map: RwLock::new(HashMap::new()),
            callsites: RwLock::new(HashMap::new()),
            next_callsite_id: AtomicU64::new(1),
        }
    }

    fn reset(&self) {
        for shard in &self.shards {
            if let Ok(mut s) = shard.lock() {
                s.live.clear();
            }
        }
        if let Ok(mut map) = self.callsite_map.write() {
            map.clear();
        }
        if let Ok(mut callsites) = self.callsites.write() {
            callsites.clear();
        }
        self.next_callsite_id.store(1, Ordering::SeqCst);
    }
}

static HEAP_STATE: OnceLock<HeapState> = OnceLock::new();

fn state() -> &'static HeapState {
    HEAP_STATE.get_or_init(HeapState::new)
}

thread_local! {
    static IN_ALLOC: Cell<bool> = const { Cell::new(false) };
    static BYTES_LEFT: Cell<isize> = const { Cell::new(0) };
}

/// A sampling heap profiler wrapping any `GlobalAlloc` implementation.
///
/// Can be configured as `#[global_allocator]` to enable low-overhead,
/// Poisson-sampled heap and allocation profiling.
///
/// When profiling is not enabled, runtime overhead is a single atomic check (<1ns).
///
/// # Example
///
/// ```rust,no_run
/// #[global_allocator]
/// static ALLOC: rpprof::alloc::AllocProfiler = rpprof::alloc::AllocProfiler::system();
///
/// fn main() {
///     rpprof::alloc::start();
///
///     // Your workload here...
///
///     let heap_report = rpprof::alloc::heap_report().unwrap();
///     let inuse_report = heap_report.to_inuse_report();
///     let mut file = std::fs::File::create("heap_inuse.folded").unwrap();
///     inuse_report.write_folded(&mut file).unwrap();
///
///     rpprof::alloc::stop();
/// }
/// ```
pub struct AllocProfiler<A = System> {
    inner: A,
}

impl AllocProfiler<System> {
    /// Creates an `AllocProfiler` wrapping the system default allocator.
    pub const fn system() -> Self {
        Self { inner: System }
    }
}

impl<A> AllocProfiler<A> {
    /// Creates an `AllocProfiler` wrapping the provided `GlobalAlloc` allocator.
    pub const fn new(allocator: A) -> Self {
        Self { inner: allocator }
    }
}

impl<A> Default for AllocProfiler<A>
where
    A: Default,
{
    fn default() -> Self {
        Self {
            inner: A::default(),
        }
    }
}

unsafe impl<A: GlobalAlloc> GlobalAlloc for AllocProfiler<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.inner.alloc(layout) };
        if ptr.is_null() || !IS_PROFILING.load(Ordering::Relaxed) {
            return ptr;
        }

        let _ = IN_ALLOC.try_with(|in_alloc| {
            if in_alloc.get() {
                return;
            }

            let size = layout.size();
            let _ = BYTES_LEFT.try_with(|bytes_left| {
                let remaining = bytes_left.get() - size as isize;
                if remaining <= 0 {
                    in_alloc.set(true);
                    let rate = SAMPLE_RATE.load(Ordering::Relaxed);
                    bytes_left.set(rate as isize);

                    record_sample(ptr as usize, size);
                    in_alloc.set(false);
                } else {
                    bytes_left.set(remaining);
                }
            });
        });

        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if IS_PROFILING.load(Ordering::Relaxed) {
            let _ = IN_ALLOC.try_with(|in_alloc| {
                if !in_alloc.get() {
                    in_alloc.set(true);
                    record_dealloc(ptr as usize, layout.size());
                    in_alloc.set(false);
                }
            });
        }

        unsafe { self.inner.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { self.inner.realloc(ptr, layout, new_size) };
        if new_ptr.is_null() || !IS_PROFILING.load(Ordering::Relaxed) {
            return new_ptr;
        }

        let _ = IN_ALLOC.try_with(|in_alloc| {
            if !in_alloc.get() {
                in_alloc.set(true);
                record_dealloc(ptr as usize, layout.size());
                record_sample(new_ptr as usize, new_size);
                in_alloc.set(false);
            }
        });

        new_ptr
    }
}

fn record_sample(ptr: usize, size: usize) {
    let mut ips = Vec::with_capacity(32);
    backtrace::trace(|frame| {
        ips.push(frame.ip() as usize);
        ips.len() < crate::MAX_DEPTH
    });

    let s = state();
    let callsite_id = {
        let read = s.callsite_map.read().unwrap();
        if let Some(&id) = read.get(&ips) {
            id
        } else {
            drop(read);
            let mut write = s.callsite_map.write().unwrap();
            *write.entry(ips.clone()).or_insert_with(|| {
                let id = s.next_callsite_id.fetch_add(1, Ordering::SeqCst);
                let mut data_write = s.callsites.write().unwrap();
                data_write.insert(
                    id,
                    CallsiteData {
                        ips,
                        alloc_objects: AtomicU64::new(0),
                        alloc_bytes: AtomicU64::new(0),
                        inuse_objects: AtomicI64::new(0),
                        inuse_bytes: AtomicI64::new(0),
                    },
                );
                id
            })
        }
    };

    if let Ok(data_read) = s.callsites.read()
        && let Some(data) = data_read.get(&callsite_id)
    {
        data.alloc_objects.fetch_add(1, Ordering::Relaxed);
        data.alloc_bytes.fetch_add(size as u64, Ordering::Relaxed);
        data.inuse_objects.fetch_add(1, Ordering::Relaxed);
        data.inuse_bytes.fetch_add(size as i64, Ordering::Relaxed);
    }

    let shard_idx = (ptr >> 4) % NUM_SHARDS;
    if let Ok(mut shard) = s.shards[shard_idx].lock() {
        shard.live.insert(ptr, (callsite_id, size));
    }
}

fn record_dealloc(ptr: usize, _size: usize) {
    let s = state();
    let shard_idx = (ptr >> 4) % NUM_SHARDS;
    let removed = if let Ok(mut shard) = s.shards[shard_idx].lock() {
        shard.live.remove(&ptr)
    } else {
        None
    };

    if let Some((callsite_id, alloc_size)) = removed
        && let Ok(data_read) = s.callsites.read()
        && let Some(data) = data_read.get(&callsite_id)
    {
        data.inuse_objects.fetch_sub(1, Ordering::Relaxed);
        data.inuse_bytes
            .fetch_sub(alloc_size as i64, Ordering::Relaxed);
    }
}

/// Starts heap allocation profiling.
pub fn start() {
    IS_PROFILING.store(true, Ordering::SeqCst);
}

/// Stops heap allocation profiling.
pub fn stop() {
    IS_PROFILING.store(false, Ordering::SeqCst);
}

/// Returns true if heap profiling is currently active.
pub fn is_active() -> bool {
    IS_PROFILING.load(Ordering::Relaxed)
}

/// Resets all recorded heap profile data.
pub fn reset() {
    state().reset();
}

/// Sets the average sampling interval in bytes (default: 512 KiB).
pub fn set_sample_rate(rate_bytes: usize) {
    let rate = if rate_bytes == 0 { 1 } else { rate_bytes };
    SAMPLE_RATE.store(rate, Ordering::SeqCst);
}

/// Returns the current sampling interval in bytes.
pub fn sample_rate() -> usize {
    SAMPLE_RATE.load(Ordering::Relaxed)
}

/// A single callsite entry in a heap profile report.
#[derive(Clone, Debug)]
pub struct HeapRecord {
    pub frames: Frames,
    pub alloc_objects: u64,
    pub alloc_bytes: u64,
    pub inuse_objects: i64,
    pub inuse_bytes: i64,
}

/// A collected heap profile containing allocation and in-use memory records.
#[derive(Clone, Debug)]
pub struct HeapReport {
    pub records: Vec<HeapRecord>,
    pub sample_rate: usize,
    pub timestamp: SystemTime,
}

impl HeapReport {
    /// Total live bytes across all recorded stack traces.
    pub fn total_inuse_bytes(&self) -> i64 {
        self.records.iter().map(|r| r.inuse_bytes.max(0)).sum()
    }

    /// Total cumulative allocated bytes across all recorded stack traces.
    pub fn total_alloc_bytes(&self) -> u64 {
        self.records.iter().map(|r| r.alloc_bytes).sum()
    }

    /// Converts in-use (live) heap allocations into a standard `Report` for flamegraph,
    /// folded stack, or speedscope visualization in bytes.
    pub fn to_inuse_report(&self) -> Report {
        let mut data = HashMap::new();
        for rec in &self.records {
            if rec.inuse_bytes > 0 {
                data.insert(rec.frames.clone(), rec.inuse_bytes as isize);
            }
        }
        Report {
            data,
            timing: ReportTiming {
                frequency: self.sample_rate as i32,
                start_time: self.timestamp,
                duration: std::time::Duration::from_secs(0),
                clock_type: ClockType::Cpu,
            },
            missed_samples: 0,
        }
    }

    /// Converts cumulative allocated heap memory into a standard `Report` for flamegraph,
    /// folded stack, or speedscope visualization in bytes.
    pub fn to_alloc_report(&self) -> Report {
        let mut data = HashMap::new();
        for rec in &self.records {
            if rec.alloc_bytes > 0 {
                data.insert(rec.frames.clone(), rec.alloc_bytes as isize);
            }
        }
        Report {
            data,
            timing: ReportTiming {
                frequency: self.sample_rate as i32,
                start_time: self.timestamp,
                duration: std::time::Duration::from_secs(0),
                clock_type: ClockType::Cpu,
            },
            missed_samples: 0,
        }
    }
}

#[cfg(feature = "_protobuf")]
#[allow(clippy::useless_conversion)]
#[allow(clippy::needless_update)]
mod protobuf {
    use super::*;
    use crate::protos;
    use std::collections::HashSet;

    const ALLOC_OBJECTS: &str = "alloc_objects";
    const ALLOC_SPACE: &str = "alloc_space";
    const INUSE_OBJECTS: &str = "inuse_objects";
    const INUSE_SPACE: &str = "inuse_space";
    const COUNT: &str = "count";
    const BYTES: &str = "bytes";
    const SPACE: &str = "space";

    impl HeapReport {
        /// Generates a standard Google pprof protobuf profile with heap sample types
        /// (alloc_objects, alloc_space, inuse_objects, inuse_space).
        pub fn pprof(&self) -> Result<protos::Profile> {
            let mut dedup_str = HashSet::new();
            for rec in &self.records {
                dedup_str.insert(rec.frames.thread_name_or_id());
                for frame in &rec.frames.frames {
                    for symbol in frame {
                        dedup_str.insert(symbol.name());
                        dedup_str.insert(symbol.sys_name().into_owned());
                        dedup_str.insert(symbol.filename().into_owned());
                    }
                }
            }
            dedup_str.insert(ALLOC_OBJECTS.into());
            dedup_str.insert(ALLOC_SPACE.into());
            dedup_str.insert(INUSE_OBJECTS.into());
            dedup_str.insert(INUSE_SPACE.into());
            dedup_str.insert(COUNT.into());
            dedup_str.insert(BYTES.into());
            dedup_str.insert(SPACE.into());

            let mut str_tbl = vec!["".to_owned()];
            str_tbl.extend(dedup_str);

            let mut strings = HashMap::new();
            for (index, name) in str_tbl.iter().enumerate() {
                strings.insert(name.as_str(), index);
            }

            let mut samples = vec![];
            let mut loc_tbl = vec![];
            let mut fn_tbl = vec![];
            let mut functions = HashMap::new();

            for rec in &self.records {
                let mut locs = vec![];
                for frame in &rec.frames.frames {
                    for symbol in frame {
                        let name = symbol.name();
                        if let Some(loc_idx) = functions.get(&name) {
                            locs.push(*loc_idx);
                            continue;
                        }
                        let sys_name = symbol.sys_name();
                        let filename = symbol.filename();
                        let lineno = symbol.lineno();
                        let function_id = fn_tbl.len() as u64 + 1;
                        let function = protos::Function {
                            id: function_id,
                            name: *strings.get(name.as_str()).unwrap() as i64,
                            system_name: *strings.get(sys_name.as_ref()).unwrap() as i64,
                            filename: *strings.get(filename.as_ref()).unwrap() as i64,
                            ..protos::Function::default()
                        };
                        functions.insert(name, function_id);
                        let line = protos::Line {
                            function_id,
                            line: lineno as i64,
                            ..protos::Line::default()
                        };
                        let loc = protos::Location {
                            id: function_id,
                            line: vec![line].into(),
                            ..protos::Location::default()
                        };
                        fn_tbl.push(function);
                        loc_tbl.push(loc);
                        locs.push(function_id);
                    }
                }

                let sample = protos::Sample {
                    location_id: locs,
                    value: vec![
                        rec.alloc_objects as i64,
                        rec.alloc_bytes as i64,
                        rec.inuse_objects.max(0),
                        rec.inuse_bytes.max(0),
                    ],
                    ..Default::default()
                };
                samples.push(sample);
            }

            let sample_types = vec![
                protos::ValueType {
                    ty: *strings.get(ALLOC_OBJECTS).unwrap() as i64,
                    unit: *strings.get(COUNT).unwrap() as i64,
                    ..Default::default()
                },
                protos::ValueType {
                    ty: *strings.get(ALLOC_SPACE).unwrap() as i64,
                    unit: *strings.get(BYTES).unwrap() as i64,
                    ..Default::default()
                },
                protos::ValueType {
                    ty: *strings.get(INUSE_OBJECTS).unwrap() as i64,
                    unit: *strings.get(COUNT).unwrap() as i64,
                    ..Default::default()
                },
                protos::ValueType {
                    ty: *strings.get(INUSE_SPACE).unwrap() as i64,
                    unit: *strings.get(BYTES).unwrap() as i64,
                    ..Default::default()
                },
            ];

            let period_type = protos::ValueType {
                ty: *strings.get(SPACE).unwrap() as i64,
                unit: *strings.get(BYTES).unwrap() as i64,
                ..Default::default()
            };

            let profile = protos::Profile {
                sample_type: sample_types.into(),
                sample: samples.into(),
                string_table: str_tbl.into(),
                function: fn_tbl.into(),
                location: loc_tbl.into(),
                time_nanos: self
                    .timestamp
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as i64,
                period_type: Some(period_type).into(),
                period: self.sample_rate as i64,
                ..protos::Profile::default()
            };

            Ok(profile)
        }
    }
}

/// Takes a snapshot of recorded heap allocations and resolves symbol information.
pub fn heap_report() -> Result<HeapReport> {
    let s = state();
    let data_read = s.callsites.read().unwrap();
    let mut records = Vec::with_capacity(data_read.len());

    for data in data_read.values() {
        let mut frames_vec = Vec::new();

        for &ip in &data.ips {
            let mut symbols = Vec::new();
            backtrace::resolve(ip as *mut _, |sym| {
                symbols.push(Symbol {
                    name: sym.name().map(|n| n.as_bytes().to_vec()),
                    addr: sym.addr(),
                    lineno: sym.lineno(),
                    filename: sym.filename().map(|f| f.to_path_buf()),
                });
            });

            if !symbols.is_empty() {
                frames_vec.push(symbols);
            }
        }

        let frames = Frames {
            frames: frames_vec,
            thread_name: String::new(),
            thread_id: 0,
            sample_timestamp: SystemTime::now(),
        };

        records.push(HeapRecord {
            frames,
            alloc_objects: data.alloc_objects.load(Ordering::Relaxed),
            alloc_bytes: data.alloc_bytes.load(Ordering::Relaxed),
            inuse_objects: data.inuse_objects.load(Ordering::Relaxed),
            inuse_bytes: data.inuse_bytes.load(Ordering::Relaxed),
        });
    }

    Ok(HeapReport {
        records,
        sample_rate: SAMPLE_RATE.load(Ordering::Relaxed),
        timestamp: SystemTime::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alloc_profiler_tracking() {
        reset();
        set_sample_rate(512);

        let profiler = AllocProfiler::system();
        assert_eq!(sample_rate(), 512);

        start();
        assert!(is_active());

        let layout = Layout::from_size_align(1024, 8).unwrap();
        let ptr = unsafe { profiler.alloc(layout) };
        assert!(!ptr.is_null());

        let report = heap_report().unwrap();
        assert!(report.total_alloc_bytes() >= 1024);
        assert!(!report.records.is_empty());

        let inuse_rep = report.to_inuse_report();
        assert!(inuse_rep.total_samples() >= 1024);

        let alloc_rep = report.to_alloc_report();
        assert!(alloc_rep.total_samples() >= 1024);

        #[cfg(feature = "_protobuf")]
        {
            let pb = report.pprof().unwrap();
            assert!(!pb.sample.is_empty());
        }

        unsafe { profiler.dealloc(ptr, layout) };

        stop();
        assert!(!is_active());
        reset();
    }
}
