//! Googletor core: torrent -> destination, streaming, bounded disk usage.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::{bail, Context, Result};
use librqbit::{AddTorrent, AddTorrentOptions, ManagedTorrent, Session};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub mod accounts;
pub mod dropbox;
pub mod oauth;
pub mod ramstore;
pub mod onedrive;
pub mod jobstate;
pub mod magnet;
pub mod drive;

/// Размер одной порции. Кратно 256 КиБ (требование Drive).
pub const CHUNK: usize = 60 * 1024 * 1024; // кратно 256 КиБ (Google) и 320 КиБ (OneDrive)

#[derive(Clone)]
pub enum Dest {
    Local(PathBuf),
    /// Аккаунт и id папки назначения (None = корень).
    Drive(Arc<drive::DriveAuth>, Option<String>),
    Dropbox(Arc<dropbox::Dropbox>, Option<String>),
    OneDrive(Arc<onedrive::OneDrive>, Option<String>),
}

#[derive(Default)]
pub struct JobProgress {
    pub name: Mutex<String>,
    pub status: Mutex<String>,
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub rate_bps: AtomicU64,
    pub peers: AtomicU64,
    pub peers_seen: AtomicU64,
    /// Сохраняемое состояние загрузки (для продолжения после закрытия приложения или сбоя).
    pub state: Mutex<Option<jobstate::JobState>>,
    /// Сколько байт уже закончено в предыдущих файлах раздачи (основа для полосы «Скачано»).
    pub base_done: AtomicU64,
    /// Сколько занимает кэш этой загрузки сейчас, предел окна и режим «в памяти».
    pub cache_bytes: AtomicU64,
    pub cache_cap: AtomicU64,
    pub cache_ram: AtomicBool,
    /// id торрента в общем движке (+1; 0 — ещё не добавлен), нужен для удаления после завершения.
    pub torrent_id: AtomicU64,
    /// Пауза по кнопке пользователя.
    pub paused: AtomicBool,
    /// id папок и файлов, созданных в облаке этой загрузкой (чистятся при отмене).
    pub created: Mutex<Vec<String>>,
    /// Адрес незавершённой загрузки в Google Drive (чтобы прервать при отмене).
    pub session: Mutex<Option<String>>,
    /// Имя, которое уже занято в папке назначения (пусто, если конфликта нет).
    pub conflict: Mutex<String>,
    /// Выбор пользователя: 0 — ждём, 1 — заменить, 2 — переименовать, 3 — отмена.
    pub decision: std::sync::atomic::AtomicU8,
    /// Сколько байт уже скачано из торрента (в пределах окна), всего по раздаче.
    pub dl_done: AtomicU64,
    /// Скорость отправки в облако, байт/с (по последнему куску).
    pub up_bps: AtomicU64,
    pub file_idx: AtomicU64,
    pub file_count: AtomicU64,
    pub cancel: AtomicBool,
    /// Файл не помещается на диск: нужен режим больших файлов (Google Drive API).
    pub needs_api: AtomicBool,
}

impl JobProgress {
    pub fn set_status(&self, s: &str) {
        *self.status.lock().unwrap() = s.to_string();
    }
    pub fn line(&self) -> String {
        format!(
            "[{}] {} {:.2}/{:.2} GB  {:.1} MB/s",
            self.status.lock().unwrap(),
            self.name.lock().unwrap(),
            self.done.load(Relaxed) as f64 / 1e9,
            self.total.load(Relaxed) as f64 / 1e9,
            self.rate_bps.load(Relaxed) as f64 / 1e6,
        )
    }
    fn check(&self) -> Result<()> {
        if self.cancel.load(Relaxed) {
            bail!("cancelled");
        }
        Ok(())
    }
}

/// Освобождает место под уже переданным диапазоном (APFS).
#[cfg(target_os = "macos")]
fn punch_hole(path: &Path, start: u64, len: u64) {
    use std::os::fd::AsRawFd;
    const BLK: u64 = 4096;
    let s = start.div_ceil(BLK) * BLK;
    let e = (start + len) / BLK * BLK;
    if e <= s {
        return;
    }
    #[repr(C)]
    struct FPunchHole {
        flags: u32,
        reserved: u32,
        offset: i64,
        length: i64,
    }
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        let arg = FPunchHole { flags: 0, reserved: 0, offset: s as i64, length: (e - s) as i64 };
        // F_PUNCHHOLE = 99
        unsafe { libc::fcntl(f.as_raw_fd(), 99, &arg as *const FPunchHole) };
    }
}
#[cfg(not(target_os = "macos"))]
fn punch_hole(_: &Path, _: u64, _: u64) {}

/// Приёмник одного файла.
enum FileSink {
    Local { f: tokio::fs::File },
    Drive(drive::ResumableUpload),
    Dropbox(dropbox::Upload),
    OneDrive(onedrive::Upload),
}

impl FileSink {
    async fn open(dest: &Dest, rel: &Path, size: u64) -> Result<Self> {
        match dest {
            Dest::Local(root) => {
                let p = root.join(rel);
                tokio::fs::create_dir_all(p.parent().unwrap()).await?;
                Ok(FileSink::Local { f: tokio::fs::File::create(p).await? })
            }
            Dest::Drive(auth, base) => Ok(FileSink::Drive(
                drive::ResumableUpload::start(auth.clone(), rel, size, base.clone()).await?,
            )),
            Dest::Dropbox(a, base) => Ok(FileSink::Dropbox(dropbox::Upload::start(a.clone(), rel, size, base.clone()).await?)),
            Dest::OneDrive(a, base) => Ok(FileSink::OneDrive(onedrive::Upload::start(a.clone(), rel, size, base.clone()).await?)),
        }
    }
    /// Папки и файл, созданные этим приёмником в облаке (для отмены).
    fn created(&self) -> Vec<String> {
        match self {
            FileSink::Drive(u) => u.new_folders.iter().cloned().chain(u.file_id.clone()).collect(),
            _ => vec![],
        }
    }
    fn session_url(&self) -> Option<String> {
        match self { FileSink::Drive(u) => Some(u.session_url.clone()), _ => None }
    }
    async fn put(&mut self, offset: u64, data: Vec<u8>) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        match self {
            FileSink::Local { f } => Ok(f.write_all(&data).await?),
            FileSink::Drive(u) => u.put(offset, data).await,
            FileSink::Dropbox(u) => u.put(offset, data).await,
            FileSink::OneDrive(u) => u.put(offset, data).await,
        }
    }
}

/// У macOS лимит открытых файлов по умолчанию 256, а торрент с тысячами файлов
/// открывает их все при инициализации. Поднимаем лимит насколько позволит система.
pub fn raise_fd_limit() {
    unsafe {
        let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        for target in [65536u64, 10240, 4096, 2048] {
            let want = if lim.rlim_max == libc::RLIM_INFINITY { target } else { target.min(lim.rlim_max) };
            let new = libc::rlimit { rlim_cur: want, rlim_max: lim.rlim_max };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &new) == 0 {
                return;
            }
        }
    }
}

/// Режим «через оперативную память» (только для облаков по API): данные не пишутся на диск.
static USE_RAM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static RAM_MB: AtomicU64 = AtomicU64::new(512);
pub fn set_ram_mode(on: bool, mb: u64) { USE_RAM.store(on, Relaxed); RAM_MB.store(mb.clamp(128, 8192), Relaxed); }
pub fn ram_mode() -> bool { USE_RAM.load(Relaxed) }
pub fn ram_used() -> u64 { ramstore::RAM_USED.load(Relaxed) }

/// Один торрент-движок на всё приложение: общий DHT и порт. Каждая загрузка добавляет в него свой торрент.
static SESSION: tokio::sync::OnceCell<Arc<Session>> = tokio::sync::OnceCell::const_new();

async fn shared_session() -> Result<Arc<Session>> {
    let s = SESSION.get_or_try_init(|| async {
        let dir = std::env::temp_dir().join(format!("clouter-{}-session", std::process::id()));
        tokio::fs::create_dir_all(&dir).await?;
        // Свободный порт для DHT выбирает система: не конфликтуем с другими клиентами и копиями.
        let port = std::net::UdpSocket::bind("0.0.0.0:0")?.local_addr()?.port();
        Session::new_with_opts(dir, librqbit::SessionOptions {
            dht: Some(librqbit::DhtSessionConfig { bootstrap_addrs: None, port: Some(port), persistence: None }),
            ..Default::default()
        }).await
    }).await?;
    Ok(s.clone())
}

/// Лимит одновременных торрентов (меняется в настройках) и счётчик запущенных.
static MAX_PAR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
static RUNNING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static JOB_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn set_max_parallel(n: usize) { MAX_PAR.store(n.clamp(1, 8), Relaxed); }
pub fn max_parallel() -> usize { MAX_PAR.load(Relaxed) }
pub fn running_count() -> usize { RUNNING.load(Relaxed) }

struct Slot;
impl Drop for Slot { fn drop(&mut self) { RUNNING.fetch_sub(1, Relaxed); } }

/// Реально занятое место (блоки на диске, а не длина файла): учитывает вырезанные «дыры».
fn alloc_size(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(md) = std::fs::symlink_metadata(p) else { return 0 };
    if md.is_dir() {
        std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| alloc_size(&e.path())).sum()).unwrap_or(0)
    } else { md.blocks() * 512 }
}

/// Останавливает фоновый таймер статистики, когда загрузка закончилась (в том числе с ошибкой).
struct StatsGuard(Arc<AtomicBool>);
impl Drop for StatsGuard { fn drop(&mut self) { self.0.store(true, Relaxed); } }

/// Размер кэша одной загрузки: в памяти или реально занятые блоки на диске.
fn job_cache(ram: &Option<Arc<ramstore::RamStore>>, work: &Path) -> u64 {
    match ram { Some(st) => st.bytes(), None => alloc_size(work) }
}

/// Сколько места сейчас занимает временный кэш всех загрузок Clouter.
pub fn cache_usage() -> u64 {
    let prefix = format!("clouter-{}-", std::process::id());
    std::fs::read_dir(std::env::temp_dir()).map(|rd| rd.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .map(|e| alloc_size(&e.path())).sum()).unwrap_or(0)
}

/// Ручная очистка кэша из настроек: только когда загрузки не идут. Возвращает, сколько байт освобождено.
pub fn clear_cache_if_idle() -> Option<u64> {
    if running_count() > 0 { return None; }
    let prefix = format!("clouter-{}-", std::process::id());
    let mut freed = 0u64;
    if let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with(&prefix) && !n.ends_with("-session") {
                freed += alloc_size(&e.path());
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    Some(freed)
}

/// Удаляет кэш, оставшийся от прошлых запусков (после сбоя или принудительного закрытия).
pub fn clean_stale_cache() {
    if let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("clouter-") || n.starts_with("googletor-") { let _ = std::fs::remove_dir_all(e.path()); }
        }
    }
}

pub async fn run_job(
    source: &str,
    dest: Dest,
    window_bytes: u64,
    progress: Arc<JobProgress>,
) -> Result<()> {
    raise_fd_limit();
    progress.set_status("queued");
    let slot = loop {
        let r = RUNNING.load(Relaxed);
        if r < MAX_PAR.load(Relaxed) && RUNNING.compare_exchange(r, r + 1, Relaxed, Relaxed).is_ok() { break Slot; }
        if progress.cancel.load(Relaxed) { progress.set_status("cancelled"); bail!("cancelled"); }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let work = std::env::temp_dir().join(format!("clouter-{}-{}", std::process::id(), JOB_SEQ.fetch_add(1, Relaxed)));
    tokio::fs::create_dir_all(&work).await?;
    let res = run_inner(source, dest.clone(), window_bytes, progress.clone(), &work).await;
    // Убираем торрент из общего движка (файлы в кэше удаляем ниже сами).
    let tid = progress.torrent_id.load(Relaxed);
    if tid > 0 {
        if let Ok(s) = shared_session().await {
            let _ = s.delete(librqbit::api::TorrentIdOrHash::Id((tid - 1) as usize), false).await;
        }
    }
    let _ = tokio::fs::remove_dir_all(&work).await;
    let cancelled = matches!(&res, Err(e) if e.to_string() == "cancelled");
    drop(slot); // следующая загрузка из очереди стартует сразу, не ждёт чистку
    if cancelled { cleanup_cloud(&dest, &progress).await; }
    {
        let id = progress.state.lock().unwrap().as_ref().map(|s| s.id.clone());
        match (&res, id) {
            (Ok(_), Some(id)) => { jobstate::remove(&id); *progress.state.lock().unwrap() = None; }
            (Err(_), Some(id)) if cancelled => { jobstate::remove(&id); *progress.state.lock().unwrap() = None; }
            (Err(e), Some(_)) => { let m = e.to_string(); persist(&progress, |s| s.error = Some(m)); }
            _ => {}
        }
    }
    match &res {
        Ok(_) => progress.set_status("done"),
        Err(_) if cancelled => progress.set_status("cancelled"),
        Err(e) => progress.set_status(&format!("error:{e:#}")),
    }
    res
}

/// Выполняет операцию, но прерывает её сразу, как только пользователь нажал «Отмена».
async fn cancellable<T>(p: &JobProgress, f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::select! {
        r = f => r,
        _ = async { while !p.cancel.load(Relaxed) { tokio::time::sleep(Duration::from_millis(150)).await } } => bail!("cancelled"),
    }
}

/// Отладочный вывод (включается переменной CLOUTER_TRACE).
fn trace(msg: &str) { if std::env::var("CLOUTER_TRACE").is_ok() { eprintln!("[trace] {msg}"); } }

/// Обновляет и записывает состояние загрузки на диск.
fn persist(progress: &JobProgress, upd: impl FnOnce(&mut jobstate::JobState)) {
    let mut g = progress.state.lock().unwrap();
    if let Some(st) = g.as_mut() { upd(st); jobstate::save(st); }
}

/// Отмена: прерываем незавершённую загрузку и отправляем в корзину облака всё, что эта загрузка успела создать.
pub async fn cleanup_cloud(dest: &Dest, progress: &JobProgress) {
    if let Dest::Drive(auth, _) = dest {
        let url = progress.session.lock().unwrap().take();
        if let Some(u) = url { auth.cancel_session(&u).await; }
        let ids: Vec<String> = std::mem::take(&mut *progress.created.lock().unwrap());
        let mut seen = std::collections::HashSet::new();
        let ids: Vec<String> = ids.into_iter().filter(|id| seen.insert(id.clone())).collect();
        // Первым создаётся корневая папка раздачи (или единственный файл): в корзину уходит она, вложенное вместе с ней.
        if let Some(root) = ids.first() { let _ = auth.trash(root).await; }
        auth.forget_folders();
    }
}

async fn run_inner(
    source: &str,
    dest: Dest,
    window_bytes: u64,
    progress: Arc<JobProgress>,
    work: &Path,
) -> Result<()> {
    // В режиме RAM данные не касаются диска: окно задаёт настройка, но не больше четверти памяти Mac.
    let ram_store: Option<Arc<ramstore::RamStore>> =
        ((USE_RAM.load(Relaxed) && !matches!(dest, Dest::Local(_))) || std::env::var("CLOUTER_RAM_TEST").is_ok()).then(|| Arc::new(ramstore::RamStore::default()));
    let window_bytes = if ram_store.is_some() {
        (RAM_MB.load(Relaxed) << 20).min(ramstore::physical_memory() / 4).max(3 * CHUNK as u64)
    } else {
        // Окно на диске не больше трети свободного места (но не меньше 256 МБ).
        match accounts::free_space(work) {
            Some(free) => window_bytes.min((free / 3).max(256 << 20)).max(3 * CHUNK as u64),
            None => window_bytes.max(3 * CHUNK as u64),
        }
    };
    progress.cache_cap.store(window_bytes, Relaxed);
    progress.cache_ram.store(ram_store.is_some(), Relaxed);
    let mut last_cache = std::time::Instant::now() - Duration::from_secs(10);
    let session = shared_session().await?;
    progress.set_status("meta");

    let add = if source.starts_with("magnet:") {
        AddTorrent::from_url(source)
    } else {
        AddTorrent::from_bytes(tokio::fs::read(source).await.context("reading .torrent")?)
    };
    let opts = AddTorrentOptions {
        only_files: Some(vec![]),
        overwrite: true,
        output_folder: Some(work.to_string_lossy().to_string()),
        storage_factory: ram_store.as_ref().map(|st| {
            use librqbit::storage::StorageFactoryExt;
            ramstore::RamFactory(st.clone()).boxed()
        }),
        ..Default::default()
    };
    let handle = cancellable(&progress, async { Ok(session.add_torrent(add, Some(opts)).await?) }).await?
        .into_handle()
        .context("torrent not added")?;
    progress.torrent_id.store(handle.id() as u64 + 1, Relaxed);
    // Скорость, пиры, скачанное и размер кэша обновляются сами, независимо от хода передачи в облако.
    let stop = Arc::new(AtomicBool::new(false));
    let _guard = StatsGuard(stop.clone());
    {
        let (h, p, rs, w, st) = (handle.clone(), progress.clone(), ram_store.clone(), work.to_path_buf(), stop.clone());
        tokio::spawn(async move {
            let mut n = 0u32;
            // Скорость скачивания: среднее за последние 10 секунд (мгновенная у торрентов сильно скачет).
            let mut hist: std::collections::VecDeque<(std::time::Instant, u64)> = Default::default();
            while !st.load(Relaxed) {
                update_rates(&h, &p);
                let now = std::time::Instant::now();
                let pb = h.stats().progress_bytes;
                hist.push_back((now, pb));
                while hist.front().map(|f| now.duration_since(f.0) > Duration::from_secs(10)).unwrap_or(false) { hist.pop_front(); }
                if let Some(first) = hist.front() {
                    let dt = now.duration_since(first.0).as_secs_f64();
                    if dt >= 1.0 { p.rate_bps.store((pb.saturating_sub(first.1) as f64 / dt) as u64, Relaxed); }
                }
                let i = p.file_idx.load(Relaxed).saturating_sub(1) as usize;
                let have = h.stats().file_progress.get(i).copied().unwrap_or(0);
                p.dl_done.store(p.base_done.load(Relaxed) + have, Relaxed);
                if n % 4 == 0 { p.cache_bytes.store(job_cache(&rs, &w), Relaxed); }
                n += 1;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
    }
    cancellable(&progress, async { handle.wait_until_initialized().await }).await?;

    let files: Vec<(PathBuf, u64)> = handle.with_metadata(|m| {
        m.file_infos.iter().map(|f| (f.relative_filename.clone(), f.len)).collect()
    })?;
    *progress.name.lock().unwrap() = handle.name().unwrap_or_default();
    progress.total.store(files.iter().map(|f| f.1).sum(), Relaxed);
    progress.file_count.store(files.len() as u64, Relaxed);
    let (tname, ttotal) = (progress.name.lock().unwrap().clone(), progress.total.load(Relaxed));
    persist(&progress, |s| { s.name = tname; s.total = ttotal; s.error = None; });
    let prev = progress.state.lock().unwrap().clone();
    let resuming = prev.as_ref().map(|s| s.next_file > 0 || s.session.is_some() || !s.created.is_empty()).unwrap_or(false);
    let rename_to: Option<String> = prev.as_ref().and_then(|s| s.rename_to.clone());
    let mut renames: std::collections::HashMap<String, String> = prev.as_ref().map(|s| s.renames.clone()).unwrap_or_default();
    if resuming {
        if let (Dest::Drive(auth, _), Some(s)) = (&dest, &prev) { auth.restore_folders(s.folders.clone()); }
        progress.done.store(prev.as_ref().map(|s| s.done_bytes).unwrap_or(0), Relaxed);
    }
    // Конфликт имён в облаке: проверяем ВСЕ элементы верхнего уровня раздачи и спрашиваем один раз (при продолжении вопрос уже был задан).
    if let (false, Dest::Drive(auth, base)) = (resuming, &dest) {
        let mut tops: Vec<String> = vec![];
        for (rel, _) in &files {
            let t = rel.components().next().map(|c| c.as_os_str().to_string_lossy().to_string()).unwrap_or_default();
            if !t.is_empty() && !tops.contains(&t) { tops.push(t); }
        }
        let mut existing: Vec<(String, String)> = vec![];
        for t in &tops {
            if let Some(id) = auth.find_by_name(base.as_deref(), t).await? { existing.push((t.clone(), id)); }
        }
        if !existing.is_empty() {
            *progress.conflict.lock().unwrap() = existing.iter().map(|e| e.0.clone()).collect::<Vec<_>>().join(", ");
            progress.decision.store(0, Relaxed);
            progress.set_status("conflict");
            let choice = loop {
                progress.check()?;
                let d = progress.decision.load(Relaxed);
                if d != 0 { break d; }
                tokio::time::sleep(Duration::from_millis(300)).await;
            };
            progress.conflict.lock().unwrap().clear();
            match choice {
                // Заменить: старое целиком (со всем содержимым) уходит в корзину, новое пишется с нуля.
                1 => { for (_, id) in &existing { auth.trash(id).await?; } }
                2 => {
                    for (top, _) in &existing {
                        let (stem, ext) = match top.rsplit_once('.') {
                            Some((s, e)) if !s.is_empty() && e.len() <= 5 && files.len() == 1 => (s.to_string(), format!(".{e}")),
                            _ => (top.clone(), String::new()),
                        };
                        let mut n = 1;
                        let name = loop {
                            let cand = format!("{stem} ({n}){ext}");
                            if auth.find_by_name(base.as_deref(), &cand).await?.is_none() { break cand; }
                            n += 1;
                        };
                        renames.insert(top.clone(), name);
                    }
                    let r = renames.clone();
                    persist(&progress, |s| s.renames = r);
                }
                _ => bail!("cancelled"),
            }
            progress.set_status("meta");
        }
    }
    let out_folder = handle.output_folder().to_path_buf();
    // Локальный приёмник принимает файл целиком: проверим, что он поместится.
    if let Dest::Local(root) = &dest {
        let biggest = files.iter().map(|f| f.1).max().unwrap_or(0);
        if let Some(free) = accounts::free_space(root) {
            let need = biggest + (1 << 30);
            if need > free {
                progress.needs_api.store(true, Relaxed);
                bail!("TOO_BIG|{}|{}", biggest, free);
            }
        }
    }
    let mut base_done = 0u64;

    for (i, (rel, size)) in files.iter().enumerate() {
        progress.check()?;
        if *size == 0 {
            continue;
        }
        let (skip_before, resume_session, resume_file) = prev.as_ref().map(|s| (s.next_file, s.session.clone(), s.session_file)).unwrap_or((0, None, 0));
        if resuming && i < skip_before { base_done += *size; progress.base_done.store(base_done, Relaxed); progress.done.store(base_done, Relaxed); continue; }
        trace(&format!("file {i}: update_only_files"));
        // Выбор файла в торренте: отменяется кнопкой и повторяется, если затянулся (редкий случай холодного старта сети).
        for attempt in 0..4 {
            match tokio::time::timeout(Duration::from_secs(20), cancellable(&progress, async { Ok(session.update_only_files(&handle, &HashSet::from([i])).await?) })).await {
                Ok(r) => { r?; break; }
                Err(_) if attempt < 3 => continue,
                Err(_) => bail!("E_TORRENT_STALL"),
            }
        }
        trace(&format!("file {i}: only_files set"));
        let abs = out_folder.join(rel);
        let target_rel: PathBuf = {
            let mut c = rel.components();
            let first = c.next().map(|x| x.as_os_str().to_string_lossy().to_string()).unwrap_or_default();
            match renames.get(&first).or(rename_to.as_ref()) {
                Some(new) => std::iter::once(std::ffi::OsString::from(new)).chain(c.map(|x| x.as_os_str().to_owned())).collect(),
                None => rel.clone(),
            }
        };
        let mut offset = 0u64;
        // Продолжение: спрашиваем у Google, докуда дошла прерванная загрузка этого файла.
        let mut resumed: Option<FileSink> = None;
        if let (true, Some(url), Dest::Drive(auth, _)) = (resuming && resume_file == i, resume_session.clone(), &dest) {
            match drive::ResumableUpload::query(auth, &url, *size).await? {
                drive::SessionState::Next(n) => { offset = n; resumed = Some(FileSink::Drive(drive::ResumableUpload::from_session(auth.clone(), url, *size))); }
                drive::SessionState::Done(_) => { base_done += *size; progress.base_done.store(base_done, Relaxed); progress.done.store(base_done, Relaxed); persist(&progress, |s| { s.next_file = i + 1; s.session = None; s.offset = 0; s.done_bytes = base_done; }); continue; }
                drive::SessionState::Expired => {}
            }
        }
        trace(&format!("file {i}: opening sink"));
        let mut sink = match resumed { Some(s) => s, None => FileSink::open(&dest, &target_rel, *size).await? };
        trace(&format!("file {i}: sink opened"));
        progress.created.lock().unwrap().extend(sink.created());
        *progress.session.lock().unwrap() = sink.session_url();
        let mut stream = handle.clone().stream(i).await?;
        if offset > 0 { stream.seek(std::io::SeekFrom::Start(offset)).await?; }
        progress.file_idx.store(i as u64 + 1, Relaxed);
        progress.base_done.store(base_done, Relaxed);
        progress.done.store(base_done + offset, Relaxed);
        let mut paused = false;

        while offset < *size {
            progress.check()?;
            let want = CHUNK.min((*size - offset) as usize);
            let mut buf = vec![0u8; want];
            progress.set_status("working");
            // чтение с периодическим обновлением статистики
            let mut read = 0;
            let mut last_data = std::time::Instant::now();
            while read < want {
                // Пауза по кнопке: останавливаем торрент и ждём продолжения.
                if progress.paused.load(Relaxed) {
                    let _ = session.pause(&handle).await;
                    progress.set_status("paused");
                    progress.rate_bps.store(0, Relaxed);
                    while progress.paused.load(Relaxed) {
                        progress.check()?;
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                    session.unpause(&handle).await?;
                    paused = false;
                    stream = handle.clone().stream(i).await?;
                    stream.seek(std::io::SeekFrom::Start(offset + read as u64)).await?;
                    progress.set_status("working");
                }
                tokio::select! {
                    r = stream.read(&mut buf[read..]) => {
                        let n = r?;
                        if n == 0 { bail!("unexpected end of stream"); }
                        read += n;
                        last_data = std::time::Instant::now();
                    }
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {
                        progress.check()?;
                        // Данные не идут 3 минуты (сон Mac, потеря пиров): пересоздаём поток и будим торрент.
                        if last_data.elapsed() > Duration::from_secs(180) && !progress.paused.load(Relaxed) {
                            last_data = std::time::Instant::now();
                            let _ = session.unpause(&handle).await;
                            paused = false;
                            stream = handle.clone().stream(i).await?;
                            stream.seek(std::io::SeekFrom::Start(offset + read as u64)).await?;
                        }
                        if paused {
                            // ждём данные, а торрент стоит на паузе окна: возобновляем, иначе они не придут
                            session.unpause(&handle).await?;
                            paused = false;
                            stream = handle.clone().stream(i).await?;
                            stream.seek(std::io::SeekFrom::Start(offset + read as u64)).await?;
                        }
                        update_rates(&handle, &progress);
                        let have = handle.stats().file_progress.get(i).copied().unwrap_or(0);
                        progress.dl_done.store(base_done + have.min(*size), Relaxed);
                        if last_cache.elapsed() > Duration::from_secs(2) {
                            progress.cache_bytes.store(job_cache(&ram_store, work), Relaxed);
                            last_cache = std::time::Instant::now();
                        }
                    }
                }
            }
            progress.set_status("working");
            let t0 = std::time::Instant::now();
            cancellable(&progress, sink.put(offset, buf)).await?;
            let secs = t0.elapsed().as_secs_f64().max(0.001);
            let now_bps = (want as f64 / secs) as u64;
            let old = progress.up_bps.load(Relaxed);
            progress.up_bps.store(if old == 0 { now_bps } else { (old * 6 + now_bps * 4) / 10 }, Relaxed);
            match &ram_store { Some(st) => st.free(i, offset, want as u64), None => punch_hole(&abs, offset, want as u64) }
            offset += want as u64;
            progress.done.store(base_done + offset, Relaxed);
            progress.cache_bytes.store(job_cache(&ram_store, work), Relaxed);
            {
                let (sess, created) = (sink.session_url(), progress.created.lock().unwrap().clone());
                let folders = if let Dest::Drive(a, _) = &dest { a.folders_snapshot() } else { Default::default() };
                let done_now = base_done + offset;
                persist(&progress, |s| { s.session = sess; s.session_file = i; s.offset = offset; s.created = created; s.folders = folders; s.done_bytes = done_now; });
            }

            // контроль окна на диске
            let have = handle.stats().file_progress.get(i).copied().unwrap_or(0);
            let ahead = have.saturating_sub(offset);
            if !paused && ahead > window_bytes {
                session.pause(&handle).await?;
                paused = true;
            } else if paused && ahead < window_bytes / 2 {
                session.unpause(&handle).await?;
                paused = false;
                // поток после паузы пересоздаём
                stream = handle.clone().stream(i).await?;
                stream.seek(std::io::SeekFrom::Start(offset)).await?;
            }
        }
        if paused {
            session.unpause(&handle).await?;
        }
        drop(stream);
        progress.created.lock().unwrap().extend(sink.created());
        *progress.session.lock().unwrap() = None;
        if ram_store.is_none() { let _ = tokio::fs::remove_file(&abs).await; }
        base_done += *size;
        progress.base_done.store(base_done, Relaxed);
        progress.done.store(base_done, Relaxed);
        progress.dl_done.store(base_done, Relaxed);
        persist(&progress, |s| { s.next_file = i + 1; s.session = None; s.offset = 0; s.done_bytes = base_done; });
    }
    Ok(())
}

fn update_rates(h: &Arc<ManagedTorrent>, p: &JobProgress) {
    let st = h.stats();
    if let Some(l) = st.live {
        p.peers.store(l.snapshot.peer_stats.live as u64, Relaxed);
        p.peers_seen.store(l.snapshot.peer_stats.seen as u64, Relaxed);
    } else {
        p.peers.store(0, Relaxed);
    }
}
