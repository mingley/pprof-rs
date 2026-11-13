// Copyright 2026 rpprof Authors. Licensed under Apache-2.0 OR MIT.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::SystemTime;

use crate::Result;
use crate::frames::{Frames, Symbol, SymbolCache};
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
        let _ = BYTES_LEFT.try_with(|bytes_left| bytes_left.set(0));
    }
}

static HEAP_STATE: OnceLock<HeapState> = OnceLock::new();

fn state() -> &'static HeapState {
    HEAP_STATE.get_or_init(HeapState::new)
}

thread_local! {
    static IN_ALLOC: Cell<bool> = const { Cell::new(false) };
    static BYTES_LEFT: Cell<isize> = const { Cell::new(0) };
    static RNG_STATE: Cell<u64> = const { Cell::new(0) };
}

fn next_poisson_interval(rate: usize) -> isize {
    if rate <= 1 {
        return 1;
    }
    let u = RNG_STATE
        .try_with(|state| {
            let mut x = state.get();
            if x == 0 {
                x = (state as *const Cell<u64> as usize as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
                    | 1;
            }
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            state.set(x);
            let rand64 = x.wrapping_mul(0x2545_f491_4f6c_dd1d);
            ((rand64 >> 11) as f64 + 1.0) * (1.0 / ((1u64 << 53) as f64 + 1.0))
        })
        .unwrap_or(0.5);
    let sample = -(rate as f64) * u.ln();
    if !sample.is_finite() || sample < 1.0 {
        1
    } else if sample > (isize::MAX / 2) as f64 {
        isize::MAX / 2
    } else {
        sample as isize
    }
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
                    bytes_left.set(next_poisson_interval(rate));

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
                dropped_spill_samples: 0,
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
                dropped_spill_samples: 0,
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
            let mut str_tbl = vec![String::new()];
            let mut str_index: HashMap<String, i64> = HashMap::new();
            str_index.insert(String::new(), 0);

            let mut intern = |s: &str| -> i64 {
                if let Some(&id) = str_index.get(s) {
                    return id;
                }
                let id = str_tbl.len() as i64;
                let owned = s.to_owned();
                str_tbl.push(owned.clone());
                str_index.insert(owned, id);
                id
            };

            let mut samples = Vec::with_capacity(self.records.len());
            let mut loc_tbl = Vec::new();
            let mut fn_tbl = Vec::new();
            let mut functions: HashMap<(i64, i64, i64), u64> = HashMap::new();
            let mut locations: HashMap<(u64, u32), u64> = HashMap::new();

            for rec in &self.records {
                let mut locs = Vec::with_capacity(rec.frames.frames.len());
                for frame in &rec.frames.frames {
                    for symbol in frame {
                        let name = symbol.name();
                        let sys_name = symbol.sys_name();
                        let filename = symbol.filename();
                        let lineno = symbol.lineno();

                        let name_idx = intern(&name);
                        let sys_idx = intern(sys_name.as_ref());
                        let file_idx = intern(filename.as_ref());

                        let fn_key = (name_idx, sys_idx, file_idx);
                        let function_id = if let Some(&id) = functions.get(&fn_key) {
                            id
                        } else {
                            let id = fn_tbl.len() as u64 + 1;
                            fn_tbl.push(protos::Function {
                                id,
                                name: name_idx,
                                system_name: sys_idx,
                                filename: file_idx,
                                ..protos::Function::default()
                            });
                            functions.insert(fn_key, id);
                            id
                        };

                        let loc_key = (function_id, lineno);
                        let loc_id = if let Some(&id) = locations.get(&loc_key) {
                            id
                        } else {
                            let id = loc_tbl.len() as u64 + 1;
                            let line = protos::Line {
                                function_id,
                                line: lineno as i64,
                                ..protos::Line::default()
                            };
                            loc_tbl.push(protos::Location {
                                id,
                                line: vec![line].into(),
                                ..protos::Location::default()
                            });
                            locations.insert(loc_key, id);
                            id
                        };
                        locs.push(loc_id);
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

            let alloc_obj_idx = intern(ALLOC_OBJECTS);
            let alloc_space_idx = intern(ALLOC_SPACE);
            let inuse_obj_idx = intern(INUSE_OBJECTS);
            let inuse_space_idx = intern(INUSE_SPACE);
            let count_idx = intern(COUNT);
            let bytes_idx = intern(BYTES);
            let space_idx = intern(SPACE);

            let sample_types = vec![
                protos::ValueType {
                    ty: alloc_obj_idx,
                    unit: count_idx,
                    ..Default::default()
                },
                protos::ValueType {
                    ty: alloc_space_idx,
                    unit: bytes_idx,
                    ..Default::default()
                },
                protos::ValueType {
                    ty: inuse_obj_idx,
                    unit: count_idx,
                    ..Default::default()
                },
                protos::ValueType {
                    ty: inuse_space_idx,
                    unit: bytes_idx,
                    ..Default::default()
                },
            ];

            let period_type = protos::ValueType {
                ty: space_idx,
                unit: bytes_idx,
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
    let mut symbol_cache = SymbolCache::default();

    for data in data_read.values() {
        let mut frames_vec = Vec::with_capacity(data.ips.len());

        for &ip in &data.ips {
            let cached = symbol_cache.resolve_ip_with(ip, |pc, out| {
                backtrace::resolve(pc as *mut _, |sym| {
                    out.push(Symbol {
                        name: sym.name().map(|n| n.as_bytes().to_vec()),
                        addr: sym.addr(),
                        lineno: sym.lineno(),
                        filename: sym.filename().map(|f| f.to_path_buf()),
                    });
                });
            });

            if !cached.symbols.is_empty() {
                frames_vec.push(cached.symbols.as_ref().to_vec());
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
