//! Облака, найденные в системе: iCloud Drive и приложения-синхронизаторы
//! (Google Drive для компьютера, Dropbox, OneDrive...). Вход не нужен — их
//! синхронизирует сама macOS/приложение. Ограничение: файл пишется на локальный диск.
use std::path::PathBuf;

use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct SystemCloud {
    pub name: String,
    pub path: PathBuf,
}

pub fn detect() -> Vec<SystemCloud> {
    let home = match std::env::var("HOME") { Ok(h) => PathBuf::from(h), Err(_) => return vec![] };
    let mut v = vec![];
    let legacy_dropbox = home.join("Dropbox");
    if legacy_dropbox.is_dir() {
        v.push(SystemCloud { name: "Dropbox".into(), path: legacy_dropbox });
    }
    if let Ok(rd) = std::fs::read_dir(home.join("Library/CloudStorage")) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("iCloud") || n.starts_with("GoogleDrive-") { continue; } // Google идёт через API
            let nice = if let Some(x) = n.strip_prefix("GoogleDrive-") { format!("Google Drive ({x})") }
                else if n.starts_with("Dropbox") { "Dropbox".to_string() }
                else if n.starts_with("OneDrive") { format!("OneDrive ({})", n.trim_start_matches("OneDrive-")) }
                else { n.clone() };
            let mut p = e.path();
            if n.starts_with("GoogleDrive-") {
                if let Some(d) = google_my_drive(&p) { p = d; }
            }
            if p.is_dir() { v.push(SystemCloud { name: nice, path: p }); }
        }
    }
    v
}

/// «My Drive» называется по-разному в зависимости от языка системы («Мой диск», «Mon Drive»...).
fn google_my_drive(root: &std::path::Path) -> Option<PathBuf> {
    const SKIP: [&str; 6] = ["Shared drives", "Общие диски", "Shortcuts", "Ярлыки", "Computers", "Компьютеры"];
    let mut cands: Vec<PathBuf> = std::fs::read_dir(root).ok()?.flatten()
        .filter(|e| e.path().is_dir())
        .filter(|e| { let n = e.file_name().to_string_lossy().to_string();
            !n.starts_with('.') && !SKIP.contains(&n.as_str()) })
        .map(|e| e.path()).collect();
    cands.sort();
    cands.into_iter().next()
}

/// Свободное место (байт) на томе, где лежит путь.
pub fn free_space(path: &std::path::Path) -> Option<u64> {
    use std::ffi::CString;
    let mut p = path.to_path_buf();
    while !p.exists() { if !p.pop() { return None; } }
    let c = CString::new(p.to_string_lossy().as_bytes()).ok()?;
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 { return None; }
        Some(st.f_bavail as u64 * st.f_frsize as u64)
    }
}

/// Почта Google-аккаунта, уже известная системе (по папке приложения Google Drive для компьютера).
pub fn google_hint() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    std::fs::read_dir(PathBuf::from(home).join("Library/CloudStorage")).ok()?.flatten()
        .find_map(|e| e.file_name().to_string_lossy().strip_prefix("GoogleDrive-").map(|x| x.to_string()))
        .filter(|x| x.contains('@'))
}
