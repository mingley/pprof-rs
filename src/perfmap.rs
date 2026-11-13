use std::{
    fs::File,
    io::BufRead,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use arc_swap::ArcSwap;
use once_cell::sync::Lazy;

use crate::Symbol;

#[derive(Debug)]
pub struct PerfMap {
    ranges: Vec<(usize, usize, String)>,
}

impl PerfMap {
    pub fn new(file: File) -> Option<Self> {
        let reader = std::io::BufReader::new(file);
        let mut ranges = Vec::new();
        for line in reader.lines() {
            let line = line.ok()?;
            // The format of perf map is:
            // <start addr> <len addr> <name>
            // where <start addr> and <len addr> are hexadecimal numbers
            // and <name> may contain spaces.
            let mut parts = line.split_whitespace();
            let Some(start_str) = parts.next() else {
                continue;
            };
            let Some(len_str) = parts.next() else {
                continue;
            };
            let Ok(start) = usize::from_str_radix(start_str.trim_start_matches("0x"), 16) else {
                continue;
            };
            let Ok(len) = usize::from_str_radix(len_str.trim_start_matches("0x"), 16) else {
                continue;
            };
            if len == 0 {
                continue;
            }
            let name = parts.collect::<Vec<_>>().join(" ");
            ranges.push((start, start.saturating_add(len), name));
        }
        ranges.sort_unstable_by_key(|(start, _, _)| *start);
        Some(Self { ranges })
    }

    pub fn find(&self, addr: usize) -> Option<PerfMapSymbol> {
        let idx = self.ranges.partition_point(|(start, _, _)| *start <= addr);
        for &(start, end, ref name) in self.ranges[..idx].iter().rev() {
            if start <= addr && addr < end {
                return Some(PerfMapSymbol(name.clone()));
            }
            if end <= addr {
                break;
            }
        }
        None
    }
}

#[derive(Debug)]
pub struct PerfMapSymbol(String);

impl From<PerfMapSymbol> for Symbol {
    fn from(value: PerfMapSymbol) -> Self {
        Symbol {
            name: Some(value.0.into_bytes()),
            addr: None,
            filename: None,
            lineno: None,
        }
    }
}

static EPOCH: Lazy<Instant> = Lazy::new(Instant::now);
static LAST_CHECK_SECS: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_FINGERPRINT: AtomicU64 = AtomicU64::new(0);

fn perf_map_path() -> PathBuf {
    PathBuf::from("/tmp").join(format!("perf-{}.map", std::process::id()))
}

fn load_if_changed(path: &Path) -> Option<Option<PerfMap>> {
    let meta = match path.metadata() {
        Ok(m) => m,
        Err(_) => {
            if LAST_FINGERPRINT.swap(0, Ordering::Relaxed) != 0 {
                return Some(None);
            }
            return None;
        }
    };
    let mtime_nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    let fingerprint = mtime_nanos.rotate_left(17) ^ meta.len().wrapping_add(1);
    if LAST_FINGERPRINT.load(Ordering::Relaxed) == fingerprint {
        return None;
    }
    let file = File::open(path).ok()?;
    let parsed = PerfMap::new(file);
    LAST_FINGERPRINT.store(fingerprint, Ordering::Relaxed);
    Some(parsed)
}

pub fn get_resolver() -> Arc<Option<PerfMap>> {
    static RESOLVER: Lazy<ArcSwap<Option<PerfMap>>> = Lazy::new(|| {
        let initial = load_if_changed(&perf_map_path()).flatten();
        LAST_CHECK_SECS.store(EPOCH.elapsed().as_secs(), Ordering::Relaxed);
        ArcSwap::from(Arc::new(initial))
    });

    let resolver = &*RESOLVER;
    let now_secs = EPOCH.elapsed().as_secs();
    let prev_secs = LAST_CHECK_SECS.load(Ordering::Relaxed);
    if now_secs != prev_secs
        && LAST_CHECK_SECS
            .compare_exchange(prev_secs, now_secs, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        && let Some(updated) = load_if_changed(&perf_map_path())
    {
        resolver.store(Arc::new(updated));
    }

    resolver.load_full()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn perfmap_parses_unsorted_hex_and_binary_searches_accurately() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "2000 100 jit_second_fn").unwrap();
        writeln!(tmp).unwrap();
        writeln!(tmp, "0x1000 0x40 jit first fn with spaces").unwrap();
        writeln!(tmp, "malformed line").unwrap();
        tmp.flush().unwrap();

        let map = PerfMap::new(tmp.reopen().unwrap()).unwrap();
        assert!(map.find(0x0fff).is_none());
        assert_eq!(
            Symbol::from(map.find(0x1000).unwrap()).name(),
            "jit first fn with spaces"
        );
        assert_eq!(
            Symbol::from(map.find(0x103f).unwrap()).name(),
            "jit first fn with spaces"
        );
        assert!(map.find(0x1040).is_none());
        assert_eq!(
            Symbol::from(map.find(0x2080).unwrap()).name(),
            "jit_second_fn"
        );
        assert!(map.find(0x2100).is_none());
    }
}
