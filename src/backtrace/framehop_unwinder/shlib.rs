use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use findshlibs::{SharedLibrary, TargetSharedLibrary};
use framehop::{Module, ModuleSectionInfo};
use memmap2::Mmap;
use object::{Object, ObjectSection};
use once_cell::sync::Lazy;

#[derive(Clone)]
pub enum SectionData {
    Mapped {
        mmap: Arc<Mmap>,
        offset: usize,
        len: usize,
    },
    Owned(Arc<[u8]>),
}

impl Deref for SectionData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Mapped { mmap, offset, len } => &mmap[*offset..*offset + *len],
            Self::Owned(buf) => buf,
        }
    }
}

static OBJECTS: Lazy<Vec<Module<SectionData>>> = Lazy::new(find_objects);

pub fn get_objects() -> &'static [Module<SectionData>] {
    &OBJECTS
}

pub struct ObjectInfo<'f> {
    obj: object::File<'f>,
    mmap: Arc<Mmap>,
}

impl<'file> ModuleSectionInfo<SectionData> for ObjectInfo<'file> {
    fn base_svma(&self) -> u64 {
        if let Some(section) = self.obj.section_by_name("__TEXT") {
            // in mach-o addresses are relative to __TEXT
            section.address()
        } else {
            self.obj.relative_address_base()
        }
    }

    fn section_svma_range(&mut self, name: &[u8]) -> Option<std::ops::Range<u64>> {
        if let Some(section) = self.obj.section_by_name_bytes(name) {
            let start = section.address();
            let end = start + section.size();
            Some(start..end)
        } else {
            None
        }
    }

    fn section_data(&mut self, name: &[u8]) -> Option<SectionData> {
        let section = self.obj.section_by_name_bytes(name)?;
        let data = section.data().ok()?;
        let base = self.mmap.as_ptr() as usize;
        let end = base.saturating_add(self.mmap.len());
        let ptr = data.as_ptr() as usize;
        if ptr >= base && ptr.saturating_add(data.len()) <= end {
            Some(SectionData::Mapped {
                mmap: Arc::clone(&self.mmap),
                offset: ptr - base,
                len: data.len(),
            })
        } else {
            Some(SectionData::Owned(Arc::from(data)))
        }
    }
}

fn open_mmap(path: &Path, is_first: bool) -> Option<Arc<Mmap>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        #[cfg(target_os = "linux")]
        Err(_) if is_first => std::fs::File::open("/proc/self/exe").ok()?,
        Err(_) => {
            let _ = is_first;
            return None;
        }
    };
    let mmap = unsafe { Mmap::map(&file) }.ok()?;
    Some(Arc::new(mmap))
}

fn find_objects() -> Vec<Module<SectionData>> {
    let mut objects = Vec::new();
    let mut is_first = true;
    TargetSharedLibrary::each(|shlib| {
        let first = is_first;
        is_first = false;
        let path = PathBuf::from(shlib.name());
        let base_avma = shlib.actual_load_addr().0 as u64;
        let avma_range = base_avma..base_avma + shlib.len() as u64;
        if let Some(mmap) = open_mmap(&path, first)
            && let Ok(obj) = object::File::parse(&**mmap)
        {
            let section_info = ObjectInfo {
                obj,
                mmap: Arc::clone(&mmap),
            };

            objects.push(Module::new(
                path.to_string_lossy().into_owned(),
                avma_range,
                base_avma,
                section_info,
            ));
        }
    });

    objects
}
