// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::{self, Debug, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use smallvec::SmallVec;
use symbolic_demangle::demangle;

use crate::backtrace::{Frame, Trace, TraceImpl};
use crate::{MAX_DEPTH, MAX_THREAD_NAME};

#[cfg(feature = "perfmaps")]
fn resolve_in_perfmap(ip: usize) -> Option<Symbol> {
    use crate::perfmap::get_resolver;

    if let Some(perf_map_resolver) = get_resolver().as_ref()
        && let Some(symbol) = perf_map_resolver.find(ip as _)
    {
        return Some(Symbol::from(symbol));
    }

    None
}

#[cfg(not(feature = "perfmaps"))]
fn resolve_in_perfmap(_ip: usize) -> Option<Symbol> {
    None
}

#[derive(Clone)]
pub struct UnresolvedFrames {
    pub frames: SmallVec<[<TraceImpl as Trace>::Frame; MAX_DEPTH]>,
    pub thread_name: [u8; MAX_THREAD_NAME],
    pub thread_name_length: usize,
    pub thread_id: u64,
    pub sample_timestamp: SystemTime,
}

impl Default for UnresolvedFrames {
    fn default() -> Self {
        let frames = SmallVec::with_capacity(MAX_DEPTH);
        Self {
            frames,
            thread_name: [0; MAX_THREAD_NAME],
            thread_name_length: 0,
            thread_id: 0,
            sample_timestamp: SystemTime::now(),
        }
    }
}

impl Debug for UnresolvedFrames {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        self.frames.fmt(f)
    }
}

impl UnresolvedFrames {
    pub fn new(
        frames: SmallVec<[<TraceImpl as Trace>::Frame; MAX_DEPTH]>,
        tn: &[u8],
        thread_id: u64,
        sample_timestamp: SystemTime,
    ) -> Self {
        let thread_name_length = tn.len();
        let mut thread_name = [0; MAX_THREAD_NAME];
        thread_name[0..thread_name_length].clone_from_slice(tn);

        Self {
            frames,
            thread_name,
            thread_name_length,
            thread_id,
            sample_timestamp,
        }
    }

    #[cfg(feature = "_protobuf")]
    pub(crate) fn thread_name_or_id(&self) -> String {
        if self.thread_name_length > 0 {
            String::from_utf8_lossy(&self.thread_name[0..self.thread_name_length]).into_owned()
        } else {
            format!("{:?}", self.thread_id)
        }
    }
}

impl PartialEq for UnresolvedFrames {
    fn eq(&self, other: &Self) -> bool {
        let (frames1, frames2) = (&self.frames, &other.frames);
        if self.thread_id != other.thread_id || frames1.len() != frames2.len() {
            false
        } else {
            Iterator::zip(frames1.iter(), frames2.iter())
                .all(|(s1, s2)| Frame::ip(s1) == Frame::ip(s2))
        }
    }
}

impl Eq for UnresolvedFrames {}

impl Hash for UnresolvedFrames {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.frames
            .iter()
            .for_each(|frame| Frame::ip(frame).hash(state));
        self.thread_id.hash(state);
    }
}

/// Symbol is a representation of a function symbol. It contains name and addr of it. If built with
/// debug message, it can also provide line number and filename. The name in it is not demangled.
#[derive(Debug, Clone)]
pub struct Symbol {
    /// This name is raw name of a symbol (which hasn't been demangled).
    pub name: Option<Vec<u8>>,

    /// The address of the function. It is not 100% trustworthy.
    pub addr: Option<*mut c_void>,

    /// Line number of this symbol. If compiled with debug message, you can get it.
    pub lineno: Option<u32>,

    /// Filename of this symbol. If compiled with debug message, you can get it.
    pub filename: Option<PathBuf>,
}

impl Symbol {
    pub fn raw_name(&self) -> &[u8] {
        self.name.as_deref().unwrap_or(b"Unknown")
    }

    pub fn name(&self) -> String {
        demangle(&String::from_utf8_lossy(self.raw_name())).into_owned()
    }

    pub fn sys_name(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(self.raw_name())
    }

    pub fn filename(&self) -> Cow<'_, str> {
        self.filename
            .as_ref()
            .map(|name| name.as_os_str().to_string_lossy())
            .unwrap_or_else(|| Cow::Borrowed("Unknown"))
    }

    pub fn lineno(&self) -> u32 {
        self.lineno.unwrap_or(0)
    }
}

unsafe impl Send for Symbol {}

impl<T> From<&T> for Symbol
where
    T: crate::backtrace::Symbol,
{
    fn from(symbol: &T) -> Self {
        Symbol {
            name: symbol.name(),
            addr: symbol.addr(),
            lineno: symbol.lineno(),
            filename: symbol.filename(),
        }
    }
}

impl Display for Symbol {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.write_str(&self.name())
    }
}

impl PartialEq for Symbol {
    fn eq(&self, other: &Self) -> bool {
        self.raw_name() == other.raw_name()
    }
}

impl Hash for Symbol {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.raw_name().hash(state)
    }
}

pub(crate) fn is_signal_handler_symbol(symbol: &Symbol) -> bool {
    // macOS prepends an underscore even with `#[no_mangle]`
    matches!(
        &*symbol.name(),
        "perf_signal_handler" | "_perf_signal_handler"
    )
}

pub(crate) fn resolve_frame_symbols<F: Frame>(frame: &F, out: &mut Vec<Symbol>) {
    out.clear();
    let ip = Frame::ip(frame);
    if let Some(perfmap_symbol) = resolve_in_perfmap(ip) {
        out.push(perfmap_symbol);
    } else {
        frame.resolve_symbol(|symbol| {
            out.push(Symbol::from(symbol));
        });
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CachedSymbols {
    pub(crate) symbols: Arc<[Symbol]>,
    pub(crate) is_signal_handler: bool,
}

/// Per-report-build memoization table mapping instruction pointers (`usize`)
/// to shared `Arc<[Symbol]>` slices so each unique PC is symbolized at most once.
#[derive(Default)]
pub(crate) struct SymbolCache {
    cache: HashMap<usize, CachedSymbols>,
    scratch: Vec<Symbol>,
    #[cfg(test)]
    pub(crate) misses: usize,
    #[cfg(test)]
    pub(crate) hits: usize,
}

impl SymbolCache {
    pub(crate) fn resolve_ip_with<R>(&mut self, ip: usize, resolve_fn: R) -> &CachedSymbols
    where
        R: FnOnce(usize, &mut Vec<Symbol>),
    {
        if self.cache.contains_key(&ip) {
            #[cfg(test)]
            {
                self.hits += 1;
            }
            return self.cache.get(&ip).unwrap();
        }

        #[cfg(test)]
        {
            self.misses += 1;
        }
        self.scratch.clear();
        resolve_fn(ip, &mut self.scratch);
        let is_signal_handler = self.scratch.iter().any(is_signal_handler_symbol);
        let symbols: Arc<[Symbol]> = Arc::from(self.scratch.as_slice());
        self.cache.entry(ip).or_insert(CachedSymbols {
            symbols,
            is_signal_handler,
        })
    }

    #[cfg(test)]
    pub(crate) fn get_cached(&self, ip: usize) -> Option<&Arc<[Symbol]>> {
        self.cache.get(&ip).map(|c| &c.symbols)
    }
}

/// A representation of a backtrace. `thread_name` and `thread_id` was got from `pthread_getname_np`
/// and `pthread_self`. frames is a vector of symbols.
#[derive(Clone, PartialEq, Hash)]
pub struct Frames {
    pub frames: Vec<Vec<Symbol>>,
    pub thread_name: String,
    pub thread_id: u64,
    pub sample_timestamp: SystemTime,
}

impl Frames {
    /// Returns a thread identifier (name or ID) as a string.
    pub fn thread_name_or_id(&self) -> String {
        if !self.thread_name.is_empty() {
            self.thread_name.clone()
        } else {
            format!("{:?}", self.thread_id)
        }
    }

    pub(crate) fn from_unresolved_with_cache(
        frames: &UnresolvedFrames,
        cache: &mut SymbolCache,
    ) -> Self {
        Self::from_unresolved_with_resolver(frames, cache, resolve_frame_symbols)
    }

    pub(crate) fn from_unresolved_with_resolver<R>(
        frames: &UnresolvedFrames,
        cache: &mut SymbolCache,
        mut resolve_fn: R,
    ) -> Self
    where
        R: FnMut(&<TraceImpl as Trace>::Frame, &mut Vec<Symbol>),
    {
        let mut fs = Vec::with_capacity(frames.frames.len());
        let mut frame_iter = frames.frames.iter();

        while let Some(frame) = frame_iter.next() {
            let ip = Frame::ip(frame);
            let cached = cache.resolve_ip_with(ip, |_, out| resolve_fn(frame, out));
            if cached.is_signal_handler {
                // ignore frame itself and its next one
                frame_iter.next();
                continue;
            }

            if !cached.symbols.is_empty() {
                fs.push(cached.symbols.as_ref().to_vec());
            }
        }

        Self {
            frames: fs,
            thread_name: String::from_utf8_lossy(&frames.thread_name[0..frames.thread_name_length])
                .into_owned(),
            thread_id: frames.thread_id,
            sample_timestamp: frames.sample_timestamp,
        }
    }
}

impl From<UnresolvedFrames> for Frames {
    fn from(frames: UnresolvedFrames) -> Self {
        let mut cache = SymbolCache::default();
        Self::from_unresolved_with_cache(&frames, &mut cache)
    }
}

impl Eq for Frames {}

impl Debug for Frames {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        for frame in self.frames.iter() {
            write!(f, "FRAME: ")?;
            for symbol in frame.iter() {
                write!(f, "{} -> ", symbol)?;
            }
        }
        write!(f, "THREAD: ")?;
        if !self.thread_name.is_empty() {
            write!(f, "{}", self.thread_name)
        } else {
            write!(f, "ThreadId({})", self.thread_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demangle_rust() {
        let symbol = Symbol {
            name: Some(b"_ZN3foo3barE".to_vec()),
            addr: None,
            lineno: None,
            filename: None,
        };

        assert_eq!(&symbol.name(), "foo::bar")
    }

    #[test]
    fn demangle_cpp() {
        let name =
            b"_ZNK3MapI10StringName3RefI8GDScriptE10ComparatorIS0_E16DefaultAllocatorE3hasERKS0_"
                .to_vec();

        let symbol = Symbol {
            name: Some(name),
            addr: None,
            lineno: None,
            filename: None,
        };

        assert_eq!(
            &symbol.name(),
            "Map<StringName, Ref<GDScript>, Comparator<StringName>, DefaultAllocator>::has(StringName const&) const"
        )
    }

    #[test]
    fn symbol_cache_memoizes_pcs_and_shares_arc_allocations() {
        let mut cache = SymbolCache::default();
        let mut resolve_calls = 0usize;

        let mock_resolve = |ip: usize, out: &mut Vec<Symbol>| {
            resolve_calls += 1;
            match ip {
                0x100 => {
                    out.push(Symbol {
                        name: Some(b"inlined_leaf".to_vec()),
                        addr: None,
                        lineno: Some(12),
                        filename: Some(PathBuf::from("src/leaf.rs")),
                    });
                    out.push(Symbol {
                        name: Some(b"outer_caller".to_vec()),
                        addr: None,
                        lineno: Some(44),
                        filename: Some(PathBuf::from("src/outer.rs")),
                    });
                }
                0x200 => {
                    out.push(Symbol {
                        name: Some(b"_perf_signal_handler".to_vec()),
                        addr: None,
                        lineno: Some(1),
                        filename: None,
                    });
                }
                other => {
                    out.push(Symbol {
                        name: Some(format!("fn_{other:x}").into_bytes()),
                        addr: None,
                        lineno: Some(other as u32),
                        filename: Some(PathBuf::from("src/lib.rs")),
                    });
                }
            }
        };

        let mut mock = mock_resolve;
        for &pc in &[0x100usize, 0x300, 0x100, 0x200, 0x300, 0x100] {
            let _ = cache.resolve_ip_with(pc, &mut mock);
        }

        assert_eq!(resolve_calls, 3, "each unique PC must resolve only once");
        assert_eq!(cache.misses, 3);
        assert_eq!(cache.hits, 3);

        let arc_a = Arc::clone(cache.get_cached(0x100).unwrap());
        let cached_again = cache.resolve_ip_with(0x100, |_, _| {
            panic!("cached PC should not invoke resolver again");
        });
        assert!(Arc::ptr_eq(&arc_a, &cached_again.symbols));
        assert_eq!(cached_again.symbols.len(), 2);
        assert!(!cached_again.is_signal_handler);

        let sig = cache.resolve_ip_with(0x200, |_, _| unreachable!());
        assert!(sig.is_signal_handler);
    }

    #[cfg(any(feature = "framehop-unwinder", feature = "frame-pointer"))]
    #[test]
    fn frames_from_unresolved_uses_pc_cache_across_distinct_stacks() {
        use crate::backtrace::Trace;
        type RawFrame = <TraceImpl as Trace>::Frame;

        let mut cache = SymbolCache::default();
        let stack_a = UnresolvedFrames::new(
            smallvec::smallvec![
                RawFrame { ip: 0x10 }, // signal handler -> skips self + next (0x20)
                RawFrame { ip: 0x20 }, // trampoline (never even symbolized!)
                RawFrame { ip: 0x30 }, // inlined pair
                RawFrame { ip: 0x40 }, // shared root
            ],
            b"worker-1",
            1,
            SystemTime::UNIX_EPOCH,
        );
        let stack_b = UnresolvedFrames::new(
            smallvec::smallvec![
                RawFrame { ip: 0x50 }, // distinct leaf
                RawFrame { ip: 0x30 }, // shared inlined pair (cache hit)
                RawFrame { ip: 0x40 }, // shared root (cache hit)
            ],
            b"worker-2",
            2,
            SystemTime::UNIX_EPOCH,
        );

        let mut resolve = |frame: &RawFrame, out: &mut Vec<Symbol>| {
            let pc = crate::backtrace::Frame::ip(frame);
            match pc {
                0x10 => out.push(Symbol {
                    name: Some(b"perf_signal_handler".to_vec()),
                    addr: None,
                    lineno: None,
                    filename: None,
                }),
                0x20 => panic!("trampoline frame after signal handler must be skipped"),
                0x30 => {
                    out.push(Symbol {
                        name: Some(b"inline_inner".to_vec()),
                        addr: None,
                        lineno: Some(7),
                        filename: Some(PathBuf::from("a.rs")),
                    });
                    out.push(Symbol {
                        name: Some(b"inline_outer".to_vec()),
                        addr: None,
                        lineno: Some(19),
                        filename: Some(PathBuf::from("a.rs")),
                    });
                }
                other => out.push(Symbol {
                    name: Some(format!("func_{other:x}").into_bytes()),
                    addr: None,
                    lineno: Some(1),
                    filename: Some(PathBuf::from("b.rs")),
                }),
            }
        };

        let resolved_a = Frames::from_unresolved_with_resolver(&stack_a, &mut cache, &mut resolve);
        let resolved_b = Frames::from_unresolved_with_resolver(&stack_b, &mut cache, &mut resolve);

        assert_eq!(
            cache.misses, 4,
            "unique PCs resolved: 0x10, 0x30, 0x40, 0x50"
        );
        assert_eq!(
            cache.hits, 2,
            "shared PCs 0x30 and 0x40 hit cache on stack_b"
        );
        assert_eq!(resolved_a.frames.len(), 2);
        assert_eq!(resolved_a.frames[0].len(), 2);
        assert_eq!(resolved_b.frames.len(), 3);
    }
}
