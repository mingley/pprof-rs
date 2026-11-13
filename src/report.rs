// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::collections::HashMap;
use std::fmt::{Debug, Formatter};

use spin::RwLock;

use crate::collector::{Collector, Entry, ShardedCollector};
use crate::frames::{Frames, SymbolCache, UnresolvedFrames};
use crate::profiler::Profiler;
use crate::timer::ReportTiming;

use crate::{Error, Result};

fn merge_unresolved_entry(
    hash_map: &mut HashMap<UnresolvedFrames, isize>,
    entry: &Entry<UnresolvedFrames>,
) {
    let count = entry.count;
    if count > 0 {
        if let Some(value) = hash_map.get_mut(&entry.item) {
            *value += count;
        } else {
            hash_map.insert(entry.item.clone(), count);
        }
    }
}

#[allow(dead_code)]
pub(crate) fn collect_unresolved_counts(
    collector: &Collector<UnresolvedFrames>,
) -> Result<HashMap<UnresolvedFrames, isize>> {
    let mut hash_map = HashMap::new();
    collector.try_for_each(|entry| merge_unresolved_entry(&mut hash_map, entry))?;
    Ok(hash_map)
}

pub(crate) fn collect_sharded_unresolved_counts(
    collector: &ShardedCollector<UnresolvedFrames>,
) -> Result<HashMap<UnresolvedFrames, isize>> {
    let mut hash_map = HashMap::new();
    collector.try_for_each(|entry| merge_unresolved_entry(&mut hash_map, entry))?;
    Ok(hash_map)
}

/// The final presentation of a report which is actually an `HashMap` from `Frames` to isize (count).
pub struct Report {
    /// Key is a backtrace captured by profiler and value is count of it.
    pub data: HashMap<Frames, isize>,

    /// Collection frequency, start time, duration.
    pub timing: ReportTiming,

    /// Number of samples dropped due to lock contention in the signal handler.
    pub missed_samples: u64,
}

/// The presentation of an unsymbolicated report which is actually an `HashMap` from `UnresolvedFrames` to isize (count).
pub struct UnresolvedReport {
    /// key is a backtrace captured by profiler and value is count of it.
    pub data: HashMap<UnresolvedFrames, isize>,

    /// Collection frequency, start time, duration.
    pub timing: ReportTiming,

    /// Number of samples dropped due to lock contention in the signal handler.
    pub missed_samples: u64,
}

type FramesPostProcessor = Box<dyn Fn(&mut Frames)>;

/// A builder of `Report` and `UnresolvedReport`. It builds report from a running `Profiler`.
pub struct ReportBuilder<'a> {
    frames_post_processor: Option<FramesPostProcessor>,
    profiler: &'a RwLock<Result<Profiler>>,
    timing: ReportTiming,
}

impl<'a> ReportBuilder<'a> {
    pub(crate) fn new(profiler: &'a RwLock<Result<Profiler>>, timing: ReportTiming) -> Self {
        Self {
            frames_post_processor: None,
            profiler,
            timing,
        }
    }

    /// Set `frames_post_processor` of a `ReportBuilder`. Before finally building a report, `frames_post_processor`
    /// will be applied to every Frames.
    pub fn frames_post_processor<T>(&mut self, frames_post_processor: T) -> &mut Self
    where
        T: Fn(&mut Frames) + 'static,
    {
        self.frames_post_processor
            .replace(Box::new(frames_post_processor));

        self
    }

    /// Build an `UnresolvedReport`
    pub fn build_unresolved(&self) -> Result<UnresolvedReport> {
        match self.profiler.write().as_ref() {
            Err(err) => {
                log::error!("Error in creating profiler: {}", err);
                Err(Error::CreatingError)
            }
            Ok(profiler) => {
                let hash_map = collect_sharded_unresolved_counts(&profiler.data)?;
                let mut timing = self.timing.clone();
                timing.dropped_spill_samples = profiler.data.dropped_samples();

                Ok(UnresolvedReport {
                    data: hash_map,
                    timing,
                    missed_samples: crate::profiler::MISSED_SAMPLES
                        .load(std::sync::atomic::Ordering::Relaxed),
                })
            }
        }
    }

    /// Build a `Report`.
    pub fn build(&self) -> Result<Report> {
        match self.profiler.write().as_mut() {
            Err(err) => {
                log::error!("Error in creating profiler: {}", err);
                Err(Error::CreatingError)
            }
            Ok(profiler) => {
                let unresolved_map = collect_sharded_unresolved_counts(&profiler.data)?;
                let dropped_spill = profiler.data.dropped_samples();
                let mut symbol_cache = SymbolCache::default();
                let mut hash_map = HashMap::with_capacity(unresolved_map.len());

                for (unresolved, count) in unresolved_map {
                    let mut key =
                        Frames::from_unresolved_with_cache(&unresolved, &mut symbol_cache);
                    if let Some(processor) = &self.frames_post_processor {
                        processor(&mut key);
                    }

                    match hash_map.get_mut(&key) {
                        Some(value) => {
                            *value += count;
                        }
                        None => {
                            hash_map.insert(key, count);
                        }
                    }
                }

                let mut timing = self.timing.clone();
                timing.dropped_spill_samples = dropped_spill;

                Ok(Report {
                    data: hash_map,
                    timing,
                    missed_samples: crate::profiler::MISSED_SAMPLES
                        .load(std::sync::atomic::Ordering::Relaxed),
                })
            }
        }
    }
}

impl Report {
    /// Returns the total number of sample hits collected across all stack traces.
    pub fn total_samples(&self) -> usize {
        self.data.values().map(|&v| v.max(0) as usize).sum()
    }

    /// Returns the number of samples dropped because the configured spill cap (or spill I/O) was reached.
    pub fn dropped_spill_samples(&self) -> u64 {
        self.timing.dropped_spill_samples
    }

    /// Returns the number of unique backtraces recorded.
    pub fn unique_stacks(&self) -> usize {
        self.data.len()
    }

    /// Returns the collection duration.
    pub fn duration(&self) -> std::time::Duration {
        self.timing.duration
    }

    /// Returns the clock type used for profiling.
    pub fn clock_type(&self) -> crate::timer::ClockType {
        self.timing.clock_type
    }

    /// Calculates the effective samples collected per second across the collection duration.
    pub fn samples_per_second(&self) -> f64 {
        let secs = self.timing.duration.as_secs_f64();
        if secs > 0.0 {
            self.total_samples() as f64 / secs
        } else {
            0.0
        }
    }

    /// Writes raw folded stacks in the standard format:
    /// `thread;frame1;frame2 count\n`
    ///
    /// This format is universally supported by Brendan Gregg's FlameGraph scripts,
    /// speedscope (speedscope.app), and other performance analysis tools.
    pub fn write_folded<W: std::io::Write>(&self, mut writer: W) -> std::io::Result<()> {
        for (key, value) in &self.data {
            if *value <= 0 {
                continue;
            }
            write!(writer, "{}", key.thread_name_or_id())?;

            for frame in key.frames.iter().rev() {
                for symbol in frame.iter().rev() {
                    write!(writer, ";{}", symbol)?;
                }
            }

            writeln!(writer, " {}", value)?;
        }
        Ok(())
    }

    /// Writes the report as a JSON file conforming to the Speedscope file format
    /// (https://www.speedscope.app/file-format-schema.json).
    ///
    /// The resulting file can be opened directly in https://www.speedscope.app
    /// for interactive flamegraphs, sandwich views, and left-heavy analysis.
    pub fn write_speedscope<W: std::io::Write>(
        &self,
        mut writer: W,
        name: &str,
    ) -> std::io::Result<()> {
        let mut frame_indices: HashMap<String, usize> = HashMap::new();
        let mut frame_names: Vec<String> = Vec::new();

        let mut get_frame_idx = |frame_name: &str| -> usize {
            if let Some(&idx) = frame_indices.get(frame_name) {
                idx
            } else {
                let idx = frame_names.len();
                frame_indices.insert(frame_name.to_string(), idx);
                frame_names.push(frame_name.to_string());
                idx
            }
        };

        let mut samples: Vec<Vec<usize>> = Vec::new();
        let mut weights: Vec<usize> = Vec::new();
        let mut total_weight: usize = 0;

        for (key, value) in &self.data {
            if *value <= 0 {
                continue;
            }
            let weight = *value as usize;
            let mut sample = Vec::new();
            sample.push(get_frame_idx(&key.thread_name_or_id()));

            for frame in key.frames.iter().rev() {
                for symbol in frame.iter().rev() {
                    sample.push(get_frame_idx(&symbol.to_string()));
                }
            }

            samples.push(sample);
            weights.push(weight);
            total_weight += weight;
        }

        fn escape_json(s: &str) -> String {
            let mut out = String::with_capacity(s.len() + 8);
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 => {
                        use std::fmt::Write;
                        let _ = write!(&mut out, "\\u{:04x}", c as u32);
                    }
                    c => out.push(c),
                }
            }
            out
        }

        write!(
            writer,
            "{{\"$schema\":\"https://www.speedscope.app/file-format-schema.json\",\"version\":\"0.0.1\",\"exporter\":\"rpprof\",\"name\":\"{}\",\"activeProfileIndex\":0,\"profiles\":[{{\"type\":\"sampled\",\"name\":\"{}\",\"unit\":\"none\",\"startValue\":0,\"endValue\":{},\"samples\":[",
            escape_json(name),
            escape_json(name),
            total_weight
        )?;

        for (i, sample) in samples.iter().enumerate() {
            if i > 0 {
                write!(writer, ",")?;
            }
            write!(writer, "[")?;
            for (j, &idx) in sample.iter().enumerate() {
                if j > 0 {
                    write!(writer, ",")?;
                }
                write!(writer, "{}", idx)?;
            }
            write!(writer, "]")?;
        }

        write!(writer, "],\"weights\":[")?;
        for (i, &weight) in weights.iter().enumerate() {
            if i > 0 {
                write!(writer, ",")?;
            }
            write!(writer, "{}", weight)?;
        }
        write!(writer, "]}}],\"shared\":{{\"frames\":[")?;

        for (i, fname) in frame_names.iter().enumerate() {
            if i > 0 {
                write!(writer, ",")?;
            }
            write!(writer, "{{\"name\":\"{}\"}}", escape_json(fname))?;
        }

        write!(writer, "]}}}}")?;
        Ok(())
    }
}

impl UnresolvedReport {
    /// Returns the total number of sample hits collected across all stack traces.
    pub fn total_samples(&self) -> usize {
        self.data.values().map(|&v| v.max(0) as usize).sum()
    }

    /// Returns the number of samples dropped because the configured spill cap (or spill I/O) was reached.
    pub fn dropped_spill_samples(&self) -> u64 {
        self.timing.dropped_spill_samples
    }

    /// Returns the number of unique backtraces recorded.
    pub fn unique_stacks(&self) -> usize {
        self.data.len()
    }

    /// Returns the collection duration.
    pub fn duration(&self) -> std::time::Duration {
        self.timing.duration
    }

    /// Returns the clock type used for profiling.
    pub fn clock_type(&self) -> crate::timer::ClockType {
        self.timing.clock_type
    }

    /// Calculates the effective samples collected per second across the collection duration.
    pub fn samples_per_second(&self) -> f64 {
        let secs = self.timing.duration.as_secs_f64();
        if secs > 0.0 {
            self.total_samples() as f64 / secs
        } else {
            0.0
        }
    }
}

/// This will generate Report in a human-readable format:
///
/// ```shell
/// FRAME: pprof::profiler::perf_signal_handler::h7b995c4ab2e66493 -> FRAME: Unknown -> FRAME: {func1} ->
/// FRAME: {func2} -> FRAME: {func3} ->  THREAD: {thread_name} {count}
/// ```
impl Debug for Report {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        for (key, val) in self.data.iter() {
            write!(f, "{:?} {}", key, val)?;
            writeln!(f)?;
        }

        Ok(())
    }
}

#[cfg(feature = "flamegraph")]
mod flamegraph {
    use super::*;
    use inferno::flamegraph;
    use std::fmt::Write;

    impl Report {
        /// `flamegraph` will write an svg flamegraph into `writer` **only available with `flamegraph` feature**
        pub fn flamegraph<W>(&self, writer: W) -> Result<()>
        where
            W: std::io::Write,
        {
            self.flamegraph_with_options(writer, &mut flamegraph::Options::default())
        }

        /// same as `flamegraph`, but accepts custom `options` for the flamegraph
        pub fn flamegraph_with_options<W>(
            &self,
            writer: W,
            options: &mut flamegraph::Options,
        ) -> Result<()>
        where
            W: std::io::Write,
        {
            let lines: Vec<String> = self
                .data
                .iter()
                .filter(|&(_, &value)| value > 0)
                .map(|(key, value)| {
                    let mut line = key.thread_name_or_id();
                    line.push(';');

                    for frame in key.frames.iter().rev() {
                        for symbol in frame.iter().rev() {
                            write!(&mut line, "{};", symbol).unwrap();
                        }
                    }

                    line.pop().unwrap_or_default();
                    write!(&mut line, " {}", value).unwrap();

                    line
                })
                .collect();
            if !lines.is_empty() {
                flamegraph::from_lines(options, lines.iter().map(|s| &**s), writer)
                    .map_err(|err| Error::IoError(std::io::Error::other(err)))?;
            }

            Ok(())
        }
    }
}

#[cfg(feature = "_protobuf")]
#[allow(clippy::useless_conversion)]
#[allow(clippy::needless_update)]
pub(crate) mod protobuf {
    use super::*;
    use crate::backtrace::{Frame as BacktraceFrame, Trace, TraceImpl};
    use crate::frames::{Symbol, is_signal_handler_symbol, resolve_frame_symbols};
    use crate::protos;
    use smallvec::SmallVec;
    use std::time::SystemTime;

    const SAMPLES: &str = "samples";
    const COUNT: &str = "count";
    const CPU: &str = "cpu";
    const WALL: &str = "wall";
    const NANOSECONDS: &str = "nanoseconds";
    const THREAD: &str = "thread";

    struct StringTable {
        table: Vec<String>,
        index: HashMap<String, i64>,
    }

    impl StringTable {
        fn new() -> Self {
            // string table's first element must be an empty string
            let mut table = Vec::with_capacity(64);
            let mut index = HashMap::with_capacity(64);
            table.push(String::new());
            index.insert(String::new(), 0);
            Self { table, index }
        }

        fn intern(&mut self, s: &str) -> i64 {
            if let Some(&id) = self.index.get(s) {
                return id;
            }
            let id = self.table.len() as i64;
            let owned = s.to_owned();
            self.table.push(owned.clone());
            self.index.insert(owned, id);
            id
        }
    }

    struct PprofBuilder {
        strings: StringTable,
        loc_tbl: Vec<protos::Location>,
        fn_tbl: Vec<protos::Function>,
        functions: HashMap<(i64, i64, i64), u64>,
        locations: HashMap<(u64, u32), u64>,
        sample_counts: HashMap<(Vec<u64>, i64), i64>,
        thread_key_idx: i64,
    }

    impl PprofBuilder {
        fn with_sample_capacity(sample_cap: usize) -> Self {
            let mut strings = StringTable::new();
            let thread_key_idx = strings.intern(THREAD);
            Self {
                strings,
                loc_tbl: Vec::new(),
                fn_tbl: Vec::new(),
                functions: HashMap::new(),
                locations: HashMap::new(),
                sample_counts: HashMap::with_capacity(sample_cap),
                thread_key_idx,
            }
        }

        fn intern_symbol(&mut self, symbol: &Symbol) -> u64 {
            let name = symbol.name();
            let sys_name = symbol.sys_name();
            let filename = symbol.filename();
            let lineno = symbol.lineno();

            let name_idx = self.strings.intern(&name);
            let sys_name_idx = self.strings.intern(sys_name.as_ref());
            let filename_idx = self.strings.intern(filename.as_ref());

            let fn_key = (name_idx, sys_name_idx, filename_idx);
            let function_id = if let Some(&id) = self.functions.get(&fn_key) {
                id
            } else {
                let id = self.fn_tbl.len() as u64 + 1;
                self.fn_tbl.push(protos::Function {
                    id,
                    name: name_idx,
                    system_name: sys_name_idx,
                    filename: filename_idx,
                    ..protos::Function::default()
                });
                self.functions.insert(fn_key, id);
                id
            };

            let loc_key = (function_id, lineno);
            if let Some(&loc_id) = self.locations.get(&loc_key) {
                loc_id
            } else {
                let loc_id = self.loc_tbl.len() as u64 + 1;
                let line = protos::Line {
                    function_id,
                    line: lineno as i64,
                    ..protos::Line::default()
                };
                self.loc_tbl.push(protos::Location {
                    id: loc_id,
                    line: vec![line].into(),
                    ..protos::Location::default()
                });
                self.locations.insert(loc_key, loc_id);
                loc_id
            }
        }

        fn add_sample(&mut self, locs: Vec<u64>, thread_name: &str, count: i64) {
            let thread_str_idx = self.strings.intern(thread_name);
            *self
                .sample_counts
                .entry((locs, thread_str_idx))
                .or_insert(0) += count;
        }

        fn finish(mut self, timing: &ReportTiming, missed_samples: u64) -> protos::Profile {
            let samples_ty_idx = self.strings.intern(SAMPLES);
            let count_unit_idx = self.strings.intern(COUNT);
            let time_kind = match timing.clock_type {
                crate::timer::ClockType::Cpu => CPU,
                crate::timer::ClockType::Wall => WALL,
            };
            let time_ty_idx = self.strings.intern(time_kind);
            let nanos_unit_idx = self.strings.intern(NANOSECONDS);

            let missed_comment =
                format!("missed samples due to lock contention: {}", missed_samples);
            let mut comments = vec![self.strings.intern(&missed_comment)];
            if timing.dropped_spill_samples > 0 {
                let dropped_comment = format!(
                    "dropped samples due to spill cap: {}",
                    timing.dropped_spill_samples
                );
                comments.push(self.strings.intern(&dropped_comment));
            }

            let freq = i64::from(timing.frequency).max(1);
            let thread_key_idx = self.thread_key_idx;
            let mut samples = Vec::with_capacity(self.sample_counts.len());
            for ((locs, thread_str_idx), count) in self.sample_counts {
                let thread_label = protos::Label {
                    key: thread_key_idx,
                    str: thread_str_idx,
                    ..protos::Label::default()
                };
                samples.push(protos::Sample {
                    location_id: locs,
                    value: vec![count, count * 1_000_000_000 / freq],
                    label: vec![thread_label].into(),
                    ..Default::default()
                });
            }

            let samples_value = protos::ValueType {
                ty: samples_ty_idx,
                unit: count_unit_idx,
                ..Default::default()
            };
            let time_value = protos::ValueType {
                ty: time_ty_idx,
                unit: nanos_unit_idx,
                ..Default::default()
            };

            protos::Profile {
                sample_type: vec![samples_value, time_value.clone()].into(),
                sample: samples.into(),
                string_table: self.strings.table.into(),
                function: self.fn_tbl.into(),
                location: self.loc_tbl.into(),
                time_nanos: timing
                    .start_time
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as i64,
                duration_nanos: timing.duration.as_nanos() as i64,
                period_type: Some(time_value).into(),
                period: 1_000_000_000 / freq,
                comment: comments.into(),
                ..protos::Profile::default()
            }
        }
    }

    enum PcResolution {
        SignalHandler,
        Locations(SmallVec<[u64; 2]>),
    }

    pub(crate) fn build_pprof_from_unresolved_with<R>(
        unresolved_map: HashMap<UnresolvedFrames, isize>,
        timing: &ReportTiming,
        missed_samples: u64,
        mut resolve_frame: R,
    ) -> protos::Profile
    where
        R: FnMut(&<TraceImpl as Trace>::Frame, &mut Vec<Symbol>),
    {
        let mut builder = PprofBuilder::with_sample_capacity(unresolved_map.len());
        let mut pc_cache: HashMap<usize, PcResolution> = HashMap::new();
        let mut scratch_symbols: Vec<Symbol> = Vec::new();

        for (unresolved, count) in unresolved_map {
            if count <= 0 {
                continue;
            }
            let mut locs = Vec::with_capacity(unresolved.frames.len());
            let mut frame_iter = unresolved.frames.iter();

            while let Some(frame) = frame_iter.next() {
                let ip = BacktraceFrame::ip(frame);
                if let Some(cached) = pc_cache.get(&ip) {
                    match cached {
                        PcResolution::SignalHandler => {
                            frame_iter.next();
                            continue;
                        }
                        PcResolution::Locations(ids) => {
                            locs.extend_from_slice(ids);
                            continue;
                        }
                    }
                }

                resolve_frame(frame, &mut scratch_symbols);
                if scratch_symbols.iter().any(is_signal_handler_symbol) {
                    pc_cache.insert(ip, PcResolution::SignalHandler);
                    frame_iter.next();
                    continue;
                }

                let mut pc_locs: SmallVec<[u64; 2]> =
                    SmallVec::with_capacity(scratch_symbols.len());
                for symbol in &scratch_symbols {
                    pc_locs.push(builder.intern_symbol(symbol));
                }
                locs.extend_from_slice(&pc_locs);
                pc_cache.insert(ip, PcResolution::Locations(pc_locs));
            }

            let thread_name = unresolved.thread_name_or_id();
            builder.add_sample(locs, &thread_name, count as i64);
        }

        builder.finish(timing, missed_samples)
    }

    impl ReportBuilder<'_> {
        /// Builds a Google `pprof` protobuf profile directly from deduplicated
        /// raw stack frames without materializing an intermediate [`Report`].
        ///
        /// When a custom [`Self::frames_post_processor`] is configured, this falls
        /// back to `self.build()?.pprof()` so post-processor mutations are honored.
        pub fn build_pprof(&self) -> crate::Result<protos::Profile> {
            if self.frames_post_processor.is_some() {
                return self.build()?.pprof();
            }

            match self.profiler.write().as_mut() {
                Err(err) => {
                    log::error!("Error in creating profiler: {}", err);
                    Err(Error::CreatingError)
                }
                Ok(profiler) => {
                    let unresolved_map = collect_sharded_unresolved_counts(&profiler.data)?;
                    let mut timing = self.timing.clone();
                    timing.dropped_spill_samples = profiler.data.dropped_samples();
                    let missed_samples =
                        crate::profiler::MISSED_SAMPLES.load(std::sync::atomic::Ordering::Relaxed);

                    Ok(build_pprof_from_unresolved_with(
                        unresolved_map,
                        &timing,
                        missed_samples,
                        resolve_frame_symbols,
                    ))
                }
            }
        }
    }

    #[cfg(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        any(target_os = "linux", target_os = "macos"),
        feature = "framehop-unwinder"
    ))]
    fn read_build_id_hex(path: &std::path::Path, is_first: bool) -> Option<String> {
        use object::Object;
        use std::fmt::Write;

        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            #[cfg(target_os = "linux")]
            Err(_) if is_first => std::fs::File::open("/proc/self/exe").ok()?,
            Err(_) => {
                let _ = is_first;
                return None;
            }
        };
        let mmap = unsafe { memmap2::Mmap::map(&file) }.ok()?;
        let obj = object::File::parse(&*mmap).ok()?;
        let uuid_buf;
        let bytes: &[u8] = if let Ok(Some(bid)) = obj.build_id() {
            bid
        } else if let Ok(Some(uuid)) = obj.mach_uuid() {
            uuid_buf = uuid;
            uuid_buf.as_slice()
        } else {
            return None;
        };
        if bytes.is_empty() {
            return None;
        }
        let mut hex = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(&mut hex, "{b:02x}");
        }
        Some(hex)
    }

    #[cfg(not(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        any(target_os = "linux", target_os = "macos"),
        feature = "framehop-unwinder"
    )))]
    fn read_build_id_hex(_path: &std::path::Path, _is_first: bool) -> Option<String> {
        None
    }

    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    fn collect_process_mappings(strings: &mut StringTable) -> Vec<protos::Mapping> {
        use findshlibs::{SharedLibrary, TargetSharedLibrary};
        use std::path::PathBuf;

        let mut mappings = Vec::new();
        let mut is_first = true;
        TargetSharedLibrary::each(|shlib| {
            let first = is_first;
            is_first = false;
            let len = shlib.len() as u64;
            if len == 0 {
                return;
            }
            let start = shlib.actual_load_addr().0 as u64;
            let limit = start.saturating_add(len);
            let path = PathBuf::from(shlib.name());
            let filename_idx = strings.intern(&path.to_string_lossy());
            let build_id_idx = read_build_id_hex(&path, first)
                .map(|bid| strings.intern(&bid))
                .unwrap_or(0);
            let id = mappings.len() as u64 + 1;
            mappings.push(protos::Mapping {
                id,
                memory_start: start,
                memory_limit: limit,
                filename: filename_idx,
                build_id: build_id_idx,
                ..protos::Mapping::default()
            });
        });
        mappings
    }

    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )))]
    fn collect_process_mappings(_strings: &mut StringTable) -> Vec<protos::Mapping> {
        Vec::new()
    }

    impl UnresolvedReport {
        /// Generates an unsymbolized Google `pprof` protobuf profile containing raw
        /// instruction addresses, loaded binary [`protos::Mapping`] ranges, and build IDs
        /// (when available) for zero-symbolization-overhead capture and offline symbolication.
        pub fn pprof(&self) -> crate::Result<protos::Profile> {
            let mut strings = StringTable::new();
            let thread_key_idx = strings.intern(THREAD);
            let mappings = collect_process_mappings(&mut strings);

            let mut sorted_ranges: Vec<(u64, u64, u64)> = mappings
                .iter()
                .map(|m| (m.memory_start, m.memory_limit, m.id))
                .collect();
            sorted_ranges.sort_unstable_by_key(|&(start, _, _)| start);

            let find_mapping = |addr: u64| -> u64 {
                let idx = sorted_ranges.partition_point(|&(start, _, _)| start <= addr);
                for &(start, limit, id) in sorted_ranges[..idx].iter().rev() {
                    if start <= addr && addr < limit {
                        return id;
                    }
                    if limit <= addr {
                        break;
                    }
                }
                0
            };

            let mut loc_tbl: Vec<protos::Location> = Vec::new();
            let mut pc_to_loc: HashMap<u64, u64> = HashMap::new();
            let mut sample_counts: HashMap<(Vec<u64>, i64), i64> =
                HashMap::with_capacity(self.data.len());

            for (unresolved, &count) in &self.data {
                if count <= 0 {
                    continue;
                }
                let mut locs = Vec::with_capacity(unresolved.frames.len());
                for frame in &unresolved.frames {
                    let addr = BacktraceFrame::ip(frame) as u64;
                    let loc_id = if let Some(&id) = pc_to_loc.get(&addr) {
                        id
                    } else {
                        let id = loc_tbl.len() as u64 + 1;
                        let mapping_id = find_mapping(addr);
                        loc_tbl.push(protos::Location {
                            id,
                            mapping_id,
                            address: addr,
                            ..protos::Location::default()
                        });
                        pc_to_loc.insert(addr, id);
                        id
                    };
                    locs.push(loc_id);
                }
                let thread_name = unresolved.thread_name_or_id();
                let thread_str_idx = strings.intern(&thread_name);
                *sample_counts.entry((locs, thread_str_idx)).or_insert(0) += count as i64;
            }

            let samples_ty_idx = strings.intern(SAMPLES);
            let count_unit_idx = strings.intern(COUNT);
            let time_kind = match self.timing.clock_type {
                crate::timer::ClockType::Cpu => CPU,
                crate::timer::ClockType::Wall => WALL,
            };
            let time_ty_idx = strings.intern(time_kind);
            let nanos_unit_idx = strings.intern(NANOSECONDS);

            let missed_comment = format!(
                "missed samples due to lock contention: {}",
                self.missed_samples
            );
            let mut comments = vec![strings.intern(&missed_comment)];
            if self.timing.dropped_spill_samples > 0 {
                let dropped_comment = format!(
                    "dropped samples due to spill cap: {}",
                    self.timing.dropped_spill_samples
                );
                comments.push(strings.intern(&dropped_comment));
            }

            let freq = i64::from(self.timing.frequency).max(1);
            let mut samples = Vec::with_capacity(sample_counts.len());
            for ((locs, thread_str_idx), count) in sample_counts {
                let thread_label = protos::Label {
                    key: thread_key_idx,
                    str: thread_str_idx,
                    ..protos::Label::default()
                };
                samples.push(protos::Sample {
                    location_id: locs,
                    value: vec![count, count * 1_000_000_000 / freq],
                    label: vec![thread_label].into(),
                    ..Default::default()
                });
            }

            let samples_value = protos::ValueType {
                ty: samples_ty_idx,
                unit: count_unit_idx,
                ..Default::default()
            };
            let time_value = protos::ValueType {
                ty: time_ty_idx,
                unit: nanos_unit_idx,
                ..Default::default()
            };

            Ok(protos::Profile {
                sample_type: vec![samples_value, time_value.clone()].into(),
                sample: samples.into(),
                mapping: mappings.into(),
                location: loc_tbl.into(),
                string_table: strings.table.into(),
                time_nanos: self
                    .timing
                    .start_time
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as i64,
                duration_nanos: self.timing.duration.as_nanos() as i64,
                period_type: Some(time_value).into(),
                period: 1_000_000_000 / freq,
                comment: comments.into(),
                ..protos::Profile::default()
            })
        }
    }

    impl Report {
        /// `pprof` will generate google's pprof format report.
        pub fn pprof(&self) -> crate::Result<protos::Profile> {
            let mut builder = PprofBuilder::with_sample_capacity(self.data.len());
            for (key, count) in self.data.iter() {
                let mut locs = Vec::with_capacity(key.frames.len());
                for frame in key.frames.iter() {
                    for symbol in frame {
                        locs.push(builder.intern_symbol(symbol));
                    }
                }
                let thread_name = key.thread_name_or_id();
                builder.add_sample(locs, &thread_name, *count as i64);
            }
            Ok(builder.finish(&self.timing, self.missed_samples))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Symbol;
    use std::time::{Duration, SystemTime};

    #[test]
    fn test_report_metrics_and_folded() {
        let mut data = HashMap::new();
        let frames = Frames {
            frames: vec![vec![Symbol {
                name: Some(b"my_func".to_vec()),
                addr: None,
                lineno: Some(10),
                filename: None,
            }]],
            thread_name: "main".to_string(),
            thread_id: 1,
            sample_timestamp: SystemTime::UNIX_EPOCH,
        };
        data.insert(frames, 42);

        let report = Report {
            data,
            timing: ReportTiming {
                frequency: 100,
                start_time: SystemTime::UNIX_EPOCH,
                duration: Duration::from_secs(2),
                clock_type: crate::timer::ClockType::Cpu,
                dropped_spill_samples: 0,
            },
            missed_samples: 0,
        };

        assert_eq!(report.total_samples(), 42);
        assert_eq!(report.unique_stacks(), 1);
        assert_eq!(report.duration(), Duration::from_secs(2));
        assert_eq!(report.samples_per_second(), 21.0);
        assert_eq!(report.clock_type(), crate::timer::ClockType::Cpu);

        let mut folded_output = Vec::new();
        report.write_folded(&mut folded_output).unwrap();
        let folded_str = String::from_utf8(folded_output).unwrap();
        assert!(folded_str.contains("main;my_func 42"));

        let mut speedscope_output = Vec::new();
        report
            .write_speedscope(&mut speedscope_output, "test_profile")
            .unwrap();
        let speedscope_str = String::from_utf8(speedscope_output).unwrap();
        assert!(speedscope_str.contains("\"exporter\":\"rpprof\""));
        assert!(speedscope_str.contains("\"unit\":\"none\""));
        assert!(speedscope_str.contains("\"name\":\"test_profile\""));
        assert!(speedscope_str.contains("\"name\":\"my_func\""));

        #[cfg(feature = "flamegraph")]
        {
            struct FailingWriter;
            impl std::io::Write for FailingWriter {
                fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                    Err(std::io::Error::other("simulated write failure"))
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Err(std::io::Error::other("simulated flush failure"))
                }
            }
            assert!(report.flamegraph(FailingWriter).is_err());
        }
    }

    #[cfg(feature = "_protobuf")]
    mod protobuf_tests {
        use super::*;
        use crate::protos;
        use std::collections::BTreeMap;

        type ResolvedFrameInfo = (String, String, String, i64);
        type SampleKey = (Vec<ResolvedFrameInfo>, Vec<(String, String)>);

        #[derive(Debug, PartialEq, Eq)]
        struct NormalizedProfile {
            sample_types: Vec<(String, String)>,
            period_type: Option<(String, String)>,
            period: i64,
            time_nanos: i64,
            duration_nanos: i64,
            comments: Vec<String>,
            samples: BTreeMap<SampleKey, Vec<i64>>,
        }

        fn normalize_profile(profile: &protos::Profile) -> NormalizedProfile {
            assert_eq!(
                profile.string_table.first().map(String::as_str),
                Some(""),
                "string_table[0] must be empty"
            );
            let s = |idx: i64| profile.string_table[idx as usize].clone();

            let funcs: HashMap<u64, (String, String, String)> = profile
                .function
                .iter()
                .map(|f| (f.id, (s(f.name), s(f.system_name), s(f.filename))))
                .collect();

            let locs: HashMap<u64, Vec<ResolvedFrameInfo>> = profile
                .location
                .iter()
                .map(|loc| {
                    let lines = loc
                        .line
                        .iter()
                        .map(|ln| {
                            let (name, sys, file) = funcs[&ln.function_id].clone();
                            (name, sys, file, ln.line)
                        })
                        .collect();
                    (loc.id, lines)
                })
                .collect();

            let mut samples: BTreeMap<SampleKey, Vec<i64>> = BTreeMap::new();
            for sample in &profile.sample {
                let mut expanded_frames = Vec::new();
                for &loc_id in &sample.location_id {
                    expanded_frames.extend(locs[&loc_id].iter().cloned());
                }
                let mut labels: Vec<(String, String)> =
                    sample.label.iter().map(|l| (s(l.key), s(l.str))).collect();
                labels.sort();

                let entry = samples
                    .entry((expanded_frames, labels))
                    .or_insert_with(|| vec![0; sample.value.len()]);
                for (acc, &v) in entry.iter_mut().zip(&sample.value) {
                    *acc += v;
                }
            }

            NormalizedProfile {
                sample_types: profile
                    .sample_type
                    .iter()
                    .map(|vt| (s(vt.ty), s(vt.unit)))
                    .collect(),
                period_type: profile
                    .period_type
                    .as_ref()
                    .map(|vt| (s(vt.ty), s(vt.unit))),
                period: profile.period,
                time_nanos: profile.time_nanos,
                duration_nanos: profile.duration_nanos,
                comments: profile.comment.iter().map(|&idx| s(idx)).collect(),
                samples,
            }
        }

        fn encode_roundtrip_bytes(profile: &protos::Profile) -> Vec<u8> {
            use crate::protos::Message;
            let mut bytes = Vec::new();
            #[cfg(not(feature = "protobuf-codec"))]
            {
                profile.encode(&mut bytes).unwrap();
                let decoded = protos::Profile::decode(bytes.as_slice()).unwrap();
                assert_eq!(normalize_profile(profile), normalize_profile(&decoded));
            }
            #[cfg(feature = "protobuf-codec")]
            {
                profile.write_to_vec(&mut bytes).unwrap();
                let decoded = protos::Profile::parse_from_bytes(&bytes).unwrap();
                assert_eq!(normalize_profile(profile), normalize_profile(&decoded));
            }
            bytes
        }

        #[inline(always)]
        fn inlined_work_step(x: u64) -> u64 {
            std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1))
        }

        #[inline(never)]
        fn outer_work_driver(iters: usize) -> u64 {
            let mut acc = 1u64;
            for _ in 0..iters {
                acc = inlined_work_step(acc);
            }
            acc
        }

        #[test]
        fn test_live_build_pprof_matches_report_pprof_and_post_processor() {
            let _serial = crate::profiler::tests::TEST_PROFILER_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());

            let guard = crate::ProfilerGuard::new(200).unwrap();
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_millis(80) {
                std::hint::black_box(outer_work_driver(2_000));
            }
            // Disarm the interval timer before comparing two sequential builds on the same guard
            // so no new SIGPROF tick mutates collector or missed_samples in between.
            drop(crate::timer::Timer::new(1));

            let rb = guard.report();
            let via_report = rb.build().unwrap().pprof().unwrap();
            let via_direct = rb.build_pprof().unwrap();
            assert_eq!(
                normalize_profile(&via_report),
                normalize_profile(&via_direct)
            );
            assert!(!encode_roundtrip_bytes(&via_direct).is_empty());

            // Also verify equivalence when `frames_post_processor` is configured.
            let mut rb_proc = guard.report();
            rb_proc.frames_post_processor(|frames| {
                frames.thread_name = "normalized-worker".to_string();
            });
            let proc_via_report = rb_proc.build().unwrap().pprof().unwrap();
            let proc_via_direct = rb_proc.build_pprof().unwrap();
            assert_eq!(
                normalize_profile(&proc_via_report),
                normalize_profile(&proc_via_direct)
            );

            // Verify UnresolvedReport::pprof() populates mappings + raw addresses without symbolizing.
            let unresolved_rep = guard.report().build_unresolved().unwrap();
            let unresolved_pb = unresolved_rep.pprof().unwrap();
            assert!(!unresolved_pb.mapping.is_empty());
            assert!(
                unresolved_pb
                    .location
                    .iter()
                    .all(|loc| loc.address != 0 && loc.line.is_empty())
            );
            assert!(unresolved_pb.location.iter().any(|loc| loc.mapping_id != 0));
            assert!(!encode_roundtrip_bytes(&unresolved_pb).is_empty());
        }

        #[test]
        fn test_wall_clock_pprof_emits_wall_value_type() {
            let mut data = HashMap::new();
            data.insert(
                Frames {
                    frames: vec![vec![crate::Symbol {
                        name: Some(b"parked_io".to_vec()),
                        addr: None,
                        lineno: Some(12),
                        filename: None,
                    }]],
                    thread_name: "io-worker".to_string(),
                    thread_id: 7,
                    sample_timestamp: SystemTime::UNIX_EPOCH,
                },
                10,
            );
            let report = Report {
                data,
                timing: ReportTiming {
                    frequency: 100,
                    start_time: SystemTime::UNIX_EPOCH,
                    duration: Duration::from_secs(1),
                    clock_type: crate::timer::ClockType::Wall,
                    dropped_spill_samples: 0,
                },
                missed_samples: 0,
            };
            let pb = report.pprof().unwrap();
            let norm = normalize_profile(&pb);
            assert_eq!(
                norm.sample_types,
                vec![
                    ("samples".to_string(), "count".to_string()),
                    ("wall".to_string(), "nanoseconds".to_string()),
                ]
            );
            assert_eq!(
                norm.period_type,
                Some(("wall".to_string(), "nanoseconds".to_string()))
            );
        }

        #[cfg(any(feature = "framehop-unwinder", feature = "frame-pointer"))]
        #[test]
        fn test_synthetic_inlined_frames_build_pprof_equivalence() {
            use crate::backtrace::Trace;
            use std::path::PathBuf;
            type RawFrame = <crate::backtrace::TraceImpl as Trace>::Frame;

            let mock_symbols = |ip: usize, out: &mut Vec<Symbol>| {
                out.clear();
                match ip {
                    0x10 => out.push(Symbol {
                        name: Some(b"_perf_signal_handler".to_vec()),
                        addr: None,
                        lineno: Some(1),
                        filename: None,
                    }),
                    0x20 => out.push(Symbol {
                        name: Some(b"__sigtramp".to_vec()),
                        addr: None,
                        lineno: Some(1),
                        filename: None,
                    }),
                    0x30 => {
                        // 3-deep inlined chain at a single PC
                        out.push(Symbol {
                            name: Some(b"_ZN4core5slice4iterE".to_vec()),
                            addr: None,
                            lineno: Some(101),
                            filename: Some(PathBuf::from("core/slice.rs")),
                        });
                        out.push(Symbol {
                            name: Some(b"inlined_mid".to_vec()),
                            addr: None,
                            lineno: Some(55),
                            filename: Some(PathBuf::from("src/mid.rs")),
                        });
                        out.push(Symbol {
                            name: Some(b"outer_handler".to_vec()),
                            addr: None,
                            lineno: Some(88),
                            filename: Some(PathBuf::from("src/handler.rs")),
                        });
                    }
                    0x40 => out.push(Symbol {
                        name: Some(b"tokio_poll".to_vec()),
                        addr: None,
                        lineno: Some(210),
                        filename: Some(PathBuf::from("src/runtime.rs")),
                    }),
                    0x50 => out.push(Symbol {
                        name: Some(b"leaf_fast_path".to_vec()),
                        addr: None,
                        lineno: Some(14),
                        filename: Some(PathBuf::from("src/fast.rs")),
                    }),
                    _ => {}
                }
            };

            let mut unresolved_map = HashMap::new();
            let s1 = UnresolvedFrames::new(
                smallvec::smallvec![
                    RawFrame { ip: 0x10 },
                    RawFrame { ip: 0x20 },
                    RawFrame { ip: 0x30 },
                    RawFrame { ip: 0x40 },
                ],
                b"tokio-worker-1",
                11,
                SystemTime::UNIX_EPOCH + Duration::from_secs(1),
            );
            let s2 = UnresolvedFrames::new(
                smallvec::smallvec![
                    RawFrame { ip: 0x50 },
                    RawFrame { ip: 0x30 },
                    RawFrame { ip: 0x40 },
                ],
                b"",
                22,
                SystemTime::UNIX_EPOCH + Duration::from_secs(2),
            );
            unresolved_map.insert(s1, 37);
            unresolved_map.insert(s2, 19);

            let timing = ReportTiming {
                frequency: 99,
                start_time: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                duration: Duration::from_secs(120),
                clock_type: crate::timer::ClockType::Cpu,
                dropped_spill_samples: 7,
            };
            let missed_samples = 3;

            // Build `Report` via `SymbolCache` + `Frames`
            let mut sym_cache = SymbolCache::default();
            let mut report_data = HashMap::new();
            for (u, count) in &unresolved_map {
                let f = Frames::from_unresolved_with_resolver(u, &mut sym_cache, |frame, out| {
                    mock_symbols(crate::backtrace::Frame::ip(frame), out);
                });
                *report_data.entry(f).or_insert(0) += *count;
            }
            let report = Report {
                data: report_data,
                timing: timing.clone(),
                missed_samples,
            };
            let pprof_from_report = report.pprof().unwrap();

            // Build directly via `build_pprof_from_unresolved_with` without materializing `Report`
            let pprof_direct = protobuf::build_pprof_from_unresolved_with(
                unresolved_map,
                &timing,
                missed_samples,
                |frame, out| mock_symbols(crate::backtrace::Frame::ip(frame), out),
            );

            let norm_report = normalize_profile(&pprof_from_report);
            let norm_direct = normalize_profile(&pprof_direct);
            assert_eq!(norm_report, norm_direct);
            assert_eq!(norm_direct.samples.len(), 2);
            // Verify the 3 inlined symbols + 1 caller = 4 expanded frames on the first stack.
            assert!(
                norm_direct
                    .samples
                    .keys()
                    .any(|(frames, _)| frames.len() == 4 && frames[0].0 == "core::slice::iter")
            );
            assert!(!encode_roundtrip_bytes(&pprof_direct).is_empty());
        }

        #[cfg(any(feature = "framehop-unwinder", feature = "frame-pointer"))]
        mod high_cardinality_pcs {
            #[inline(never)]
            pub(super) fn pc_fn<const N: usize>(x: usize) -> usize {
                std::hint::black_box(x.wrapping_mul(31).wrapping_add(N))
            }

            macro_rules! gen_32 {
                ($base:expr, $($off:expr),+ $(,)?) => {
                    [ $( (pc_fn::<{ $base + $off }> as *const () as usize) + 4 ),+ ]
                };
            }

            macro_rules! gen_row {
                ($base:expr) => {
                    gen_32!(
                        $base, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18,
                        19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
                    )
                };
            }

            pub(super) fn unique_1000_pcs() -> Vec<usize> {
                let rows: [[usize; 32]; 32] = [
                    gen_row!(0),
                    gen_row!(32),
                    gen_row!(64),
                    gen_row!(96),
                    gen_row!(128),
                    gen_row!(160),
                    gen_row!(192),
                    gen_row!(224),
                    gen_row!(256),
                    gen_row!(288),
                    gen_row!(320),
                    gen_row!(352),
                    gen_row!(384),
                    gen_row!(416),
                    gen_row!(448),
                    gen_row!(480),
                    gen_row!(512),
                    gen_row!(544),
                    gen_row!(576),
                    gen_row!(608),
                    gen_row!(640),
                    gen_row!(672),
                    gen_row!(704),
                    gen_row!(736),
                    gen_row!(768),
                    gen_row!(800),
                    gen_row!(832),
                    gen_row!(864),
                    gen_row!(896),
                    gen_row!(928),
                    gen_row!(960),
                    gen_row!(992),
                ];
                let mut pcs: Vec<usize> = rows.iter().flatten().copied().take(1_000).collect();
                pcs.sort_unstable();
                pcs.dedup();
                assert_eq!(pcs.len(), 1_000, "all 1,000 generated PCs must be distinct");
                pcs
            }
        }

        #[cfg(any(feature = "framehop-unwinder", feature = "frame-pointer"))]
        #[test]
        fn test_high_cardinality_50k_spilled_100_frames_1k_pcs_bounded_memory() {
            use crate::backtrace::Trace;
            use crate::collector::{BUCKETS, BUCKETS_ASSOCIATIVITY, Collector};
            use crate::frames::resolve_frame_symbols;
            use smallvec::SmallVec;
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};

            type RawFrame = <crate::backtrace::TraceImpl as Trace>::Frame;

            let _serial = crate::profiler::tests::TEST_PROFILER_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // Warm up backtrace/symbolizer state once outside byte tracking, matching a running service.
            let _ = backtrace::Backtrace::new();

            let pcs = high_cardinality_pcs::unique_1000_pcs();
            const DEPTH: usize = 100;
            const TARGET_SPILLED: usize = 50_000;

            // Build candidate 100-frame stacks covering all 1,000 unique PCs and group into
            // colliding 5-stack sets (exceeding BUCKETS_ASSOCIATIVITY = 4) so cycling them
            // deterministically spills 50,000+ entries to the temporary spill file.
            let mut by_bucket: HashMap<usize, Vec<UnresolvedFrames>> = HashMap::new();
            let mut colliding_groups: Vec<Vec<UnresolvedFrames>> = Vec::new();
            let mut idx = 0usize;
            while colliding_groups.len() < 40 {
                let mut frames: SmallVec<[RawFrame; crate::MAX_DEPTH]> =
                    SmallVec::with_capacity(DEPTH);
                for d in 0..DEPTH {
                    let pc = pcs[(idx * 97 + d * 13 + (idx / 10)) % pcs.len()];
                    frames.push(RawFrame { ip: pc });
                }
                let uf = UnresolvedFrames::new(
                    frames,
                    b"async-tokio",
                    (idx % 8) as u64 + 1,
                    SystemTime::UNIX_EPOCH,
                );
                let mut hasher = DefaultHasher::new();
                uf.hash(&mut hasher);
                let bucket = (hasher.finish() % BUCKETS as u64) as usize;
                let group = by_bucket.entry(bucket).or_default();
                if !group.contains(&uf) {
                    group.push(uf);
                    if group.len() == BUCKETS_ASSOCIATIVITY + 1 {
                        colliding_groups.push(std::mem::take(group));
                    }
                }
                idx += 1;
            }
            drop(by_bucket);

            // Ensure every one of the 1,000 unique PCs appears in at least one active stack.
            let mut extra_cover: Vec<UnresolvedFrames> = Vec::new();
            for chunk_idx in 0..(1_000 / DEPTH) {
                let mut frames: SmallVec<[RawFrame; crate::MAX_DEPTH]> =
                    SmallVec::with_capacity(DEPTH);
                for d in 0..DEPTH {
                    frames.push(RawFrame {
                        ip: pcs[chunk_idx * DEPTH + d],
                    });
                }
                extra_cover.push(UnresolvedFrames::new(
                    frames,
                    b"cover-worker",
                    100 + chunk_idx as u64,
                    SystemTime::UNIX_EPOCH,
                ));
            }

            let mut collector = Collector::<UnresolvedFrames>::new().unwrap();
            let mut total_added: i64 = 0;
            for stack in &extra_cover {
                collector.add(stack.clone(), 1).unwrap();
                total_added += 1;
            }

            let mut round = 0usize;
            while collector.spilled_entries() < TARGET_SPILLED {
                let group = &colliding_groups[round % colliding_groups.len()];
                for stack in group {
                    collector.add(stack.clone(), 1).unwrap();
                    total_added += 1;
                }
                round += 1;
            }

            let spilled_entries = collector.spilled_entries();
            let flushed_bytes = collector.flushed_spill_bytes();
            assert!(
                spilled_entries >= TARGET_SPILLED,
                "expected >= 50k spilled entries, got {spilled_entries}"
            );
            assert!(
                flushed_bytes > 40 * 1024 * 1024,
                "expected > 40 MiB spill file, got {flushed_bytes} bytes"
            );

            let timing = ReportTiming {
                frequency: 99,
                start_time: SystemTime::UNIX_EPOCH,
                duration: Duration::from_secs(300),
                clock_type: crate::timer::ClockType::Cpu,
                dropped_spill_samples: 0,
            };

            let mut symbol_resolutions = 0usize;
            crate::profiler::tests::ALLOC.start_byte_tracking();
            let unresolved_map = collect_unresolved_counts(&collector).unwrap();
            let unique_stacks = unresolved_map.len();
            let profile = protobuf::build_pprof_from_unresolved_with(
                unresolved_map,
                &timing,
                0,
                |frame, out| {
                    symbol_resolutions += 1;
                    resolve_frame_symbols(frame, out);
                },
            );
            let (peak_bytes, max_single_alloc) = crate::profiler::tests::ALLOC.stop_byte_tracking();

            assert_eq!(
                symbol_resolutions, 1_000,
                "each of the 1,000 unique PCs must be symbolized exactly once across {spilled_entries} spilled entries ({unique_stacks} unique stacks)"
            );

            let profile_total_samples: i64 = profile.sample.iter().map(|s| s.value[0]).sum();
            assert_eq!(profile_total_samples, total_added);

            // `flushed_bytes` is ~54 MiB (default MAX_DEPTH=128) or ~412 MiB (large-depth=1024).
            // Streaming chunks + pre-aggregation + direct pprof interning must never allocate
            // a contiguous whole-spill-file buffer and must keep peak live heap bounded.
            assert!(
                max_single_alloc < 4 * 1024 * 1024 && max_single_alloc < flushed_bytes / 10,
                "max single alloc ({max_single_alloc} B) must stay far below spill file ({flushed_bytes} B)"
            );
            assert!(
                peak_bytes < 24 * 1024 * 1024 && peak_bytes < flushed_bytes / 2,
                "peak live heap delta ({peak_bytes} B) must stay bounded well below spill file ({flushed_bytes} B)"
            );
        }
    }
}
