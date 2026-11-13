// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::collections::hash_map::DefaultHasher;
use std::convert::TryInto;
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::frames::UnresolvedFrames;

use aligned_vec::AVec;
use spin::Mutex;
use tempfile::NamedTempFile;

pub const BUCKETS: usize = 1 << 12;
pub const BUCKETS_ASSOCIATIVITY: usize = 4;
pub(crate) const NUM_SHARDS: usize = 16;
pub(crate) const BUCKETS_PER_SHARD: usize = BUCKETS / NUM_SHARDS;
const RAW_BUFFER_LENGTH: usize = (1 << 18) / std::mem::size_of::<Entry<UnresolvedFrames>>();
pub const BUFFER_LENGTH: usize = if RAW_BUFFER_LENGTH == 0 {
    1
} else {
    RAW_BUFFER_LENGTH
};

#[derive(Debug, Clone)]
pub struct Entry<T> {
    pub item: T,
    pub count: isize,
}

impl<T: Default> Default for Entry<T> {
    fn default() -> Self {
        Entry {
            item: Default::default(),
            count: 0,
        }
    }
}

#[derive(Debug)]
pub struct Bucket<T: 'static> {
    pub length: usize,
    entries: Box<[Entry<T>; BUCKETS_ASSOCIATIVITY]>,
}

impl<T: Eq + Default> Default for Bucket<T> {
    fn default() -> Bucket<T> {
        let entries = Box::default();

        Self { length: 0, entries }
    }
}

impl<T: Eq> Bucket<T> {
    pub fn add(&mut self, key: T, count: isize) -> Option<Entry<T>> {
        let mut done = false;
        self.entries[0..self.length].iter_mut().for_each(|ele| {
            if ele.item == key {
                ele.count += count;
                done = true;
            }
        });

        if done {
            None
        } else if self.length < BUCKETS_ASSOCIATIVITY {
            let ele = &mut self.entries[self.length];
            ele.item = key;
            ele.count = count;

            self.length += 1;
            None
        } else {
            let mut min_index = 0;
            let mut min_count = self.entries[0].count;
            for index in 0..self.length {
                let count = self.entries[index].count;
                if count < min_count {
                    min_index = index;
                    min_count = count;
                }
            }

            let mut new_entry = Entry { item: key, count };
            std::mem::swap(&mut self.entries[min_index], &mut new_entry);
            Some(new_entry)
        }
    }

    /// Inserts or updates `key` when external spill storage is at capacity,
    /// retaining heavier hitters in the bucket and returning the number of
    /// dropped samples (`0` when absorbed in-memory).
    pub(crate) fn add_capped(&mut self, key: T, count: isize) -> isize {
        for ele in &mut self.entries[0..self.length] {
            if ele.item == key {
                ele.count += count;
                return 0;
            }
        }

        if self.length < BUCKETS_ASSOCIATIVITY {
            let ele = &mut self.entries[self.length];
            ele.item = key;
            ele.count = count;
            self.length += 1;
            return 0;
        }

        let mut min_index = 0;
        let mut min_count = self.entries[0].count;
        for index in 1..self.length {
            let c = self.entries[index].count;
            if c < min_count {
                min_index = index;
                min_count = c;
            }
        }

        if count > min_count {
            self.entries[min_index] = Entry { item: key, count };
            min_count.max(0)
        } else {
            count.max(0)
        }
    }

    pub fn iter(&self) -> BucketIterator<'_, T> {
        BucketIterator::<T> {
            related_bucket: self,
            index: 0,
        }
    }
}

pub struct BucketIterator<'a, T: 'static> {
    related_bucket: &'a Bucket<T>,
    index: usize,
}

impl<'a, T> Iterator for BucketIterator<'a, T> {
    type Item = &'a Entry<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.related_bucket.length {
            self.index += 1;
            Some(&self.related_bucket.entries[self.index - 1])
        } else {
            None
        }
    }
}

pub struct HashCounter<T: Hash + Eq + 'static> {
    buckets: Box<[Bucket<T>; BUCKETS]>,
}

impl<T: Hash + Eq + Default + Debug> Default for HashCounter<T> {
    fn default() -> Self {
        let mut v: Vec<Bucket<T>> = Vec::with_capacity(BUCKETS);
        v.resize_with(BUCKETS, Default::default);
        let buckets = v.into_boxed_slice().try_into().unwrap();

        Self { buckets }
    }
}

impl<T: Hash + Eq> HashCounter<T> {
    fn hash(key: &T) -> u64 {
        let mut s = DefaultHasher::new();
        key.hash(&mut s);
        s.finish()
    }

    pub fn add(&mut self, key: T, count: isize) -> Option<Entry<T>> {
        let hash_value = Self::hash(&key);
        let bucket = &mut self.buckets[(hash_value % BUCKETS as u64) as usize];

        bucket.add(key, count)
    }

    pub(crate) fn add_capped(&mut self, key: T, count: isize) -> isize {
        let hash_value = Self::hash(&key);
        let bucket = &mut self.buckets[(hash_value % BUCKETS as u64) as usize];

        bucket.add_capped(key, count)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry<T>> {
        self.buckets.iter().flat_map(|bucket| bucket.iter())
    }
}

pub struct TempFdArray<T: 'static> {
    file: Option<NamedTempFile>,
    buffer: Box<[T; BUFFER_LENGTH]>,
    buffer_index: usize,
    flush_n: usize,
    max_entries: Option<usize>,
}

impl<T: Default + Debug> TempFdArray<T> {
    #[cfg(test)]
    fn new() -> std::io::Result<TempFdArray<T>> {
        Self::with_max_entries(None)
    }

    fn with_max_entries(max_entries: Option<usize>) -> std::io::Result<TempFdArray<T>> {
        let file = if max_entries == Some(0) {
            None
        } else {
            Some(NamedTempFile::new()?)
        };

        let mut v: Vec<T> = Vec::with_capacity(BUFFER_LENGTH);
        v.resize_with(BUFFER_LENGTH, Default::default);
        let buffer = v.into_boxed_slice().try_into().unwrap();

        Ok(Self {
            file,
            buffer,
            buffer_index: 0,
            flush_n: 0,
            max_entries,
        })
    }
}

impl<T> TempFdArray<T> {
    fn set_max_entries(&mut self, max_entries: Option<usize>) -> std::io::Result<()> {
        self.buffer_index = 0;
        self.flush_n = 0;
        self.max_entries = max_entries;

        if max_entries == Some(0) {
            self.file = None;
        } else if let Some(file) = self.file.as_mut() {
            file.as_file_mut().set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
        } else {
            self.file = Some(NamedTempFile::new()?);
        }

        Ok(())
    }

    fn total_entries(&self) -> usize {
        self.flush_n * BUFFER_LENGTH + self.buffer_index
    }

    fn flushed_bytes(&self) -> usize {
        self.flush_n * BUFFER_LENGTH * std::mem::size_of::<T>()
    }

    fn is_full(&self) -> bool {
        self.max_entries
            .is_some_and(|max| self.total_entries() >= max)
    }

    fn flush_buffer(&mut self) -> std::io::Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Err(std::io::Error::other("spill file disabled"));
        };
        let entry_size = std::mem::size_of::<T>();
        let expected_offset = (self.flush_n * BUFFER_LENGTH * entry_size) as u64;
        let buf = unsafe {
            std::slice::from_raw_parts(
                self.buffer.as_ptr() as *const u8,
                BUFFER_LENGTH * entry_size,
            )
        };
        if let Err(err) = file.write_all(buf) {
            let _ = file.seek(SeekFrom::Start(expected_offset));
            let _ = file.as_file_mut().set_len(expected_offset);
            return Err(err);
        }
        self.buffer_index = 0;
        self.flush_n += 1;

        Ok(())
    }

    fn push(&mut self, entry: T) -> std::io::Result<()> {
        if self.buffer_index >= BUFFER_LENGTH {
            self.flush_buffer()?;
        }

        self.buffer[self.buffer_index] = entry;
        self.buffer_index += 1;

        Ok(())
    }

    fn try_iter(&self) -> std::io::Result<TempFdArrayIterator<'_, T>> {
        let flushed_entries = self.flush_n * BUFFER_LENGTH;
        let entry_size = std::mem::size_of::<T>();
        let chunk_capacity_entries = if flushed_entries == 0 || entry_size == 0 {
            0
        } else {
            ((1 << 18) / entry_size)
                .clamp(1, BUFFER_LENGTH)
                .min(flushed_entries)
        };

        let file = if flushed_entries > 0 {
            let mut f = self
                .file
                .as_ref()
                .ok_or_else(|| std::io::Error::other("missing spill file"))?
                .reopen()?;
            f.seek(SeekFrom::Start(0))?;
            Some(f)
        } else {
            None
        };

        let chunk_bytes = chunk_capacity_entries * entry_size;
        let mut chunk_buf = AVec::with_capacity(std::mem::align_of::<T>(), chunk_bytes);
        if chunk_bytes > 0 {
            chunk_buf.resize(chunk_bytes, 0);
        }

        Ok(TempFdArrayIterator {
            buffer: &self.buffer[0..self.buffer_index],
            buffer_pos: 0,
            file,
            remaining_flushed: flushed_entries,
            chunk_capacity_entries,
            chunk_buf,
            chunk_len: 0,
            chunk_pos: 0,
        })
    }
}

pub struct TempFdArrayIterator<'a, T> {
    buffer: &'a [T],
    buffer_pos: usize,
    file: Option<std::fs::File>,
    remaining_flushed: usize,
    chunk_capacity_entries: usize,
    chunk_buf: AVec<u8>,
    chunk_len: usize,
    chunk_pos: usize,
}

impl<'a, T> TempFdArrayIterator<'a, T> {
    pub(crate) fn next_ref(&mut self) -> std::io::Result<Option<&T>> {
        if self.buffer_pos < self.buffer.len() {
            let item = &self.buffer[self.buffer_pos];
            self.buffer_pos += 1;
            return Ok(Some(item));
        }

        if self.chunk_pos >= self.chunk_len {
            if self.remaining_flushed == 0 {
                return Ok(None);
            }
            let to_read = self.remaining_flushed.min(self.chunk_capacity_entries);
            let bytes_to_read = to_read * std::mem::size_of::<T>();
            if bytes_to_read > 0 {
                let file = self
                    .file
                    .as_mut()
                    .ok_or_else(|| std::io::Error::other("missing spill file"))?;
                file.read_exact(&mut self.chunk_buf[0..bytes_to_read])?;
            }
            self.remaining_flushed -= to_read;
            self.chunk_len = to_read;
            self.chunk_pos = 0;
        }

        let slice = unsafe {
            std::slice::from_raw_parts(self.chunk_buf.as_ptr() as *const T, self.chunk_len)
        };
        let item = &slice[self.chunk_pos];
        self.chunk_pos += 1;
        Ok(Some(item))
    }
}

impl<'a, T: Clone> Iterator for TempFdArrayIterator<'a, T> {
    type Item = std::io::Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_ref().map(|opt| opt.cloned()).transpose()
    }
}

struct LegacySpillIter<'a, T> {
    buffer: &'a [T],
    file_vec: AVec<u8>,
    index: usize,
}

impl<'a, T> Iterator for LegacySpillIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.buffer.len() {
            self.index += 1;
            Some(&self.buffer[self.index - 1])
        } else {
            let entry_size = std::mem::size_of::<T>();
            let length = self.file_vec.len().checked_div(entry_size).unwrap_or(0);
            let ts =
                unsafe { std::slice::from_raw_parts(self.file_vec.as_ptr() as *const T, length) };
            if self.index - self.buffer.len() < ts.len() {
                self.index += 1;
                Some(&ts[self.index - self.buffer.len() - 1])
            } else {
                None
            }
        }
    }
}

pub struct Collector<T: Hash + Eq + 'static> {
    map: HashCounter<T>,
    temp_array: TempFdArray<Entry<T>>,
    dropped_samples: u64,
}

impl<T: Hash + Eq + Default + Debug + 'static> Collector<T> {
    pub fn new() -> std::io::Result<Self> {
        Self::with_max_spill_entries(None)
    }

    /// Creates a `Collector` whose spill storage is bounded by `max_spill_bytes`.
    pub fn with_max_spill_bytes(max_spill_bytes: Option<usize>) -> std::io::Result<Self> {
        let entry_size = std::mem::size_of::<Entry<T>>().max(1);
        let max_entries = max_spill_bytes.map(|bytes| bytes / entry_size);
        Self::with_max_spill_entries(max_entries)
    }

    /// Creates a `Collector` whose spilled entry count is bounded by `max_spill_entries`.
    pub fn with_max_spill_entries(max_spill_entries: Option<usize>) -> std::io::Result<Self> {
        Ok(Self {
            map: HashCounter::<T>::default(),
            temp_array: TempFdArray::<Entry<T>>::with_max_entries(max_spill_entries)?,
            dropped_samples: 0,
        })
    }
}

impl<T: Hash + Eq + 'static> Collector<T> {
    /// Returns the number of samples dropped because the spill cap (or spill I/O) was reached.
    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples
    }

    /// Returns the total number of evicted entries currently stored in spill buffers/file.
    pub fn spilled_entries(&self) -> usize {
        self.temp_array.total_entries()
    }

    /// Returns the total number of bytes flushed to the temporary spill file on disk.
    pub fn flushed_spill_bytes(&self) -> usize {
        self.temp_array.flushed_bytes()
    }

    pub fn add(&mut self, key: T, count: isize) -> std::io::Result<()> {
        if self.temp_array.is_full() {
            let dropped = self.map.add_capped(key, count);
            if dropped > 0 {
                self.dropped_samples += dropped as u64;
            }
            return Ok(());
        }

        if let Some(evict) = self.map.add(key, count) {
            let evict_count = evict.count.max(0) as u64;
            if let Err(err) = self.temp_array.push(evict) {
                self.dropped_samples += evict_count;
                return Err(err);
            }
        }

        Ok(())
    }

    /// Streams all in-memory and spilled entries in bounded 256 KiB chunks without
    /// allocating the full spill file in memory.
    pub fn try_for_each<F>(&self, mut f: F) -> std::io::Result<()>
    where
        F: FnMut(&Entry<T>),
    {
        for entry in self.map.iter() {
            f(entry);
        }
        let mut spill = self.temp_array.try_iter()?;
        while let Some(entry) = spill.next_ref()? {
            f(entry);
        }
        Ok(())
    }

    pub fn try_iter(&self) -> std::io::Result<impl Iterator<Item = &Entry<T>>> {
        let entry_size = std::mem::size_of::<Entry<T>>();
        let flushed_bytes = self.temp_array.flushed_bytes();
        let mut file_vec = AVec::with_capacity(std::mem::align_of::<Entry<T>>(), flushed_bytes);
        let mut spill = self.temp_array.try_iter()?;
        // Advance past in-memory tail; `LegacySpillIter` reads `self.temp_array.buffer` directly.
        spill.buffer_pos = spill.buffer.len();
        while let Some(entry) = spill.next_ref()? {
            let bytes = unsafe {
                std::slice::from_raw_parts(entry as *const Entry<T> as *const u8, entry_size)
            };
            file_vec.extend_from_slice(bytes);
        }

        Ok(self.map.iter().chain(LegacySpillIter {
            buffer: &self.temp_array.buffer[0..self.temp_array.buffer_index],
            file_vec,
            index: 0,
        }))
    }
}

pub(crate) enum ConcurrentAddOutcome {
    Recorded,
    DroppedSpill(u64),
    Contended(u64),
}

/// 16-shard concurrent collector used by the signal-handler profiler so
/// multiple CPU cores can unwind and record samples concurrently under a
/// shared read lock without contending on a single global table lock.
pub(crate) struct ShardedCollector<T: Hash + Eq + 'static> {
    shards: [Mutex<Box<[Bucket<T>; BUCKETS_PER_SHARD]>>; NUM_SHARDS],
    spill: Mutex<TempFdArray<Entry<T>>>,
    spill_full: AtomicBool,
    dropped_samples: AtomicU64,
}

impl<T: Hash + Eq + Default + Debug + 'static> ShardedCollector<T> {
    pub(crate) fn new() -> std::io::Result<Self> {
        let shards = std::array::from_fn(|_| {
            let mut v: Vec<Bucket<T>> = Vec::with_capacity(BUCKETS_PER_SHARD);
            v.resize_with(BUCKETS_PER_SHARD, Default::default);
            let boxed: Box<[Bucket<T>; BUCKETS_PER_SHARD]> =
                v.into_boxed_slice().try_into().unwrap();
            Mutex::new(boxed)
        });
        Ok(Self {
            shards,
            spill: Mutex::new(TempFdArray::with_max_entries(None)?),
            spill_full: AtomicBool::new(false),
            dropped_samples: AtomicU64::new(0),
        })
    }
}

impl<T: Hash + Eq + 'static> ShardedCollector<T> {
    pub(crate) fn configure_max_spill_entries(
        &mut self,
        max_spill_entries: Option<usize>,
    ) -> std::io::Result<()> {
        for shard in &mut self.shards {
            for bucket in shard.get_mut().iter_mut() {
                bucket.length = 0;
            }
        }
        self.dropped_samples.store(0, Ordering::Relaxed);
        self.spill_full
            .store(max_spill_entries == Some(0), Ordering::Relaxed);
        self.spill.get_mut().set_max_entries(max_spill_entries)
    }

    pub(crate) fn reset(&mut self) -> std::io::Result<()> {
        self.configure_max_spill_entries(None)
    }

    pub(crate) fn dropped_samples(&self) -> u64 {
        self.dropped_samples.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn spilled_entries(&self) -> usize {
        self.spill.lock().total_entries()
    }

    pub(crate) fn try_add(&self, key: T, count: isize) -> ConcurrentAddOutcome {
        let hash_value = HashCounter::<T>::hash(&key);
        let shard_idx = ((hash_value >> 32) as usize) & (NUM_SHARDS - 1);
        let bucket_idx = (hash_value as usize) & (BUCKETS_PER_SHARD - 1);

        let mut shard_guard = None;
        for _ in 0..32 {
            if let Some(g) = self.shards[shard_idx].try_lock() {
                shard_guard = Some(g);
                break;
            }
            std::hint::spin_loop();
        }
        let Some(mut shard) = shard_guard else {
            return ConcurrentAddOutcome::Contended(count.max(0) as u64);
        };

        if self.spill_full.load(Ordering::Relaxed) {
            let dropped = shard[bucket_idx].add_capped(key, count);
            drop(shard);
            if dropped > 0 {
                let d = dropped as u64;
                self.dropped_samples.fetch_add(d, Ordering::Relaxed);
                return ConcurrentAddOutcome::DroppedSpill(d);
            }
            return ConcurrentAddOutcome::Recorded;
        }

        let evict = shard[bucket_idx].add(key, count);
        drop(shard);

        if let Some(evict) = evict {
            let evict_count = evict.count.max(0) as u64;
            let mut spill_guard = None;
            for _ in 0..256 {
                if let Some(g) = self.spill.try_lock() {
                    spill_guard = Some(g);
                    break;
                }
                std::hint::spin_loop();
            }
            let Some(mut spill) = spill_guard else {
                return ConcurrentAddOutcome::Contended(evict_count);
            };

            if spill.is_full() {
                self.spill_full.store(true, Ordering::Relaxed);
                self.dropped_samples
                    .fetch_add(evict_count, Ordering::Relaxed);
                return ConcurrentAddOutcome::DroppedSpill(evict_count);
            }

            if spill.push(evict).is_err() {
                self.dropped_samples
                    .fetch_add(evict_count, Ordering::Relaxed);
                return ConcurrentAddOutcome::DroppedSpill(evict_count);
            }
            if spill.is_full() {
                self.spill_full.store(true, Ordering::Relaxed);
            }
        }

        ConcurrentAddOutcome::Recorded
    }

    pub(crate) fn try_for_each<F>(&self, mut f: F) -> std::io::Result<()>
    where
        F: FnMut(&Entry<T>),
    {
        for shard_mutex in &self.shards {
            let shard = shard_mutex.lock();
            for bucket in shard.iter() {
                for entry in bucket.iter() {
                    f(entry);
                }
            }
        }
        let spill = self.spill.lock();
        let mut iter = spill.try_iter()?;
        while let Some(entry) = iter.next_ref()? {
            f(entry);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test_utils {
    use super::*;
    use std::collections::BTreeMap;

    pub fn add_map<T: std::cmp::Ord + Copy>(hashmap: &mut BTreeMap<T, isize>, entry: &Entry<T>) {
        match hashmap.get_mut(&entry.item) {
            None => {
                hashmap.insert(entry.item, entry.count);
            }
            Some(count) => *count += entry.count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn stack_hash_counter() {
        let mut stack_hash_counter = HashCounter::<usize>::default();
        stack_hash_counter.add(0, 1);
        stack_hash_counter.add(1, 1);
        stack_hash_counter.add(1, 1);

        stack_hash_counter.iter().for_each(|item| {
            if item.item == 0 {
                assert_eq!(item.count, 1);
            } else if item.item == 1 {
                assert_eq!(item.count, 2);
            } else {
                unreachable!();
            }
        });
    }

    #[test]
    fn evict_test() {
        let mut stack_hash_counter = HashCounter::<usize>::default();
        let mut real_map = BTreeMap::new();

        for item in 0..(1 << 10) * 4 {
            for _ in 0..(item % 4) {
                match stack_hash_counter.add(item, 1) {
                    None => {}
                    Some(evict) => {
                        test_utils::add_map(&mut real_map, &evict);
                    }
                }
            }
        }

        stack_hash_counter.iter().for_each(|entry| {
            test_utils::add_map(&mut real_map, entry);
        });

        for item in 0..(1 << 10) * 4 {
            let count = (item % 4) as isize;
            match real_map.get(&item) {
                Some(item) => {
                    assert_eq!(*item, count);
                }
                None => {
                    assert_eq!(count, 0);
                }
            }
        }
    }

    #[test]
    fn collector_test() {
        let mut collector = Collector::new().unwrap();
        let mut real_map = BTreeMap::new();

        for item in 0..(1 << 12) * 4 {
            for _ in 0..(item % 4) {
                collector.add(item, 1).unwrap();
            }
        }

        collector.try_iter().unwrap().for_each(|entry| {
            test_utils::add_map(&mut real_map, entry);
        });

        for item in 0..(1 << 12) * 4 {
            let count = (item % 4) as isize;
            match real_map.get(&item) {
                Some(value) => {
                    assert_eq!(count, *value);
                }
                None => {
                    assert_eq!(count, 0);
                }
            }
        }
    }

    #[derive(Debug, Hash, Eq, PartialEq, PartialOrd, Ord, Default, Clone, Copy)]
    struct AlignTest {
        a: u16,
        b: u32,
        c: u64,
        d: u64,
    }

    // collector_align_test uses a bigger item to test the alignment of the collector
    #[test]
    fn collector_align_test() {
        let mut collector = Collector::new().unwrap();
        let mut real_map = BTreeMap::new();

        for item in 0..(1 << 12) * 4 {
            for _ in 0..(item % 4) {
                collector
                    .add(
                        AlignTest {
                            a: item as u16,
                            b: item as u32,
                            c: item as u64,
                            d: item as u64,
                        },
                        1,
                    )
                    .unwrap();
            }
        }

        collector.try_iter().unwrap().for_each(|entry| {
            test_utils::add_map(&mut real_map, entry);
        });

        for item in 0..(1 << 12) * 4 {
            let count = (item % 4) as isize;
            let align_item = AlignTest {
                a: item as u16,
                b: item as u32,
                c: item as u64,
                d: item as u64,
            };
            match real_map.get(&align_item) {
                Some(value) => {
                    assert_eq!(count, *value);
                }
                None => {
                    assert_eq!(count, 0);
                }
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    struct BigPayload([u64; 128]);

    impl Default for BigPayload {
        fn default() -> Self {
            Self([0; 128])
        }
    }

    impl Debug for BigPayload {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "BigPayload({})", self.0[0])
        }
    }

    #[test]
    fn temp_fd_array_multi_chunk_spillover_and_bounded_alloc() {
        let _serial = crate::profiler::tests::TEST_PROFILER_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let mut temp = TempFdArray::<Entry<BigPayload>>::new().unwrap();
        let entry_size = std::mem::size_of::<Entry<BigPayload>>();
        // Ensure we flush at least 4 chunks (> 1 MiB total > 256 KiB chunk size) plus an unflushed tail.
        let min_entries_for_1mib = (1 << 20) / entry_size + BUFFER_LENGTH;
        let total_entries = (BUFFER_LENGTH * 4).max(min_entries_for_1mib) + 37;

        let mut expected_sum: isize = 0;
        for i in 0..total_entries {
            let count = ((i % 7) + 1) as isize;
            expected_sum += count;
            temp.push(Entry {
                item: BigPayload([i as u64; 128]),
                count,
            })
            .unwrap();
        }

        assert!(
            temp.flushed_bytes() > (1 << 18) * 3,
            "expected multi-chunk spill > 768 KiB, got {} bytes",
            temp.flushed_bytes()
        );

        crate::profiler::tests::ALLOC.start_byte_tracking();
        let mut streamed_count = 0usize;
        let mut streamed_sum: isize = 0;
        let mut iter = temp.try_iter().unwrap();
        while let Some(entry) = iter.next_ref().unwrap() {
            assert_eq!(entry.item.0[0], entry.item.0[127]);
            streamed_count += 1;
            streamed_sum += entry.count;
        }
        drop(iter);
        let (_peak_bytes, max_single_alloc) = crate::profiler::tests::ALLOC.stop_byte_tracking();

        assert_eq!(streamed_count, total_entries);
        assert_eq!(streamed_sum, expected_sum);
        assert!(
            max_single_alloc <= (1 << 18),
            "expected chunk allocation <= 256 KiB, got {} bytes (flushed file was {} bytes)",
            max_single_alloc,
            temp.flushed_bytes()
        );

        // Also exercise the standard `Iterator` implementation across all chunks.
        let iter_sum: isize = temp.try_iter().unwrap().map(|res| res.unwrap().count).sum();
        assert_eq!(iter_sum, expected_sum);
    }

    #[test]
    fn collector_spill_cap_enforcement_and_accounting() {
        // Case 1: zero-byte spill cap (strictly in-memory only, zero `/tmp` writes).
        let mut zero_spill = Collector::<BigPayload>::with_max_spill_bytes(Some(0)).unwrap();
        let total_zero_inputs: usize = BUCKETS * BUCKETS_ASSOCIATIVITY + 5_000;
        for i in 0..total_zero_inputs {
            zero_spill.add(BigPayload([i as u64; 128]), 1).unwrap();
        }
        assert_eq!(zero_spill.spilled_entries(), 0);
        assert_eq!(zero_spill.flushed_spill_bytes(), 0);
        assert!(zero_spill.dropped_samples() > 0);

        let mut retained_zero: isize = 0;
        zero_spill
            .try_for_each(|entry| retained_zero += entry.count)
            .unwrap();
        assert_eq!(
            retained_zero as u64 + zero_spill.dropped_samples(),
            total_zero_inputs as u64
        );

        // Case 2: bounded 300 KiB spill cap (> 1 chunk, strictly bounded).
        let cap_bytes = 300 * 1024;
        let mut capped = Collector::<BigPayload>::with_max_spill_bytes(Some(cap_bytes)).unwrap();
        let mut total_added: u64 = 0;
        // Establish heavy hitters first, then flood with cold keys to verify conservation and heavy-hitter retention.
        for hot in 0..32u64 {
            capped.add(BigPayload([hot; 128]), 50).unwrap();
            total_added += 50;
        }
        for cold in 1_000..(1_000 + BUCKETS * BUCKETS_ASSOCIATIVITY + 10_000) {
            capped.add(BigPayload([cold as u64; 128]), 1).unwrap();
            total_added += 1;
        }

        assert!(capped.flushed_spill_bytes() <= cap_bytes);
        assert!(capped.spilled_entries() * std::mem::size_of::<Entry<BigPayload>>() <= cap_bytes);
        assert!(capped.dropped_samples() > 0);

        let mut retained_counts = BTreeMap::<u64, isize>::new();
        let mut retained_sum: isize = 0;
        capped
            .try_for_each(|entry| {
                retained_sum += entry.count;
                *retained_counts.entry(entry.item.0[0]).or_insert(0) += entry.count;
            })
            .unwrap();

        assert_eq!(retained_sum as u64 + capped.dropped_samples(), total_added);
        for hot in 0..32u64 {
            assert_eq!(
                retained_counts.get(&hot).copied(),
                Some(50),
                "heavy hitter {hot} should be retained when cold tail overflows spill cap"
            );
        }
    }

    #[test]
    fn sharded_collector_concurrent_multithread_conservation() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;

        let collector = Arc::new(ShardedCollector::<u64>::new().unwrap());
        let missed = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));

        const THREADS: usize = 8;
        const SAMPLES_PER_THREAD: u64 = 4_000;

        let mut handles = Vec::with_capacity(THREADS);
        for t in 0..THREADS {
            let c = Arc::clone(&collector);
            let m = Arc::clone(&missed);
            let d = Arc::clone(&dropped);
            handles.push(std::thread::spawn(move || {
                for i in 0..SAMPLES_PER_THREAD {
                    let key = (t as u64) * 10_000 + (i % 2_500);
                    match c.try_add(key, 1) {
                        ConcurrentAddOutcome::Recorded => {}
                        ConcurrentAddOutcome::DroppedSpill(n) => {
                            d.fetch_add(n, Ordering::Relaxed);
                        }
                        ConcurrentAddOutcome::Contended(n) => {
                            m.fetch_add(n, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let mut retained: i64 = 0;
        collector
            .try_for_each(|entry| retained += entry.count as i64)
            .unwrap();
        let total_accounted =
            retained as u64 + dropped.load(Ordering::Relaxed) + missed.load(Ordering::Relaxed);
        assert_eq!(total_accounted, THREADS as u64 * SAMPLES_PER_THREAD);
    }
}
