//! Хранилище торрента в оперативной памяти: данные не касаются диска.
//! Загруженный кусок отправляется в облако и тут же освобождается (`free`).
use std::{
    collections::HashMap,
    path::Path,
    sync::{atomic::{AtomicU64, Ordering::Relaxed}, Arc, Mutex},
};

use anyhow::{bail, Result};
use librqbit::{
    storage::{BoxStorageFactory, StorageFactory, StorageFactoryExt, TorrentStorage},
    ManagedTorrentShared, TorrentMetadata,
};

const SEG: u64 = 1 << 20; // 1 МиБ

/// Всего занято оперативной памяти всеми хранилищами Clouter.
pub static RAM_USED: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
pub struct RamStore {
    segs: Mutex<HashMap<(usize, u64), Box<[u8]>>>,
}

impl RamStore {
    /// Освобождает сегменты, целиком лежащие внутри [start, start+len) файла file.
    pub fn free(&self, file: usize, start: u64, len: u64) {
        let mut g = self.segs.lock().unwrap();
        let (first, end) = (start.div_ceil(SEG), (start + len) / SEG);
        for s in first..end {
            if g.remove(&(file, s)).is_some() { RAM_USED.fetch_sub(SEG, Relaxed); }
        }
    }
    /// Сколько байт сейчас лежит в памяти у этой загрузки.
    pub fn bytes(&self) -> u64 { self.segs.lock().unwrap().len() as u64 * SEG }
    pub fn clear(&self) {
        let mut g = self.segs.lock().unwrap();
        RAM_USED.fetch_sub(g.len() as u64 * SEG, Relaxed);
        g.clear();
    }
}

impl Drop for RamStore { fn drop(&mut self) { self.clear(); } }

#[derive(Clone)]
pub struct RamFactory(pub Arc<RamStore>);

impl StorageFactory for RamFactory {
    type Storage = RamStorage;
    fn create(&self, _s: &ManagedTorrentShared, _m: &TorrentMetadata) -> Result<RamStorage> {
        Ok(RamStorage(self.0.clone()))
    }
    fn clone_box(&self) -> BoxStorageFactory { self.clone().boxed() }
}

pub struct RamStorage(Arc<RamStore>);

impl TorrentStorage for RamStorage {
    fn init(&mut self, _s: &ManagedTorrentShared, _m: &TorrentMetadata) -> Result<()> { Ok(()) }

    fn pread_exact(&self, file: usize, offset: u64, buf: &mut [u8]) -> Result<()> {
        let g = self.0.segs.lock().unwrap();
        let (mut off, mut done) = (offset, 0usize);
        while done < buf.len() {
            let (seg, inner) = (off / SEG, (off % SEG) as usize);
            let n = (SEG as usize - inner).min(buf.len() - done);
            match g.get(&(file, seg)) {
                Some(s) => buf[done..done + n].copy_from_slice(&s[inner..inner + n]),
                None => bail!("data already released from memory"),
            }
            off += n as u64; done += n;
        }
        Ok(())
    }

    fn pwrite_all(&self, file: usize, offset: u64, buf: &[u8]) -> Result<()> {
        let mut g = self.0.segs.lock().unwrap();
        let (mut off, mut done) = (offset, 0usize);
        while done < buf.len() {
            let (seg, inner) = (off / SEG, (off % SEG) as usize);
            let n = (SEG as usize - inner).min(buf.len() - done);
            let s = g.entry((file, seg)).or_insert_with(|| {
                RAM_USED.fetch_add(SEG, Relaxed);
                vec![0u8; SEG as usize].into_boxed_slice()
            });
            s[inner..inner + n].copy_from_slice(&buf[done..done + n]);
            off += n as u64; done += n;
        }
        Ok(())
    }

    fn remove_file(&self, _f: usize, _p: &Path) -> Result<()> { Ok(()) }
    fn remove_directory_if_empty(&self, _p: &Path) -> Result<()> { Ok(()) }
    fn ensure_file_length(&self, _f: usize, _l: u64) -> Result<()> { Ok(()) }
    fn take(&self) -> Result<Box<dyn TorrentStorage>> { Ok(Box::new(RamStorage(self.0.clone()))) }
}

/// Объём оперативной памяти Mac в байтах.
pub fn physical_memory() -> u64 {
    let mut v: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let name = std::ffi::CString::new("hw.memsize").unwrap();
    unsafe {
        libc::sysctlbyname(name.as_ptr(), &mut v as *mut u64 as *mut libc::c_void, &mut len, std::ptr::null_mut(), 0);
    }
    v
}
