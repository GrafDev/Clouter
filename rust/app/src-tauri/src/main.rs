#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{atomic::{AtomicU64, Ordering::Relaxed}, Arc, Mutex},
};

use googletor_core::{
    accounts, drive::DriveAuth, dropbox::Dropbox, magnet,
    oauth::{self, OAuth}, onedrive::OneDrive, run_job, Dest, JobProgress,
};
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, State, WindowEvent,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_deep_link::DeepLinkExt;

#[cfg(target_os = "macos")]
mod launch {
    //! Назначение Clouter программой по умолчанию для magnet: и .torrent (LaunchServices).
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};

    const BUNDLE: &str = "app.clouter";
    const ROLE_ALL: u32 = 0xFFFF_FFFF;

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        fn LSSetDefaultHandlerForURLScheme(scheme: CFStringRef, bundle: CFStringRef) -> i32;
        fn LSCopyDefaultHandlerForURLScheme(scheme: CFStringRef) -> CFStringRef;
        fn LSSetDefaultRoleHandlerForContentType(uti: CFStringRef, role: u32, bundle: CFStringRef) -> i32;
        fn LSCopyDefaultRoleHandlerForContentType(uti: CFStringRef, role: u32) -> CFStringRef;
        fn UTTypeCreatePreferredIdentifierForTag(class: CFStringRef, tag: CFStringRef, conforming: CFStringRef) -> CFStringRef;
    }

    fn torrent_uti() -> Option<CFString> {
        unsafe {
            let class = CFString::new("public.filename-extension");
            let tag = CFString::new("torrent");
            let r = UTTypeCreatePreferredIdentifierForTag(class.as_concrete_TypeRef(), tag.as_concrete_TypeRef(), std::ptr::null());
            (!r.is_null()).then(|| CFString::wrap_under_create_rule(r))
        }
    }

    pub fn is_default() -> bool {
        unsafe {
            let magnet = CFString::new("magnet");
            let h = LSCopyDefaultHandlerForURLScheme(magnet.as_concrete_TypeRef());
            let ok_magnet = !h.is_null() && CFString::wrap_under_create_rule(h).to_string().eq_ignore_ascii_case(BUNDLE);
            let ok_file = match torrent_uti() {
                Some(u) => {
                    let h = LSCopyDefaultRoleHandlerForContentType(u.as_concrete_TypeRef(), ROLE_ALL);
                    !h.is_null() && CFString::wrap_under_create_rule(h).to_string().eq_ignore_ascii_case(BUNDLE)
                }
                None => false,
            };
            ok_magnet && ok_file
        }
    }

    pub fn make_default() -> Result<(), String> {
        unsafe {
            let b = CFString::new(BUNDLE);
            let magnet = CFString::new("magnet");
            let r1 = LSSetDefaultHandlerForURLScheme(magnet.as_concrete_TypeRef(), b.as_concrete_TypeRef());
            let r2 = match torrent_uti() {
                Some(u) => LSSetDefaultRoleHandlerForContentType(u.as_concrete_TypeRef(), ROLE_ALL, b.as_concrete_TypeRef()),
                None => -1,
            };
            if r1 != 0 || r2 != 0 { return Err(format!("E_DEFAULT_DENIED|{r1}/{r2}")); }
            Ok(())
        }
    }
}

#[tauri::command]
fn default_status() -> bool {
    #[cfg(target_os = "macos")] { launch::is_default() }
    #[cfg(not(target_os = "macos"))] { true }
}

#[tauri::command]
fn make_default() -> Result<(), String> {
    #[cfg(target_os = "macos")] { launch::make_default() }
    #[cfg(not(target_os = "macos"))] { Ok(()) }
}

struct Job {
    progress: Arc<JobProgress>,
    dest_name: String,
    finished: Arc<std::sync::atomic::AtomicBool>,
    finished_at: Arc<AtomicU64>,
}

#[derive(Default)]
struct AppState {
    jobs: Mutex<BTreeMap<u64, Job>>,
    next: AtomicU64,
    google: Mutex<Option<Arc<DriveAuth>>>,
    dropbox: Mutex<Option<Arc<Dropbox>>>,
    onedrive: Mutex<Option<Arc<OneDrive>>>,
    /// Ссылки и .torrent, пришедшие от системы до того, как окно готово их принять.
    incoming: Mutex<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(tag = "kind")]
enum DestArg {
    Cloud { provider: String, folder: Option<String> },
    Folder { path: PathBuf, name: String },
}

#[derive(Serialize)]
struct JobInfo {
    id: u64,
    name: String,
    status: String,
    done: u64,
    total: u64,
    rate: u64,
    dest: String,
    finished: bool,
    needs_api: bool,
    conflict: String,
    resumable: bool,
    cache: u64,
    cache_cap: u64,
    cache_ram: bool,
    paused: bool,
    dl_done: u64,
    peers: u64,
    peers_seen: u64,
    up_rate: u64,
    file_idx: u64,
    file_count: u64,
    eta: u64,
}

#[derive(Serialize)]
struct ProviderInfo {
    id: &'static str,
    signed_in: bool,
    embedded: bool,
    email: Option<String>,
}

#[derive(Serialize)]
struct Clouds {
    system: Vec<accounts::SystemCloud>,
    providers: Vec<ProviderInfo>,
    google_hint: Option<String>,
}

/// Подключённый аккаунт одного из облаков.
#[derive(Clone)]
enum Auth {
    Google(Arc<DriveAuth>),
    Dropbox(Arc<Dropbox>),
    OneDrive(Arc<OneDrive>),
}

fn provider_title(p: &str) -> &'static str {
    match p { "google" => "Google Drive", "dropbox" => "Dropbox", "onedrive" => "OneDrive", _ => "Облако" }
}

fn get_auth(state: &AppState, p: &str) -> Result<Auth, String> {
    let need = || "AUTH_REQUIRED".to_string();
    match p {
        "google" => { let mut g = state.google.lock().unwrap();
            if g.is_none() { *g = DriveAuth::saved(); } g.clone().map(Auth::Google).ok_or_else(need) }
        "dropbox" => { let mut g = state.dropbox.lock().unwrap();
            if g.is_none() { *g = Dropbox::saved(); } g.clone().map(Auth::Dropbox).ok_or_else(need) }
        "onedrive" => { let mut g = state.onedrive.lock().unwrap();
            if g.is_none() { *g = OneDrive::saved(); } g.clone().map(Auth::OneDrive).ok_or_else(need) }
        _ => Err("E_UNKNOWN_CLOUD".into()),
    }
}

impl Auth {
    fn dest(&self, folder: Option<String>) -> Dest {
        match self {
            Auth::Google(a) => Dest::Drive(a.clone(), folder),
            Auth::Dropbox(a) => Dest::Dropbox(a.clone(), folder),
            Auth::OneDrive(a) => Dest::OneDrive(a.clone(), folder),
        }
    }
    async fn list(&self, id: &str) -> Result<Vec<(String, String)>, String> {
        match self {
            Auth::Google(a) => a.list_folders(id).await,
            Auth::Dropbox(a) => a.list_folders(id).await,
            Auth::OneDrive(a) => a.list_folders(id).await,
        }.map_err(|e| format!("{e:#}"))
    }
    async fn create(&self, parent: &str, name: &str) -> Result<String, String> {
        match self {
            Auth::Google(a) => a.create_folder(if parent == "root" { None } else { Some(parent) }, name).await,
            Auth::Dropbox(a) => a.create_folder(parent, name).await,
            Auth::OneDrive(a) => a.create_folder(parent, name).await,
        }.map_err(|e| format!("{e:#}"))
    }
}

#[tauri::command]
fn parse_magnet(text: String) -> Result<magnet::Magnet, String> {
    let link = magnet::find_in_text(&text).unwrap_or(text);
    magnet::parse(&link).map_err(|e| e.to_string())
}

#[tauri::command]
fn clouds() -> Clouds {
    Clouds {
        system: accounts::detect(),
        providers: vec![
            ProviderInfo { id: "google", signed_in: DriveAuth::signed_in(), embedded: DriveAuth::embedded().is_some(), email: DriveAuth::saved_email() },
            ProviderInfo { id: "dropbox", signed_in: OAuth::signed_in("dropbox"), embedded: Dropbox::embedded().is_some(), email: oauth::saved_email("dropbox") },
            ProviderInfo { id: "onedrive", signed_in: OAuth::signed_in("onedrive"), embedded: OneDrive::embedded().is_some(), email: oauth::saved_email("onedrive") },
        ],
        google_hint: accounts::google_hint(),
    }
}

/// Вход через браузер. client_id / client_secret нужны, только если ключ не зашит в приложение.
#[tauri::command]
async fn sign_in(state: State<'_, AppState>, provider: String, client_id: Option<String>, client_secret: Option<String>) -> Result<Option<String>, String> {
    let id = client_id.unwrap_or_default();
    let err = |e: anyhow::Error| format!("{e:#}");
    match provider.as_str() {
        "google" => {
            let a = DriveAuth::sign_in(id.trim(), client_secret.unwrap_or_default().trim(), accounts::google_hint().as_deref()).await.map_err(err)?;
            *state.google.lock().unwrap() = Some(a);
            Ok(DriveAuth::saved_email())
        }
        "dropbox" => {
            let a = Dropbox::sign_in(&id).await.map_err(err)?;
            *state.dropbox.lock().unwrap() = Some(a);
            Ok(oauth::saved_email("dropbox"))
        }
        "onedrive" => {
            let a = OneDrive::sign_in(&id, None).await.map_err(err)?;
            *state.onedrive.lock().unwrap() = Some(a);
            Ok(oauth::saved_email("onedrive"))
        }
        _ => Err("E_UNKNOWN_CLOUD".into()),
    }
}

#[tauri::command]
fn sign_out(state: State<'_, AppState>, provider: String) {
    match provider.as_str() {
        "google" => { DriveAuth::sign_out(); *state.google.lock().unwrap() = None; }
        "dropbox" => { OAuth::sign_out("dropbox"); *state.dropbox.lock().unwrap() = None; }
        "onedrive" => { OAuth::sign_out("onedrive"); *state.onedrive.lock().unwrap() = None; }
        _ => {}
    }
}

/// Запускает (или продолжает) загрузку в фоне.
fn launch(source: String, dest: Dest, dest_name: String, window: u64, progress: Arc<JobProgress>,
          finished: Arc<std::sync::atomic::AtomicBool>, finished_at: Arc<AtomicU64>, api: Option<Auth>) {
    tauri::async_runtime::spawn(async move {
        let r = run_job(&source, dest, window, progress.clone()).await;
        if let (Err(e), Some(auth)) = (&r, api) {
            if e.to_string().starts_with("TOO_BIG") {
                progress.needs_api.store(false, Relaxed);
                progress.set_status("fallback");
                let _ = run_job(&source, auth.dest(None), window, progress.clone()).await;
            }
        }
        if progress.status.lock().unwrap().starts_with("error:TOO_BIG") {
            let (b, f) = {
                let st = progress.status.lock().unwrap().clone();
                let v: Vec<&str> = st.trim_start_matches("error:TOO_BIG|").split('|').collect();
                (v.first().and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0), v.get(1).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0))
            };
            progress.set_status(&format!("toobig:{:.1}|{:.1}", b / 1e9, f / 1e9));
            progress.needs_api.store(true, Relaxed);
        }
        let now = now_secs();
        finished_at.store(now, Relaxed);
        finished.store(true, Relaxed);
        push_history(HistoryItem {
            name: progress.name.lock().unwrap().clone(),
            status: progress.status.lock().unwrap().clone(),
            size: progress.total.load(Relaxed),
            cloud: dest_name,
            at: now,
        });
    });
}

#[tauri::command]
fn add_job(state: State<'_, AppState>, source: String, dest: DestArg, window_gb: f64) -> Result<u64, String> {
    let source = source.trim().to_string();
    let source = if source.starts_with("magnet:") || source.contains("magnet:?") {
        let link = magnet::find_in_text(&source).unwrap_or(source);
        magnet::parse(&link).map_err(|e| e.to_string())?;
        link
    } else if std::path::Path::new(&source).is_file() {
        source
    } else {
        return Err("E_NOT_SOURCE".into());
    };
    let (dest, dest_name, cloud) = match dest {
        DestArg::Cloud { provider, folder } => {
            let auth = get_auth(&state, &provider)?;
            (auth.dest(folder.clone()), provider_title(&provider).to_string(), Some((provider, folder)))
        }
        DestArg::Folder { path, name } => (Dest::Local(path), name, None),
    };
    let id = state.next.fetch_add(1, Relaxed);
    let progress = Arc::new(JobProgress::default());
    *progress.name.lock().unwrap() = source.chars().take(60).collect();
    progress.set_status("queued");
    // Для облачных загрузок сохраняем состояние: их можно продолжить после закрытия приложения или сбоя.
    let state_id = format!("{}-{}", now_secs(), id);
    let source = match &cloud {
        Some((provider, folder)) => {
            let kept = if source.starts_with("magnet:") { source.clone() } else { googletor_core::jobstate::keep_torrent_copy(&state_id, &source) };
            let st = googletor_core::jobstate::JobState {
                id: state_id.clone(), source: kept.clone(), provider: provider.clone(), folder: folder.clone(),
                dest_name: dest_name.clone(), window_gb, name: source.chars().take(60).collect(), ..Default::default()
            };
            googletor_core::jobstate::save(&st);
            *progress.state.lock().unwrap() = Some(st);
            kept
        }
        None => source,
    };
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finished_at = Arc::new(AtomicU64::new(0));
    state.jobs.lock().unwrap().insert(id, Job { progress: progress.clone(), dest_name: dest_name.clone(), finished: finished.clone(), finished_at: finished_at.clone() });
    let window = (window_gb.max(0.25) * 1073741824.0) as u64;
    // Не поместилось на диск: если подключён любой облачный API, переходим на него.
    let api = if matches!(dest, Dest::Local(_)) {
        ["google", "dropbox", "onedrive"].iter().find_map(|p| get_auth(&state, p).ok())
    } else { None };
    launch(source, dest, dest_name, window, progress, finished, finished_at, api);
    Ok(id)
}

/// Подхватывает загрузки, прерванные закрытием приложения или сбоем: они появляются в списке с кнопкой «Продолжить».
fn load_interrupted(state: &AppState) {
    for s in googletor_core::jobstate::load_all() {
        let id = state.next.fetch_add(1, Relaxed);
        let progress = Arc::new(JobProgress::default());
        *progress.name.lock().unwrap() = if s.name.is_empty() { s.source.chars().take(60).collect() } else { s.name.clone() };
        progress.total.store(s.total, Relaxed);
        progress.done.store(s.done_bytes, Relaxed);
        progress.dl_done.store(s.done_bytes, Relaxed);
        *progress.created.lock().unwrap() = s.created.clone();
        *progress.session.lock().unwrap() = s.session.clone();
        progress.set_status(&match &s.error { Some(e) => format!("error:{e}"), None => "interrupted".to_string() });
        let dest_name = s.dest_name.clone();
        *progress.state.lock().unwrap() = Some(s);
        state.jobs.lock().unwrap().insert(id, Job {
            progress, dest_name,
            finished: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            finished_at: Arc::new(AtomicU64::new(now_secs())),
        });
    }
}

/// Продолжить прерванную или упавшую загрузку с того места, где она остановилась.
#[tauri::command]
fn resume_job(state: State<'_, AppState>, id: u64) -> Result<(), String> {
    let (progress, dest_name, finished, finished_at) = {
        let g = state.jobs.lock().unwrap();
        let j = g.get(&id).ok_or("no such job")?;
        if !j.finished.load(Relaxed) { return Ok(()); }
        (j.progress.clone(), j.dest_name.clone(), j.finished.clone(), j.finished_at.clone())
    };
    let st = progress.state.lock().unwrap().clone().ok_or("nothing to resume")?;
    let auth = get_auth(&state, &st.provider)?;
    progress.cancel.store(false, Relaxed);
    progress.paused.store(false, Relaxed);
    progress.needs_api.store(false, Relaxed);
    progress.torrent_id.store(0, Relaxed);
    progress.up_bps.store(0, Relaxed);
    progress.set_status("queued");
    finished.store(false, Relaxed);
    let window = (st.window_gb.max(0.25) * 1073741824.0) as u64;
    launch(st.source.clone(), auth.dest(st.folder.clone()), dest_name, window, progress, finished, finished_at, None);
    Ok(())
}

/// Убрать прерванную загрузку: прервать сессию в облаке, отправить созданное в корзину, забыть состояние.
#[tauri::command]
fn discard_job(state: State<'_, AppState>, id: u64) {
    let (progress, st) = {
        let g = state.jobs.lock().unwrap();
        let Some(j) = g.get(&id) else { return };
        let p = j.progress.clone();
        let st = p.state.lock().unwrap().clone();
        (p, st)
    };
    progress.cancel.store(true, Relaxed); // карточка пропадает сразу
    let dest = st.as_ref().and_then(|s| get_auth(&state, &s.provider).ok().map(|a| a.dest(s.folder.clone())));
    tauri::async_runtime::spawn(async move {
        if let Some(d) = dest { googletor_core::cleanup_cloud(&d, &progress).await; }
        if let Some(s) = st { googletor_core::jobstate::remove(&s.id); }
        *progress.state.lock().unwrap() = None;
    });
}

#[derive(Serialize)]
struct FolderItem { id: String, name: String }

fn clean_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() || n.contains('/') || n.contains(':') || n == "." || n == ".." {
        return Err("E_BAD_NAME".into());
    }
    Ok(n.to_string())
}

/// cloud: "google" | "dropbox" | "onedrive" (id — идентификатор или путь папки) или "local" (id — путь).
#[tauri::command]
async fn list_folders(state: State<'_, AppState>, cloud: String, id: String) -> Result<Vec<FolderItem>, String> {
    if cloud != "local" {
        let a = get_auth(&state, &cloud)?;
        return Ok(a.list(&id).await?.into_iter().map(|(id, name)| FolderItem { id, name }).collect());
    }
    let mut out: Vec<FolderItem> = std::fs::read_dir(&id).map_err(|e| e.to_string())?.flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| { let n = e.file_name().to_string_lossy().to_string();
            (!n.starts_with('.')).then(|| FolderItem { id: e.path().to_string_lossy().to_string(), name: n }) })
        .collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

#[tauri::command]
async fn create_folder(state: State<'_, AppState>, cloud: String, parent: String, name: String) -> Result<FolderItem, String> {
    let name = clean_name(&name)?;
    if cloud != "local" {
        let a = get_auth(&state, &cloud)?;
        let id = a.create(&parent, &name).await?;
        return Ok(FolderItem { id, name });
    }
    let path = PathBuf::from(&parent).join(&name);
    std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    Ok(FolderItem { id: path.to_string_lossy().to_string(), name })
}

/// Папка «Загрузки» внутри корня облака (название на языке интерфейса, а также Downloads); создаётся, если её нет.
#[tauri::command]
async fn default_folder(state: State<'_, AppState>, cloud: String, root: String, name: String) -> Result<FolderItem, String> {
    let name = clean_name(&name)?;
    let is_dl = |n: &str| n.eq_ignore_ascii_case(&name) || n.eq_ignore_ascii_case("Downloads")
        || n == "Загрузки" || n == "Завантаження";
    if cloud != "local" {
        let a = get_auth(&state, &cloud)?;
        if let Some((id, n)) = a.list(&root).await?.into_iter().find(|(_, n)| is_dl(n)) {
            return Ok(FolderItem { id, name: n });
        }
        let id = a.create(&root, &name).await?;
        return Ok(FolderItem { id, name });
    }
    let base = PathBuf::from(&root);
    if let Ok(rd) = std::fs::read_dir(&base) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() && is_dl(&n) { return Ok(FolderItem { id: e.path().to_string_lossy().to_string(), name: n }); }
        }
    }
    let p = base.join(&name);
    std::fs::create_dir_all(&p).map_err(|e| e.to_string())?;
    Ok(FolderItem { id: p.to_string_lossy().to_string(), name })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Serialize, Deserialize, Clone)]
struct HistoryItem { name: String, status: String, size: u64, cloud: String, at: u64 }

fn history_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Application Support/Googletor/history.json")
}

fn read_history() -> Vec<HistoryItem> {
    std::fs::read_to_string(history_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Сохраняет завершённую загрузку в историю (последние 300 записей).
fn push_history(item: HistoryItem) {
    let mut h = read_history();
    h.insert(0, item);
    h.truncate(300);
    let p = history_path();
    if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
    let _ = std::fs::write(p, serde_json::to_string(&h).unwrap_or_default());
}

#[tauri::command]
fn history_list() -> Vec<HistoryItem> { read_history() }

#[tauri::command]
fn history_clear() { let _ = std::fs::remove_file(history_path()); }

/// Завершённые загрузки сами исчезают из списка (успешные через 8 с, остальные через минуту) и остаются в истории.
#[tauri::command]
fn list_jobs(state: State<'_, AppState>) -> Vec<JobInfo> {
    let now = now_secs();
    state.jobs.lock().unwrap().iter().filter(|(_, j)| {
        if j.progress.cancel.load(Relaxed) { return false; } // отменённые пропадают сразу, чистка идёт в фоне
        if !j.finished.load(Relaxed) { return true; }
        if j.progress.state.lock().unwrap().is_some() { return true; } // можно продолжить: остаётся в списке
        let ttl = if matches!(j.progress.status.lock().unwrap().as_str(), "done" | "cancelled") { 8 } else { 60 };
        now.saturating_sub(j.finished_at.load(Relaxed)) < ttl
    }).map(|(id, j)| JobInfo {
        id: *id,
        name: j.progress.name.lock().unwrap().clone(),
        status: j.progress.status.lock().unwrap().clone(),
        done: j.progress.done.load(Relaxed),
        total: j.progress.total.load(Relaxed),
        rate: j.progress.rate_bps.load(Relaxed),
        dest: j.dest_name.clone(),
        finished: j.finished.load(Relaxed),
        needs_api: j.progress.needs_api.load(Relaxed),
        conflict: j.progress.conflict.lock().unwrap().clone(),
        resumable: j.finished.load(Relaxed) && j.progress.state.lock().unwrap().is_some(),
        cache: j.progress.cache_bytes.load(Relaxed),
        cache_cap: j.progress.cache_cap.load(Relaxed),
        cache_ram: j.progress.cache_ram.load(Relaxed),
        paused: j.progress.paused.load(Relaxed),
        dl_done: j.progress.dl_done.load(Relaxed).max(j.progress.done.load(Relaxed)),
        peers: j.progress.peers.load(Relaxed),
        peers_seen: j.progress.peers_seen.load(Relaxed),
        up_rate: j.progress.up_bps.load(Relaxed),
        file_idx: j.progress.file_idx.load(Relaxed),
        file_count: j.progress.file_count.load(Relaxed),
        eta: {
            // узкое место: либо скачивание, либо отправка в облако
            let (dl, up) = (j.progress.rate_bps.load(Relaxed), j.progress.up_bps.load(Relaxed));
            let r = if dl > 0 && up > 0 { dl.min(up) } else { dl.max(up) };
            let left = j.progress.total.load(Relaxed).saturating_sub(j.progress.done.load(Relaxed));
            if r > 0 && !j.finished.load(Relaxed) { left / r } else { 0 }
        },
    }).collect()
}

/// Ответ на вопрос о занятом имени: 1 — заменить, 2 — переименовать, 3 — отмена.
#[tauri::command]
fn resolve_conflict(state: State<'_, AppState>, id: u64, choice: u8) {
    if let Some(j) = state.jobs.lock().unwrap().get(&id) { j.progress.decision.store(choice, Relaxed); }
}

#[tauri::command]
fn pause_job(state: State<'_, AppState>, id: u64, paused: bool) {
    if let Some(j) = state.jobs.lock().unwrap().get(&id) { j.progress.paused.store(paused, Relaxed); }
}

#[tauri::command]
fn cancel_job(state: State<'_, AppState>, id: u64) {
    if let Some(j) = state.jobs.lock().unwrap().get(&id) {
        j.progress.cancel.store(true, Relaxed);
        j.progress.paused.store(false, Relaxed);
        if !j.finished.load(Relaxed) { j.progress.set_status("cancelling"); }
    }
}

#[tauri::command]
fn clear_finished(state: State<'_, AppState>) {
    state.jobs.lock().unwrap().retain(|_, j| !j.finished.load(Relaxed));
}

#[derive(Serialize)]
struct DiskInfo { free: u64, cache: u64, window: u64, recommended: usize, running: usize, queued: usize, max: usize, ram_used: u64, ram_total: u64, ram_mode: bool }

/// Данные для панели «Диск»: свободное место, занятый кэш и сколько загрузок разумно запускать одновременно.
#[tauri::command]
fn disk_info(state: State<'_, AppState>, window_gb: f64) -> DiskInfo {
    let free = accounts::free_space(&std::env::temp_dir()).unwrap_or(0);
    let cache = googletor_core::cache_usage();
    let window = (window_gb.max(0.25) * 1073741824.0) as u64;
    let pool = (free + cache).saturating_sub(1 << 30);       // 1 ГБ оставляем системе
    let recommended = ((pool / window.max(256 << 20)) as usize).clamp(1, 5);
    let jobs = state.jobs.lock().unwrap();
    let active = jobs.values().filter(|j| !j.finished.load(Relaxed)).count();
    let running = googletor_core::running_count().min(active);
    DiskInfo { free, cache, window, recommended, running, queued: active - running, max: googletor_core::max_parallel(),
        ram_used: googletor_core::ram_used(), ram_total: googletor_core::ramstore::physical_memory(), ram_mode: googletor_core::ram_mode() }
}

/// Размер временных файлов и можно ли их очистить сейчас (нет идущих загрузок).
#[tauri::command]
fn cache_info() -> (u64, bool) { (googletor_core::cache_usage(), googletor_core::running_count() == 0) }

/// Кнопка «Очистить временные файлы»: возвращает освобождённые байты или None, если идёт загрузка.
#[tauri::command]
fn clear_cache() -> Option<u64> { googletor_core::clear_cache_if_idle() }

#[tauri::command]
fn set_min_height(window: tauri::WebviewWindow, h: f64) {
    let _ = window.set_min_size(Some(tauri::LogicalSize::new(640.0, h)));
    if let (Ok(sz), Ok(sf)) = (window.inner_size(), window.scale_factor()) {
        let l = sz.to_logical::<f64>(sf);
        if l.height < h {
            let _ = window.set_size(tauri::LogicalSize::new(l.width, h));
        }
    }
}

#[tauri::command]
fn set_parallel(n: usize) { googletor_core::set_max_parallel(n); }

/// Режим «через оперативную память» для облаков по API; mb — размер окна в памяти.
#[tauri::command]
fn set_ram_mode(on: bool, mb: u64) { googletor_core::set_ram_mode(on, mb); }

/// Принимает JSON, скачанный из Google Cloud Console («Download JSON»), и сохраняет ключ приложения.
#[tauri::command]
fn install_google_client(path: String) -> Result<(), String> {
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| "E_BAD_JSON".to_string())?;
    let c = v.get("installed").ok_or("E_NOT_DESKTOP")?;
    let (id, secret) = (c["client_id"].as_str().unwrap_or(""), c["client_secret"].as_str().unwrap_or(""));
    if id.is_empty() || secret.is_empty() { return Err("E_BAD_JSON".into()); }
    let dir = std::path::PathBuf::from(std::env::var("HOME").map_err(|e| e.to_string())?)
        .join("Library/Application Support/Googletor");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("google_client.json"), serde_json::json!({"installed": {"client_id": id, "client_secret": secret}}).to_string())
        .map_err(|e| e.to_string())
}

/// Открывает https-ссылку в браузере по умолчанию (страницы консолей облаков).
#[tauri::command]
fn open_link(url: String) -> Result<(), String> {
    if !url.starts_with("https://") { return Err("only https".into()); }
    std::process::Command::new("open").arg(url).status().map(|_| ()).map_err(|e| e.to_string())
}

/// Журнал входящих ссылок/файлов для диагностики: ~/Library/Application Support/Googletor/events.log
fn log_event(line: &str) {
    use std::io::Write;
    if let Ok(h) = std::env::var("HOME") {
        let dir = std::path::PathBuf::from(h).join("Library/Application Support/Googletor");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("events.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{} {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0), line);
        }
    }
}

fn push_incoming(app: &tauri::AppHandle, item: String) {
    log_event(&format!("incoming {}", item.chars().take(80).collect::<String>()));
    if let Some(st) = app.try_state::<AppState>() { st.incoming.lock().unwrap().push(item); }
    let _ = app.emit("incoming", ());
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn forward_magnets(app: &tauri::AppHandle, urls: Vec<String>) {
    for u in urls {
        if u.starts_with("magnet:") { push_incoming(app, u); }
    }
}

/// Окно забирает накопленные ссылки и файлы.
#[tauri::command]
fn take_incoming(state: State<'_, AppState>) -> Vec<String> {
    let v: Vec<String> = std::mem::take(&mut *state.incoming.lock().unwrap());
    if !v.is_empty() { log_event(&format!("take_incoming -> {}", v.len())); }
    v
}

fn show_main(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Показать Clouter", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Выйти", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    TrayIconBuilder::with_id("main")
        .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?)
        .icon_as_template(true)
        .tooltip("Clouter")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "show" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// Закрытие окна: спрашиваем, выйти или свернуть в строку меню.
fn ask_close(app: &tauri::AppHandle) {
    const HIDE: &str = "Свернуть в строку меню";
    const QUIT: &str = "Закрыть приложение";
    let a = app.clone();
    app.dialog()
        .message("Закрыть Clouter или оставить его работать в строке меню? Загрузки продолжатся.")
        .title("Clouter")
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::YesNoCancelCustom(HIDE.into(), QUIT.into(), "Отмена".into()))
        .show_with_result(move |res| {
            if let tauri_plugin_dialog::MessageDialogResult::Custom(c) = res {
                if c == HIDE {
                    if let Some(w) = a.get_webview_window("main") { let _ = w.hide(); }
                    // Убираем иконку из Dock: остаётся только в строке меню
                    #[cfg(target_os = "macos")]
                    let _ = a.set_activation_policy(tauri::ActivationPolicy::Accessory);
                } else if c == QUIT {
                    a.exit(0);
                }
            }
        });
}

fn main() {
    googletor_core::raise_fd_limit();
    googletor_core::clean_stale_cache();
    googletor_core::jobstate::gc();
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _| {
            forward_magnets(app, argv);
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                ask_close(window.app_handle());
            }
        })
        .setup(|app| {
            setup_tray(app)?;
            let h = app.handle().clone();
            if let Some(st) = app.try_state::<AppState>() { load_interrupted(&st); }
            // Ссылка, с которой приложение было запущено
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                forward_magnets(&h, urls.iter().map(|u| u.to_string()).collect());
            }
            app.deep_link().on_open_url(move |ev| {
                forward_magnets(&h, ev.urls().iter().map(|u| u.to_string()).collect());
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            parse_magnet, clouds, sign_in, sign_out,
            add_job, list_jobs, cancel_job, clear_finished, list_folders, create_folder, default_folder, default_status, make_default, take_incoming, open_link, install_google_client, disk_info, cache_info, clear_cache, set_parallel, set_min_height, set_ram_mode, history_list, history_clear, resolve_conflict, pause_job, resume_job, discard_job
        ])
        .build(tauri::generate_context!())
        .expect("ошибка сборки Clouter")
        .run(|app, event| {
            // Клик по иконке в Dock, когда окно скрыто
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event { show_main(app); return; }
            // Двойной клик по .torrent в Finder / «Открыть в Clouter»
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Opened { urls } = event {
                log_event(&format!("RunEvent::Opened {:?}", urls.iter().map(|u| u.as_str().chars().take(60).collect::<String>()).collect::<Vec<_>>()));
                for u in urls {
                    if u.scheme() == "file" {
                        if let Ok(p) = u.to_file_path() {
                            if p.extension().map(|e| e.eq_ignore_ascii_case("torrent")).unwrap_or(false) {
                                push_incoming(app, p.to_string_lossy().to_string());
                            }
                        }
                    }
                }
            }
            #[cfg(not(target_os = "macos"))]
            { let _ = (app, event); }
        });
}
