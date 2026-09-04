// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::collections::HashMap;
use std::fmt::{Debug, Formatter};

use spin::RwLock;

use crate::frames::{Frames, UnresolvedFrames};
use crate::profiler::Profiler;
use crate::timer::ReportTiming;

use crate::{Error, Result};

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
        let mut hash_map = HashMap::new();

        match self.profiler.read().as_ref() {
            Err(err) => {
                log::error!("Error in creating profiler: {}", err);
                Err(Error::CreatingError)
            }
            Ok(profiler) => {
                profiler.data.try_iter()?.for_each(|entry| {
                    let count = entry.count;
                    if count > 0 {
                        let key = &entry.item;
                        match hash_map.get_mut(key) {
                            Some(value) => {
                                *value += count;
                            }
                            None => {
                                match hash_map.insert(key.clone(), count) {
                                    None => {}
                                    Some(_) => {
                                        unreachable!();
                                    }
                                };
                            }
                        }
                    }
                });

                Ok(UnresolvedReport {
                    data: hash_map,
                    timing: self.timing.clone(),
                    missed_samples: crate::profiler::MISSED_SAMPLES
                        .load(std::sync::atomic::Ordering::Relaxed),
                })
            }
        }
    }

    /// Build a `Report`.
    pub fn build(&self) -> Result<Report> {
        let mut hash_map = HashMap::new();

        match self.profiler.write().as_mut() {
            Err(err) => {
                log::error!("Error in creating profiler: {}", err);
                Err(Error::CreatingError)
            }
            Ok(profiler) => {
                profiler.data.try_iter()?.for_each(|entry| {
                    let count = entry.count;
                    if count > 0 {
                        let mut key = Frames::from(entry.item.clone());
                        if let Some(processor) = &self.frames_post_processor {
                            processor(&mut key);
                        }

                        match hash_map.get_mut(&key) {
                            Some(value) => {
                                *value += count;
                            }
                            None => {
                                match hash_map.insert(key, count) {
                                    None => {}
                                    Some(_) => {
                                        unreachable!();
                                    }
                                };
                            }
                        }
                    }
                });

                Ok(Report {
                    data: hash_map,
                    timing: self.timing.clone(),
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
            "{{\"$schema\":\"https://www.speedscope.app/file-format-schema.json\",\"version\":\"0.0.1\",\"exporter\":\"rpprof\",\"name\":\"{}\",\"activeProfileIndex\":0,\"profiles\":[{{\"type\":\"sampled\",\"name\":\"{}\",\"unit\":\"count\",\"startValue\":0,\"endValue\":{},\"samples\":[",
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
                flamegraph::from_lines(options, lines.iter().map(|s| &**s), writer).unwrap();
                // TODO: handle this error
            }

            Ok(())
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
    use std::time::SystemTime;

    const SAMPLES: &str = "samples";
    const COUNT: &str = "count";
    const CPU: &str = "cpu";
    const NANOSECONDS: &str = "nanoseconds";
    const THREAD: &str = "thread";

    impl Report {
        /// `pprof` will generate google's pprof format report.
        pub fn pprof(&self) -> crate::Result<protos::Profile> {
            let mut dedup_str = HashSet::new();
            for key in self.data.keys() {
                dedup_str.insert(key.thread_name_or_id());
                for frame in key.frames.iter() {
                    for symbol in frame {
                        dedup_str.insert(symbol.name());
                        dedup_str.insert(symbol.sys_name().into_owned());
                        dedup_str.insert(symbol.filename().into_owned());
                    }
                }
            }
            dedup_str.insert(SAMPLES.into());
            dedup_str.insert(COUNT.into());
            dedup_str.insert(CPU.into());
            dedup_str.insert(NANOSECONDS.into());
            dedup_str.insert(THREAD.into());

            let missed_comment = format!(
                "missed samples due to lock contention: {}",
                self.missed_samples
            );
            dedup_str.insert(missed_comment.clone());

            // string table's first element must be an empty string
            let mut str_tbl = vec!["".to_owned()];
            str_tbl.extend(dedup_str.into_iter());

            let mut strings = HashMap::new();
            for (index, name) in str_tbl.iter().enumerate() {
                strings.insert(name.as_str(), index);
            }

            let mut samples = vec![];
            let mut loc_tbl = vec![];
            let mut fn_tbl = vec![];
            let mut functions = HashMap::new();
            for (key, count) in self.data.iter() {
                let mut locs = vec![];
                for frame in key.frames.iter() {
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
                        // the fn_tbl has the same length with loc_tbl
                        fn_tbl.push(function);
                        loc_tbl.push(loc);
                        // current frame locations
                        locs.push(function_id);
                    }
                }
                let thread_name = protos::Label {
                    key: *strings.get(THREAD).unwrap() as i64,
                    str: *strings.get(&key.thread_name_or_id().as_str()).unwrap() as i64,
                    ..protos::Label::default()
                };
                let sample = protos::Sample {
                    location_id: locs,
                    value: vec![
                        *count as i64,
                        *count as i64 * 1_000_000_000 / self.timing.frequency as i64,
                    ],
                    label: vec![thread_name].into(),
                    ..Default::default()
                };
                samples.push(sample);
            }
            let samples_value = protos::ValueType {
                ty: *strings.get(SAMPLES).unwrap() as i64,
                unit: *strings.get(COUNT).unwrap() as i64,
                ..Default::default()
            };
            let time_value = protos::ValueType {
                ty: *strings.get(CPU).unwrap() as i64,
                unit: *strings.get(NANOSECONDS).unwrap() as i64,
                ..Default::default()
            };
            let missed_comment_idx = *strings.get(missed_comment.as_str()).unwrap() as i64;
            let profile = protos::Profile {
                sample_type: vec![samples_value, time_value.clone()].into(),
                sample: samples.into(),
                string_table: str_tbl.into(),
                function: fn_tbl.into(),
                location: loc_tbl.into(),
                time_nanos: self
                    .timing
                    .start_time
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as i64,
                duration_nanos: self.timing.duration.as_nanos() as i64,
                period_type: Some(time_value).into(),
                period: 1_000_000_000 / self.timing.frequency as i64,
                comment: vec![missed_comment_idx].into(),
                ..protos::Profile::default()
            };
            Ok(profile)
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
        assert!(speedscope_str.contains("\"name\":\"test_profile\""));
        assert!(speedscope_str.contains("\"name\":\"my_func\""));
    }
}
