use std::{
    mem::size_of,
    sync::atomic::{AtomicI32, Ordering},
};

use spin::Mutex;

// This module talks to the OS through `libc` directly rather than through `nix`.
// `validate` runs inside the profiler's signal handler, so the calls here must be
// async-signal-safe (raw `read`/`write`/`pipe2`/`close` and reading `errno`
// qualify). Using `libc` also decouples this crate from the churn in `nix`'s
// file-descriptor API, letting `nix` be depended on with a broad version range.

struct Pipes {
    read_fd: AtomicI32,
    write_fd: AtomicI32,
}

static MEM_VALIDATE_PIPE: Pipes = Pipes {
    read_fd: AtomicI32::new(-1),
    write_fd: AtomicI32::new(-1),
};

static PIPE_INIT_LOCK: Mutex<()> = Mutex::new(());

const CHECK_LENGTH: usize = 2 * size_of::<*const libc::c_void>() / size_of::<u8>();
const PAGE_MASK: usize = !0xfff;

/// Returns the current `errno`. Reading it is async-signal-safe and does not
/// allocate.
#[inline]
fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[inline]
#[cfg(any(target_os = "android", target_os = "linux"))]
fn create_pipe() -> Result<(i32, i32), i32> {
    let mut fds = [0 as libc::c_int; 2];
    // Safety: `fds` points to an array of two `c_int`s, as `pipe2` requires. The
    // fds are intentionally leaked: these pipes live for the whole program lifetime
    // and their raw fds are stored in atomics so they can be used from the signal handler.
    let res = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if res != 0 {
        return Err(errno());
    }
    Ok((fds[0], fds[1]))
}

#[inline]
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
fn create_pipe() -> Result<(i32, i32), i32> {
    fn set_flags(fd: libc::c_int) -> Result<(), i32> {
        // Safety: `fd` is a valid fd returned by `pipe`.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
                return Err(errno());
            }
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(errno());
            }
        }
        Ok(())
    }

    let mut fds = [0 as libc::c_int; 2];
    // Safety: `fds` points to an array of two `c_int`s, as `pipe` requires.
    let res = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if res != 0 {
        return Err(errno());
    }
    if let Err(err) = set_flags(fds[0]).and_then(|()| set_flags(fds[1])) {
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        return Err(err);
    }
    Ok((fds[0], fds[1]))
}

fn reopen_pipe_locked() -> Option<(i32, i32)> {
    let old_read = MEM_VALIDATE_PIPE.read_fd.swap(-1, Ordering::AcqRel);
    let old_write = MEM_VALIDATE_PIPE.write_fd.swap(-1, Ordering::AcqRel);
    unsafe {
        if old_read >= 0 {
            libc::close(old_read);
        }
        if old_write >= 0 {
            libc::close(old_write);
        }
    }

    let (read_fd, write_fd) = create_pipe().ok()?;
    MEM_VALIDATE_PIPE.read_fd.store(read_fd, Ordering::Release);
    MEM_VALIDATE_PIPE
        .write_fd
        .store(write_fd, Ordering::Release);
    Some((read_fd, write_fd))
}

#[inline]
fn ensure_pipe_open() -> Option<(i32, i32)> {
    let write_fd = MEM_VALIDATE_PIPE.write_fd.load(Ordering::Acquire);
    let read_fd = MEM_VALIDATE_PIPE.read_fd.load(Ordering::Acquire);
    if read_fd >= 0 && write_fd >= 0 {
        return Some((read_fd, write_fd));
    }

    let _guard = PIPE_INIT_LOCK.lock();
    let write_fd = MEM_VALIDATE_PIPE.write_fd.load(Ordering::Acquire);
    let read_fd = MEM_VALIDATE_PIPE.read_fd.load(Ordering::Acquire);
    if read_fd >= 0 && write_fd >= 0 {
        return Some((read_fd, write_fd));
    }
    reopen_pipe_locked()
}

/// Pre-initializes the validation pipe outside signal context.
pub(crate) fn init() {
    let _ = ensure_pipe_open();
}

#[inline]
fn drain_pipe(read_fd: i32) -> bool {
    let mut buf = [0u8; 512];
    loop {
        let ret = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if ret >= 0 {
            return ret > 0;
        }
        match errno() {
            libc::EINTR => continue,
            libc::EAGAIN => return true,
            _ => return false,
        }
    }
}

// validate whether the address `addr` is readable through `write()` to a pipe
//
// if the second argument of `write(ptr, buf)` is not a valid address, the
// `write()` will return an error the error number should be `EFAULT` in most
// cases, but we regard all errors (except EINTR/EAGAIN) as a failure of validation
//
// `addr` is handed straight to `libc::write` and never dereferenced in Rust (in
// particular we deliberately avoid `slice::from_raw_parts`, which would be UB for
// an invalid `addr`), so keeping this a safe function is sound.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn validate(addr: *const libc::c_void) -> bool {
    if addr.is_null() {
        return false;
    }

    let Some((mut read_fd, mut write_fd)) = ensure_pipe_open() else {
        return false;
    };

    let mut reopened = false;
    loop {
        // Safety: `write_fd` is a valid non-blocking pipe fd. `addr` is passed
        // straight to `write` as the source buffer: whether it is readable is
        // what the kernel checks (returning EFAULT instead of faulting).
        let ret = unsafe { libc::write(write_fd, addr, CHECK_LENGTH) };
        if ret >= 0 {
            return ret > 0;
        }
        match errno() {
            libc::EINTR => continue,
            libc::EAGAIN => {
                if !drain_pipe(read_fd) {
                    return false;
                }
            }
            libc::EBADF if !reopened => {
                reopened = true;
                let Some(_guard) = PIPE_INIT_LOCK.try_lock() else {
                    return false;
                };
                let Some((r, w)) = reopen_pipe_locked() else {
                    return false;
                };
                read_fd = r;
                write_fd = w;
            }
            _ => return false,
        }
    }
}

/// Stack-local 4 KiB page validation cache scoped to a single unwind invocation.
/// Eliminates repeated `write()` syscalls when reading multiple stack words on
/// the same mapped stack page.
#[derive(Default)]
#[allow(dead_code)]
pub(crate) struct StackPageValidator {
    pages: [usize; 8],
    len: usize,
    cursor: usize,
}

#[allow(dead_code)]
impl StackPageValidator {
    #[inline]
    pub(crate) fn validate_range(&mut self, addr: usize, size: usize) -> bool {
        if addr == 0 {
            return false;
        }
        let start_page = addr & PAGE_MASK;
        let end_page = match addr.checked_add(size.saturating_sub(1)) {
            Some(end) => end & PAGE_MASK,
            None => return false,
        };

        if start_page == end_page && start_page != 0 {
            for &p in &self.pages[..self.len] {
                if p == start_page {
                    return true;
                }
            }
        }

        if validate(addr as *const libc::c_void) {
            if start_page == end_page && start_page != 0 {
                if self.len < self.pages.len() {
                    self.pages[self.len] = start_page;
                    self.len += 1;
                } else {
                    self.pages[self.cursor] = start_page;
                    self.cursor = (self.cursor + 1) % self.pages.len();
                }
            }
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn validate_stack() {
        let i = 0;

        assert!(validate(&i as *const _ as *const libc::c_void));
    }

    #[test]
    fn validate_heap() {
        let vec = vec![0; 1000];

        for i in vec.iter() {
            assert!(validate(i as *const _ as *const libc::c_void));
        }
    }

    #[test]
    fn failed_validate() {
        assert!(!validate(std::ptr::null::<libc::c_void>()));
        assert!(!validate(-1_i32 as usize as *const libc::c_void))
    }

    #[test]
    fn validate_pipe_auto_drains_across_many_calls_and_page_cache_bounds() {
        let buf = vec![42u64; 1024];
        // Exceed 64 KiB kernel pipe capacity (> 4,096 × 16-byte writes) to exercise EAGAIN auto-drain.
        for i in 0..6_000 {
            let ptr = &buf[i % buf.len()] as *const u64 as *const libc::c_void;
            assert!(validate(ptr));
        }
        assert!(!validate(std::ptr::null::<libc::c_void>()));

        let mut cache = StackPageValidator::default();
        let stack_val = 99u64;
        let addr = &stack_val as *const u64 as usize;
        assert!(cache.validate_range(addr, 8));
        assert!(cache.validate_range(addr, 8)); // cached page hit
        assert!(!cache.validate_range(0, 8));
        assert!(!cache.validate_range(usize::MAX - 3, 8));
    }
}
