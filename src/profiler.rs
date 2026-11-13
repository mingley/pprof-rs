// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::convert::TryInto;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use nix::sys::signal;
use once_cell::sync::Lazy;
use smallvec::SmallVec;
use spin::RwLock;

#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64"
))]
use findshlibs::{Segment, SharedLibrary, TargetSharedLibrary};

use crate::backtrace::{Trace, TraceImpl};
use crate::collector::{ConcurrentAddOutcome, ShardedCollector};
use crate::error::{Error, Result};
use crate::frames::UnresolvedFrames;
use crate::report::ReportBuilder;
use crate::timer::Timer;
use crate::{MAX_DEPTH, MAX_THREAD_NAME};

pub(crate) static PROFILER: Lazy<RwLock<Result<Profiler>>> =
    Lazy::new(|| RwLock::new(Profiler::new()));

/// Number of SIGPROF ticks dropped because the profiler lock could not be
/// acquired in the signal handler. Each dropped tick corresponds to one missed
/// sampling interval. Reset to zero when the profiler starts.
pub(crate) static MISSED_SAMPLES: AtomicU64 = AtomicU64::new(0);

/// Number of samples dropped because the configured spill cap (or spill I/O)
/// was reached. Reset to zero when the profiler starts.
pub(crate) static DROPPED_SPILL_SAMPLES: AtomicU64 = AtomicU64::new(0);

pub struct Profiler {
    pub(crate) data: ShardedCollector<UnresolvedFrames>,
    sample_counter: AtomicU64,

    old_sigaction: Option<signal::SigAction>,
    running: bool,

    #[cfg(feature = "frame-pointer")]
    on_stack: bool,

    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    blocklist_segments: Vec<(usize, usize)>,
}

#[derive(Clone)]
pub struct ProfilerGuardBuilder {
    frequency: c_int,
    max_spill_entries: Option<usize>,

    #[cfg(feature = "frame-pointer")]
    on_stack: bool,

    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    blocklist_segments: Vec<(usize, usize)>,
}

impl Default for ProfilerGuardBuilder {
    fn default() -> ProfilerGuardBuilder {
        ProfilerGuardBuilder {
            frequency: 99,
            max_spill_entries: None,

            #[cfg(feature = "frame-pointer")]
            on_stack: false,

            #[cfg(any(
                target_arch = "x86_64",
                target_arch = "aarch64",
                target_arch = "riscv64",
                target_arch = "loongarch64"
            ))]
            blocklist_segments: Vec::new(),
        }
    }
}

impl ProfilerGuardBuilder {
    pub fn frequency(self, frequency: c_int) -> Self {
        Self { frequency, ..self }
    }

    /// Sets an upper bound (in bytes) on evicted sample entries spilled outside the in-memory table.
    ///
    /// When reached, evicted samples that cannot fit in the in-memory table are dropped cleanly
    /// and counted via [`ProfilerGuard::dropped_spill_samples`] and [`Report::dropped_spill_samples`],
    /// strictly bounding `/tmp` (`tmpfs`) usage. Passing `0` disables spilling completely.
    pub fn max_spill_bytes(mut self, max_bytes: usize) -> Self {
        let entry_size = std::mem::size_of::<crate::collector::Entry<UnresolvedFrames>>().max(1);
        let max_entries = max_bytes / entry_size;
        self.max_spill_entries = Some(match self.max_spill_entries {
            Some(existing) => existing.min(max_entries),
            None => max_entries,
        });
        self
    }

    /// Sets an upper bound on the number of evicted sample entries spilled outside the in-memory table.
    ///
    /// When reached, evicted samples that cannot fit in the in-memory table are dropped cleanly
    /// and counted via [`ProfilerGuard::dropped_spill_samples`] and [`Report::dropped_spill_samples`].
    /// Passing `0` disables spilling completely.
    pub fn max_spill_entries(mut self, max_entries: usize) -> Self {
        self.max_spill_entries = Some(match self.max_spill_entries {
            Some(existing) => existing.min(max_entries),
            None => max_entries,
        });
        self
    }

    #[cfg(feature = "frame-pointer")]
    /// Sets whether to use an alternate signal stack via `SA_ONSTACK`.
    ///
    /// This is only available and only works correctly when the `frame-pointer` feature is enabled.
    ///
    /// The `backtrace-rs` unwinder ignores the signal context and unwinds the current stack. Using
    /// an alternate stack with it would produce meaningless results. The `frame-pointer` unwinder,
    /// however, uses the provided `ucontext` to correctly walk the original application stack.
    ///
    /// This should be enabled when the profiler is used in an environment
    /// with small stacks (e.g., inside a Go program) to prevent stack overflow.
    pub fn on_stack(self, on_stack: bool) -> Self {
        Self { on_stack, ..self }
    }

    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    pub fn blocklist<T: AsRef<str>>(self, blocklist: &[T]) -> Self {
        let blocklist_segments = {
            let mut segments = Vec::new();
            TargetSharedLibrary::each(|shlib| {
                let in_blocklist = match shlib.name().to_str() {
                    Some(name) => {
                        let mut in_blocklist = false;
                        for blocked_name in blocklist.iter() {
                            if name.contains(blocked_name.as_ref()) {
                                in_blocklist = true;
                            }
                        }

                        in_blocklist
                    }

                    None => false,
                };
                if in_blocklist {
                    for seg in shlib.segments() {
                        let avam = seg.actual_virtual_memory_address(shlib);
                        let start = avam.0;
                        let end = start + seg.len();
                        segments.push((start, end));
                    }
                }
            });
            segments
        };

        Self {
            blocklist_segments,
            ..self
        }
    }
    pub fn build(self) -> Result<ProfilerGuard<'static>> {
        trigger_lazy();

        match PROFILER.write().as_mut() {
            Err(err) => {
                log::error!("Error in creating profiler: {}", err);
                Err(Error::CreatingError)
            }
            Ok(profiler) => {
                #[cfg(feature = "frame-pointer")]
                {
                    profiler.on_stack = self.on_stack;
                }

                #[cfg(any(
                    target_arch = "x86_64",
                    target_arch = "aarch64",
                    target_arch = "riscv64",
                    target_arch = "loongarch64"
                ))]
                {
                    profiler.blocklist_segments = self.blocklist_segments;
                }

                match profiler.start_with_spill_cap(self.max_spill_entries) {
                    Ok(()) => Ok(ProfilerGuard::<'static> {
                        profiler: &PROFILER,
                        timer: Some(Timer::new(self.frequency)),
                    }),
                    Err(err) => Err(err),
                }
            }
        }
    }
}

/// RAII structure used to stop profiling when dropped. It is the only interface to access profiler.
pub struct ProfilerGuard<'a> {
    profiler: &'a RwLock<Result<Profiler>>,
    timer: Option<Timer>,
}

fn trigger_lazy() {
    crate::addr_validate::init();
    let _ = backtrace::Backtrace::new();
    let _profiler = PROFILER.read();
    TraceImpl::init();
}

impl ProfilerGuard<'_> {
    /// Start profiling with given sample frequency.
    pub fn new(frequency: c_int) -> Result<ProfilerGuard<'static>> {
        ProfilerGuardBuilder::default().frequency(frequency).build()
    }

    /// Generate a report
    pub fn report(&self) -> ReportBuilder<'_> {
        ReportBuilder::new(
            self.profiler,
            self.timer.as_ref().map(Timer::timing).unwrap_or_default(),
        )
    }

    /// Returns the number of samples dropped due to lock contention in the signal handler.
    pub fn missed_samples(&self) -> u64 {
        MISSED_SAMPLES.load(Ordering::Relaxed)
    }

    /// Returns the number of samples dropped because the configured spill cap (or spill I/O) was reached.
    pub fn dropped_spill_samples(&self) -> u64 {
        DROPPED_SPILL_SAMPLES.load(Ordering::Relaxed)
    }
}

impl<'a> Drop for ProfilerGuard<'a> {
    fn drop(&mut self) {
        drop(self.timer.take());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut spins = 0u32;
        loop {
            if let Some(mut guard) = self.profiler.try_write() {
                if let Ok(profiler) = guard.as_mut()
                    && let Err(err) = profiler.stop()
                {
                    log::error!("error while stopping profiler {}", err);
                }
                return;
            }
            if std::time::Instant::now() >= deadline {
                log::error!("timed out waiting for profiler lock during drop; installing SIG_IGN");
                let ignore = signal::SigAction::new(
                    signal::SigHandler::SigIgn,
                    signal::SaFlags::empty(),
                    signal::SigSet::empty(),
                );
                unsafe {
                    let _ = signal::sigaction(signal::Signal::SIGPROF, &ignore);
                }
                return;
            }
            if spins < 64 {
                std::hint::spin_loop();
                spins += 1;
            } else {
                std::thread::sleep(std::time::Duration::from_micros(50));
            }
        }
    }
}

fn write_thread_name_fallback(current_thread: libc::pthread_t, name: &mut [libc::c_char]) {
    let mut len = 0;
    let mut base = 1;

    while current_thread as u128 > base && len < MAX_THREAD_NAME {
        base *= 10;
        len += 1;
    }

    let mut index = 0;
    while index < len && base > 1 {
        base /= 10;

        name[index] = match (48 + (current_thread as u128 / base) % 10).try_into() {
            Ok(digit) => digit,
            Err(_) => {
                log::error!("fail to convert thread_id to string");
                0
            }
        };

        index += 1;
    }
}

#[cfg(not(all(any(target_os = "linux", target_os = "macos"), target_env = "gnu")))]
fn write_thread_name(current_thread: libc::pthread_t, name: &mut [libc::c_char]) {
    write_thread_name_fallback(current_thread, name);
}

#[cfg(all(any(target_os = "linux", target_os = "macos"), target_env = "gnu"))]
fn write_thread_name(current_thread: libc::pthread_t, name: &mut [libc::c_char]) {
    let name_ptr = name as *mut [libc::c_char] as *mut libc::c_char;
    let ret = unsafe { libc::pthread_getname_np(current_thread, name_ptr, MAX_THREAD_NAME) };

    if ret != 0 {
        write_thread_name_fallback(current_thread, name);
    }
}

struct ErrnoProtector(libc::c_int);

impl ErrnoProtector {
    fn new() -> Self {
        unsafe {
            #[cfg(target_os = "android")]
            {
                let errno = *libc::__errno();
                Self(errno)
            }
            #[cfg(target_os = "linux")]
            {
                let errno = *libc::__errno_location();
                Self(errno)
            }
            #[cfg(any(target_os = "macos", target_os = "freebsd"))]
            {
                let errno = *libc::__error();
                Self(errno)
            }
        }
    }
}

impl Drop for ErrnoProtector {
    fn drop(&mut self) {
        unsafe {
            #[cfg(target_os = "android")]
            {
                *libc::__errno() = self.0;
            }
            #[cfg(target_os = "linux")]
            {
                *libc::__errno_location() = self.0;
            }
            #[cfg(any(target_os = "macos", target_os = "freebsd"))]
            {
                *libc::__error() = self.0;
            }
        }
    }
}

#[unsafe(no_mangle)]
#[cfg_attr(
    not(all(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))),
    allow(unused_variables)
)]
#[allow(clippy::unnecessary_cast)]
extern "C" fn perf_signal_handler(
    _signal: c_int,
    _siginfo: *mut libc::siginfo_t,
    ucontext: *mut libc::c_void,
) {
    let _errno = ErrnoProtector::new();

    if let Some(guard) = PROFILER.try_read() {
        if let Ok(profiler) = guard.as_ref() {
            if !profiler.running {
                return;
            }
            #[cfg(any(
                target_arch = "x86_64",
                target_arch = "aarch64",
                target_arch = "riscv64",
                target_arch = "loongarch64"
            ))]
            if !ucontext.is_null() {
                let ucontext: *mut libc::ucontext_t = ucontext as *mut libc::ucontext_t;

                #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
                let addr =
                    unsafe { (*ucontext).uc_mcontext.gregs[libc::REG_RIP as usize] as usize };

                #[cfg(all(target_arch = "x86_64", target_os = "freebsd"))]
                let addr = unsafe { (*ucontext).uc_mcontext.mc_rip as usize };

                #[cfg(all(target_arch = "x86_64", target_os = "macos"))]
                let addr = unsafe {
                    let mcontext = std::ptr::addr_of!((*ucontext).uc_mcontext).read_unaligned();
                    if mcontext.is_null() {
                        0
                    } else {
                        std::ptr::addr_of!((*mcontext).__ss.__rip).read_unaligned() as usize
                    }
                };

                #[cfg(all(
                    target_arch = "aarch64",
                    any(target_os = "android", target_os = "linux")
                ))]
                let addr = unsafe { (*ucontext).uc_mcontext.pc as usize };

                #[cfg(all(target_arch = "aarch64", target_os = "freebsd"))]
                let addr = unsafe { (*ucontext).mc_gpregs.gp_elr as usize };

                #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
                let addr = unsafe {
                    let mcontext = std::ptr::addr_of!((*ucontext).uc_mcontext).read_unaligned();
                    if mcontext.is_null() {
                        0
                    } else {
                        std::ptr::addr_of!((*mcontext).__ss.__pc).read_unaligned() as usize
                    }
                };

                #[cfg(all(target_arch = "riscv64", target_os = "linux"))]
                let addr = unsafe { (*ucontext).uc_mcontext.__gregs[libc::REG_PC] as usize };

                #[cfg(all(target_arch = "loongarch64", target_os = "linux"))]
                let addr = unsafe { (*ucontext).uc_mcontext.__pc as usize };

                if profiler.is_blocklisted(addr) {
                    return;
                }
            }

            let mut bt: SmallVec<[<TraceImpl as Trace>::Frame; MAX_DEPTH]> =
                SmallVec::with_capacity(MAX_DEPTH);
            let mut index = 0;

            let sample_timestamp: SystemTime = SystemTime::now();
            TraceImpl::trace(ucontext, |frame| {
                #[cfg(feature = "frame-pointer")]
                {
                    let ip = crate::backtrace::Frame::ip(frame);
                    if profiler.is_blocklisted(ip) {
                        return false;
                    }
                }

                if index < MAX_DEPTH {
                    bt.push(frame.clone());
                    index += 1;
                    true
                } else {
                    false
                }
            });

            let current_thread = unsafe { libc::pthread_self() };
            let mut name = [0; MAX_THREAD_NAME];
            let name_ptr = &mut name as *mut [libc::c_char] as *mut libc::c_char;

            write_thread_name(current_thread, &mut name);

            let name = unsafe { std::ffi::CStr::from_ptr(name_ptr) };
            profiler.sample(bt, name.to_bytes(), current_thread as u64, sample_timestamp);
        }
    } else {
        MISSED_SAMPLES.fetch_add(1, Ordering::Relaxed);
    }
}

impl Profiler {
    fn new() -> Result<Self> {
        Ok(Profiler {
            data: ShardedCollector::new()?,
            sample_counter: AtomicU64::new(0),
            old_sigaction: None,
            running: false,

            #[cfg(feature = "frame-pointer")]
            on_stack: false,

            #[cfg(any(
                target_arch = "x86_64",
                target_arch = "aarch64",
                target_arch = "riscv64",
                target_arch = "loongarch64"
            ))]
            blocklist_segments: Vec::new(),
        })
    }

    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    ))]
    fn is_blocklisted(&self, addr: usize) -> bool {
        for libs in &self.blocklist_segments {
            if addr > libs.0 && addr < libs.1 {
                return true;
            }
        }
        false
    }
}

impl Profiler {
    #[allow(dead_code)]
    pub fn start(&mut self) -> Result<()> {
        self.start_with_spill_cap(None)
    }

    pub(crate) fn start_with_spill_cap(&mut self, max_spill_entries: Option<usize>) -> Result<()> {
        log::info!("starting cpu profiler");
        if self.running {
            Err(Error::Running)
        } else {
            self.data.configure_max_spill_entries(max_spill_entries)?;
            self.register_signal_handler()?;
            MISSED_SAMPLES.store(0, Ordering::Relaxed);
            DROPPED_SPILL_SAMPLES.store(0, Ordering::Relaxed);
            self.running = true;

            Ok(())
        }
    }

    /// Number of samples dropped due to lock contention in the signal handler.
    #[allow(dead_code)]
    pub fn missed_samples() -> u64 {
        MISSED_SAMPLES.load(Ordering::Relaxed)
    }

    /// Number of samples dropped because the configured spill cap (or spill I/O) was reached.
    #[allow(dead_code)]
    pub fn dropped_spill_samples() -> u64 {
        DROPPED_SPILL_SAMPLES.load(Ordering::Relaxed)
    }

    fn init(&mut self) -> Result<()> {
        self.sample_counter.store(0, Ordering::Relaxed);
        self.data.reset()?;
        self.running = false;

        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        log::info!("stopping cpu profiler");
        if self.running {
            self.unregister_signal_handler()?;
            self.init()?;

            Ok(())
        } else {
            Err(Error::NotRunning)
        }
    }

    fn register_signal_handler(&mut self) -> Result<()> {
        let handler = signal::SigHandler::SigAction(perf_signal_handler);
        // SA_RESTART will only restart a syscall when it's safe to do so,
        // e.g. when it's a blocking read(2) or write(2). See man 7 signal.
        let flags = signal::SaFlags::SA_SIGINFO | signal::SaFlags::SA_RESTART;
        #[cfg(feature = "frame-pointer")]
        let flags = if self.on_stack {
            // SA_ONSTACK will deliver the signal on an alternate stack. This is crucial
            // to prevent a stack overflow if the signal arrives at a thread with
            // a small stack, which is common when use pprof-rs in Go runtimes.
            flags | signal::SaFlags::SA_ONSTACK
        } else {
            flags
        };
        let sigaction = signal::SigAction::new(handler, flags, signal::SigSet::empty());
        let old_action = unsafe { signal::sigaction(signal::SIGPROF, &sigaction) }?;
        self.old_sigaction = Some(old_action);
        Ok(())
    }

    fn unregister_signal_handler(&mut self) -> Result<()> {
        if let Some(old_action) = self.old_sigaction.take() {
            if matches!(old_action.handler(), signal::SigHandler::SigDfl) {
                let ignore = signal::SigAction::new(
                    signal::SigHandler::SigIgn,
                    signal::SaFlags::empty(),
                    signal::SigSet::empty(),
                );
                unsafe { signal::sigaction(signal::SIGPROF, &ignore) }?;
            } else {
                unsafe { signal::sigaction(signal::SIGPROF, &old_action) }?;
            }
        }
        Ok(())
    }

    // This function has to be AS-safe
    pub fn sample(
        &self,
        backtrace: SmallVec<[<TraceImpl as Trace>::Frame; MAX_DEPTH]>,
        thread_name: &[u8],
        thread_id: u64,
        sample_timestamp: SystemTime,
    ) {
        let frames = UnresolvedFrames::new(backtrace, thread_name, thread_id, sample_timestamp);
        self.sample_counter.fetch_add(1, Ordering::Relaxed);

        match self.data.try_add(frames, 1) {
            ConcurrentAddOutcome::Recorded => {}
            ConcurrentAddOutcome::DroppedSpill(dropped) => {
                DROPPED_SPILL_SAMPLES.fetch_add(dropped, Ordering::Relaxed);
            }
            ConcurrentAddOutcome::Contended(missed) => {
                MISSED_SAMPLES.fetch_add(missed.max(1), Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize};

    pub(crate) static TEST_PROFILER_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) struct AllocDetector {
        should_count_alloc: AtomicBool,
        target_thread: AtomicUsize,
        alloc_count: AtomicUsize,
        track_bytes: AtomicBool,
        current_bytes: AtomicIsize,
        peak_bytes: AtomicIsize,
        max_single_alloc_bytes: AtomicUsize,
    }

    unsafe impl std::alloc::GlobalAlloc for AllocDetector {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            if self.should_count_alloc.load(Ordering::SeqCst) {
                let tid = unsafe { libc::pthread_self() as usize };
                if tid == self.target_thread.load(Ordering::SeqCst) {
                    self.alloc_count.fetch_add(1, Ordering::SeqCst);
                }
            }

            let ptr = unsafe { std::alloc::System.alloc(layout) };
            if !ptr.is_null() && self.track_bytes.load(Ordering::Relaxed) {
                let tid = unsafe { libc::pthread_self() as usize };
                if tid == self.target_thread.load(Ordering::Relaxed) {
                    let size = layout.size();
                    let delta = size as isize;
                    let cur = self.current_bytes.fetch_add(delta, Ordering::Relaxed) + delta;
                    self.peak_bytes.fetch_max(cur, Ordering::Relaxed);
                    self.max_single_alloc_bytes
                        .fetch_max(size, Ordering::Relaxed);
                }
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            if self.track_bytes.load(Ordering::Relaxed) {
                let tid = unsafe { libc::pthread_self() as usize };
                if tid == self.target_thread.load(Ordering::Relaxed) {
                    self.current_bytes
                        .fetch_sub(layout.size() as isize, Ordering::Relaxed);
                }
            }
            unsafe { std::alloc::System.dealloc(ptr, layout) }
        }
    }
    impl AllocDetector {
        fn enable_count_alloc(&self) {
            let tid = unsafe { libc::pthread_self() as usize };
            self.target_thread.store(tid, Ordering::SeqCst);
            self.alloc_count.store(0, Ordering::SeqCst);
            self.should_count_alloc.store(true, Ordering::SeqCst);
        }

        fn disable_count_alloc(&self) {
            self.should_count_alloc.store(false, Ordering::SeqCst);
        }

        fn alloc_count(&self) -> usize {
            self.alloc_count.load(Ordering::SeqCst)
        }

        pub(crate) fn start_byte_tracking(&self) {
            let tid = unsafe { libc::pthread_self() as usize };
            self.target_thread.store(tid, Ordering::SeqCst);
            self.current_bytes.store(0, Ordering::SeqCst);
            self.peak_bytes.store(0, Ordering::SeqCst);
            self.max_single_alloc_bytes.store(0, Ordering::SeqCst);
            self.track_bytes.store(true, Ordering::SeqCst);
        }

        pub(crate) fn stop_byte_tracking(&self) -> (usize, usize) {
            self.track_bytes.store(false, Ordering::SeqCst);
            let peak = self.peak_bytes.load(Ordering::SeqCst).max(0) as usize;
            (peak, self.max_single_alloc_bytes.load(Ordering::SeqCst))
        }
    }

    #[global_allocator]
    pub(crate) static ALLOC: AllocDetector = AllocDetector {
        should_count_alloc: AtomicBool::new(false),
        target_thread: AtomicUsize::new(0),
        alloc_count: AtomicUsize::new(0),
        track_bytes: AtomicBool::new(false),
        current_bytes: AtomicIsize::new(0),
        peak_bytes: AtomicIsize::new(0),
        max_single_alloc_bytes: AtomicUsize::new(0),
    };

    #[test]
    fn test_no_alloc_during_unwind() {
        let _serial = TEST_PROFILER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        trigger_lazy();
        PROFILER.write().as_mut().unwrap().start().unwrap();
        let timer = Timer::new(999);
        let start = std::time::Instant::now();
        ALLOC.enable_count_alloc();

        // alloc something to make sure the ALLOC works fine.
        let _alloc = Box::new(1usize);
        // busy loop for a while to trigger some samples
        while start.elapsed().as_millis() < 500 {
            std::hint::black_box(());
        }
        ALLOC.disable_count_alloc();

        assert_eq!(ALLOC.alloc_count(), 1);

        drop(timer);
        PROFILER.write().as_mut().unwrap().stop().unwrap();
    }

    #[test]
    fn test_missed_samples_counter() {
        let _serial = TEST_PROFILER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        trigger_lazy();
        MISSED_SAMPLES.store(42, Ordering::Relaxed);
        let guard = ProfilerGuard::new(100).unwrap();
        assert_eq!(guard.missed_samples(), 0);

        // Simulated missed sample (e.g. from lock contention in signal handler)
        MISSED_SAMPLES.fetch_add(5, Ordering::Relaxed);
        assert_eq!(guard.missed_samples(), 5);

        let report = guard.report().build().unwrap();
        assert_eq!(report.missed_samples, 5);
    }

    #[test]
    fn test_stray_sigprof_handling() {
        let _serial = TEST_PROFILER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        trigger_lazy();
        {
            let _guard = ProfilerGuard::new(100).unwrap();
        }
        // Profiler guard is dropped, unregister_signal_handler should have set SIG_IGN
        // (since original disposition was default/none), so raising SIGPROF must NOT terminate the process.
        unsafe {
            libc::raise(libc::SIGPROF);
        }
    }

    #[test]
    fn test_profiler_guard_spill_cap_accounting() {
        let _serial = TEST_PROFILER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        trigger_lazy();
        let max_spill = 12usize;
        let guard = ProfilerGuardBuilder::default()
            .frequency(100)
            .max_spill_entries(max_spill)
            .build()
            .unwrap();
        drop(Timer::new(1));
        let total_injected: u64 =
            (crate::collector::BUCKETS * crate::collector::BUCKETS_ASSOCIATIVITY + max_spill + 45)
                as u64;

        {
            let mut lock = PROFILER.write();
            let profiler = lock.as_mut().unwrap();
            profiler
                .data
                .configure_max_spill_entries(Some(max_spill))
                .unwrap();
            DROPPED_SPILL_SAMPLES.store(0, Ordering::Relaxed);

            for tid in 1..=total_injected {
                profiler.sample(SmallVec::new(), b"cap-test", tid, SystemTime::UNIX_EPOCH);
            }
            assert!(profiler.data.spilled_entries() <= max_spill);
        }

        let dropped = guard.dropped_spill_samples();
        assert!(
            dropped >= 45,
            "expected >= 45 dropped spill samples, got {dropped}"
        );

        let unresolved = guard.report().build_unresolved().unwrap();
        assert_eq!(unresolved.dropped_spill_samples(), dropped);
        let unresolved_total: usize = unresolved.data.values().map(|&v| v.max(0) as usize).sum();
        assert_eq!(unresolved_total as u64 + dropped, total_injected);

        let report = guard.report().build().unwrap();
        assert_eq!(report.dropped_spill_samples(), dropped);
        let report_total: usize = report.data.values().map(|&v| v.max(0) as usize).sum();
        assert_eq!(report_total as u64 + dropped, total_injected);

        #[cfg(feature = "_protobuf")]
        {
            let expected_comment = format!("dropped samples due to spill cap: {dropped}");
            let p1 = report.pprof().unwrap();
            let p2 = guard.report().build_pprof().unwrap();
            let comments_1: Vec<&str> = p1
                .comment
                .iter()
                .map(|&idx| p1.string_table[idx as usize].as_str())
                .collect();
            let comments_2: Vec<&str> = p2
                .comment
                .iter()
                .map(|&idx| p2.string_table[idx as usize].as_str())
                .collect();
            assert!(comments_1.contains(&expected_comment.as_str()));
            assert_eq!(comments_1, comments_2);
        }
    }
}
