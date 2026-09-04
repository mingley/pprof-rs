// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::os::raw::c_int;
use std::ptr::null_mut;
use std::time::{Duration, Instant, SystemTime};

#[repr(C)]
#[derive(Clone)]
struct Timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

#[repr(C)]
#[derive(Clone)]
struct Itimerval {
    pub it_interval: Timeval,
    pub it_value: Timeval,
}

unsafe extern "C" {
    fn setitimer(which: c_int, new_value: *mut Itimerval, old_value: *mut Itimerval) -> c_int;
}

const ITIMER_REAL: c_int = 0;
const ITIMER_PROF: c_int = 2;

/// The clock type used for profiling.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ClockType {
    /// Profiles CPU time consumed by the process using `ITIMER_PROF` / `SIGPROF` (default).
    #[default]
    Cpu,
    /// Profiles real / wall-clock time using `ITIMER_REAL` / `SIGALRM`.
    ///
    /// Useful for identifying bottlenecks in I/O, lock contention, database queries,
    /// or network requests where threads are blocked off-CPU.
    Wall,
}

pub struct Timer {
    pub frequency: c_int,
    pub clock_type: ClockType,
    pub start_time: SystemTime,
    pub start_instant: Instant,
}

impl Timer {
    #[allow(dead_code)]
    pub fn new(frequency: c_int) -> Timer {
        Self::with_clock_type(frequency, ClockType::Cpu)
    }

    pub fn with_clock_type(frequency: c_int, clock_type: ClockType) -> Timer {
        let interval = 1e6 as i64 / i64::from(frequency);
        let it_interval = Timeval {
            tv_sec: interval / 1e6 as i64,
            tv_usec: interval % 1e6 as i64,
        };
        let it_value = it_interval.clone();
        let which = match clock_type {
            ClockType::Cpu => ITIMER_PROF,
            ClockType::Wall => ITIMER_REAL,
        };

        unsafe {
            setitimer(
                which,
                &mut Itimerval {
                    it_interval,
                    it_value,
                },
                null_mut(),
            )
        };

        Timer {
            frequency,
            clock_type,
            start_time: SystemTime::now(),
            start_instant: Instant::now(),
        }
    }

    /// Returns a `ReportTiming` struct having this timer's frequency, clock type, and start
    /// time; and the time elapsed since its creation as duration.
    pub fn timing(&self) -> ReportTiming {
        ReportTiming {
            frequency: self.frequency,
            start_time: self.start_time,
            duration: self.start_instant.elapsed(),
            clock_type: self.clock_type,
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        let it_interval = Timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let it_value = it_interval.clone();
        let which = match self.clock_type {
            ClockType::Cpu => ITIMER_PROF,
            ClockType::Wall => ITIMER_REAL,
        };
        unsafe {
            setitimer(
                which,
                &mut Itimerval {
                    it_interval,
                    it_value,
                },
                null_mut(),
            )
        };
    }
}

/// Timing metadata for a collected report.
#[derive(Clone)]
pub struct ReportTiming {
    /// Frequency at which samples were collected.
    pub frequency: i32,
    /// Collection start time.
    pub start_time: SystemTime,
    /// Collection duration.
    pub duration: Duration,
    /// Clock type used for profiling.
    pub clock_type: ClockType,
}

impl Default for ReportTiming {
    fn default() -> Self {
        Self {
            frequency: 1,
            start_time: SystemTime::UNIX_EPOCH,
            duration: Default::default(),
            clock_type: ClockType::Cpu,
        }
    }
}
