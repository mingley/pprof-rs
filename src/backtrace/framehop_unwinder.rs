use framehop::{
    CacheNative, MustNotAllocateDuringUnwind, UnwindRegsNative, Unwinder, UnwinderNative,
};
use libc::{c_void, ucontext_t};
use once_cell::sync::Lazy;
use spin::Mutex;
mod shlib;

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn get_regs_from_context(ucontext: *mut c_void) -> Option<(UnwindRegsNative, u64)> {
    let ucontext: *mut ucontext_t = ucontext as *mut ucontext_t;
    if ucontext.is_null() {
        return None;
    }

    let thread_state = unsafe {
        let mcontext = std::ptr::addr_of!((*ucontext).uc_mcontext).read_unaligned();
        if mcontext.is_null() {
            return None;
        } else {
            std::ptr::addr_of!((*mcontext).__ss).read_unaligned()
        }
    };

    Some((
        UnwindRegsNative::new(thread_state.__lr, thread_state.__sp, thread_state.__fp),
        thread_state.__pc,
    ))
}

#[cfg(all(target_arch = "x86_64", target_os = "macos"))]
fn get_regs_from_context(ucontext: *mut c_void) -> Option<(UnwindRegsNative, u64)> {
    let ucontext: *mut ucontext_t = ucontext as *mut ucontext_t;
    if ucontext.is_null() {
        return None;
    }

    let thread_state = unsafe {
        let mcontext = std::ptr::addr_of!((*ucontext).uc_mcontext).read_unaligned();
        if mcontext.is_null() {
            return None;
        } else {
            std::ptr::addr_of!((*mcontext).__ss).read_unaligned()
        }
    };

    Some((
        UnwindRegsNative::new(thread_state.__rip, thread_state.__rsp, thread_state.__rbp),
        thread_state.__rip,
    ))
}

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
fn get_regs_from_context(ucontext: *mut c_void) -> Option<(UnwindRegsNative, u64)> {
    let ucontext: *mut ucontext_t = ucontext as *mut ucontext_t;
    if ucontext.is_null() {
        return None;
    }

    let regs = unsafe { &(*ucontext).uc_mcontext.regs };
    let sp = unsafe { (*ucontext).uc_mcontext.sp };
    let pc = unsafe { (*ucontext).uc_mcontext.pc };
    Some((UnwindRegsNative::new(regs[30], sp, regs[29]), pc))
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn get_regs_from_context(ucontext: *mut c_void) -> Option<(UnwindRegsNative, u64)> {
    let ucontext: *mut ucontext_t = ucontext as *mut ucontext_t;
    if ucontext.is_null() {
        return None;
    }
    let regs = unsafe { &(*ucontext).uc_mcontext.gregs };

    Some((
        UnwindRegsNative::new(
            regs[libc::REG_RIP as usize] as u64,
            regs[libc::REG_RSP as usize] as u64,
            regs[libc::REG_RBP as usize] as u64,
        ),
        regs[libc::REG_RIP as usize] as u64,
    ))
}

const CACHE_POOL_SIZE: usize = 64;

static UNWINDER: Lazy<UnwinderNative<shlib::SectionData, MustNotAllocateDuringUnwind>> =
    Lazy::new(|| {
        let mut unwinder = UnwinderNative::new();
        for obj in shlib::get_objects() {
            unwinder.add_module(obj.clone());
        }
        unwinder
    });

static CACHE_POOL: Lazy<[Mutex<CacheNative<MustNotAllocateDuringUnwind>>; CACHE_POOL_SIZE]> =
    Lazy::new(|| std::array::from_fn(|_| Mutex::new(CacheNative::default())));

fn read_stack(
    addr: u64,
    page_cache: &mut crate::addr_validate::StackPageValidator,
) -> Result<u64, ()> {
    let aligned_addr = (addr & !0b111) as usize;
    if page_cache.validate_range(aligned_addr, std::mem::size_of::<u64>()) {
        Ok(unsafe { (aligned_addr as *const u64).read() })
    } else {
        Err(())
    }
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub ip: usize,
}

unsafe extern "C" {
    fn _Unwind_FindEnclosingFunction(pc: *mut c_void) -> *mut c_void;
}

impl super::Frame for Frame {
    type S = backtrace::Symbol;
    fn ip(&self) -> usize {
        self.ip
    }

    fn symbol_address(&self) -> *mut c_void {
        if cfg!(target_os = "macos") || cfg!(target_os = "ios") {
            self.ip as *mut c_void
        } else {
            unsafe { _Unwind_FindEnclosingFunction(self.ip as *mut c_void) }
        }
    }

    fn resolve_symbol<F: FnMut(&Self::S)>(&self, cb: F) {
        backtrace::resolve(self.ip as *mut c_void, cb);
    }
}

pub struct Trace;

impl super::Trace for Trace {
    type Frame = Frame;

    fn init() {
        let _ = &*UNWINDER;
        let _ = &*CACHE_POOL;
    }

    fn trace<F: FnMut(&Self::Frame) -> bool>(ctx: *mut c_void, mut cb: F)
    where
        Self: Sized,
    {
        let Some((regs, pc)) = get_regs_from_context(ctx) else {
            return;
        };

        let tid = unsafe { libc::pthread_self() as usize };
        let start = (tid >> 4).wrapping_mul(0x9e37_79b9_7f4a_7c15) % CACHE_POOL_SIZE;

        for offset in 0..CACHE_POOL_SIZE {
            if let Some(mut cache) = CACHE_POOL[(start + offset) % CACHE_POOL_SIZE].try_lock() {
                let mut page_cache = crate::addr_validate::StackPageValidator::default();
                let mut closure = |addr| read_stack(addr, &mut page_cache);
                let mut iter = UNWINDER.iter_frames(pc, regs, &mut cache, &mut closure);
                while let Ok(Some(frame)) = iter.next() {
                    if !cb(&Frame {
                        ip: frame.address() as usize,
                    }) {
                        break;
                    }
                }
                return;
            }
        }
    }
}
