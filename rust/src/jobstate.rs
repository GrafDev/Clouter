//! Сохранённое состояние загрузки: после закрытия приложения или сбоя её можно продолжить с того же места.
use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct JobState {
    pub id: String,
    /// magnet-ссылка или путь к сохранённой копии .torrent
    pub source: String,
    pub provider: String,
    pub folder: Option<String>,
    pub dest_name: String,
    pub window_gb: f64,
    pub name: String,
    pub total: u64,
    /// Первый файл, который ещё не загружен целиком.
    pub next_file: usize,
    /// Незавершённая загрузка текущего файла: адрес сессии Google, номер файла и сколько байт принято.
    pub session: Option<String>,
    pub session_file: usize,
    pub offset: u64,
    /// Новое имя верхней папки, если пользователь выбрал «оставить оба».
    pub rename_to: Option<String>,
    /// Новые имена для нескольких верхних элементов раздачи (старое имя -> новое).
    #[serde(default)]
    pub renames: HashMap<String, String>,
    /// Что эта загрузка создала в облаке (для отмены).
    pub created: Vec<String>,
    /// Уже созданные папки: путь -> id.
    pub folders: HashMap<String, String>,
    /// Сколько байт полностью загружено (завершённые файлы).
    pub done_bytes: u64,
    pub error: Option<String>,
}

pub fn dir() -> PathBuf {
    let d = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Application Support/Googletor/jobs");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn path(id: &str) -> PathBuf { dir().join(format!("{id}.json")) }

/// Запись через временный файл: при сбое питания остаётся целая прошлая версия.
pub fn save(st: &JobState) {
    let tmp = dir().join(format!("{}.tmp", st.id));
    if std::fs::write(&tmp, serde_json::to_vec(st).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, path(&st.id));
    }
}

pub fn remove(id: &str) {
    let _ = std::fs::remove_file(path(id));
    let _ = std::fs::remove_file(dir().join(format!("{id}.torrent")));
}

pub fn load_all() -> Vec<JobState> {
    let mut v: Vec<JobState> = std::fs::read_dir(dir()).map(|rd| rd.flatten()
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .filter_map(|e| std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice(&b).ok())).collect()).unwrap_or_default();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    v
}

/// Убирает забытые состояния старше 30 дней (сессия в облаке к тому времени давно устарела)
/// и копии .torrent, у которых нет состояния.
pub fn gc() {
    let month = std::time::Duration::from_secs(30 * 24 * 3600);
    let d = dir();
    let Ok(rd) = std::fs::read_dir(&d) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
        let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or("").to_string();
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|a| a > month).unwrap_or(false);
        match ext {
            "json" if old => remove(&stem),
            "torrent" if !d.join(format!("{stem}.json")).exists() => { let _ = std::fs::remove_file(&p); }
            "tmp" => { let _ = std::fs::remove_file(&p); }
            _ => {}
        }
    }
}

/// Копия .torrent рядом с состоянием: продолжить можно, даже если исходный файл переместили.
pub fn keep_torrent_copy(id: &str, src: &str) -> String {
    let dst = dir().join(format!("{id}.torrent"));
    if std::fs::copy(src, &dst).is_ok() { dst.to_string_lossy().to_string() } else { src.to_string() }
}
