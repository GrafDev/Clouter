//! Google Drive: OAuth (loopback + PKCE), токены в Keychain, resumable upload.
use std::{path::Path, process::Command, sync::Arc, time::{Duration, SystemTime, UNIX_EPOCH}};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpListener, sync::Mutex};

const SCOPE: &str = "https://www.googleapis.com/auth/drive.file";

/// Базовый адрес Drive API. Для проверки без аккаунта можно подменить переменной CLOUTER_DRIVE_BASE.
fn api_base() -> String { std::env::var("CLOUTER_DRIVE_BASE").unwrap_or_else(|_| "https://www.googleapis.com".to_string()) }
const SERVICE: &str = "app.googletor.google";

// ---------- Keychain (через системную утилиту security) ----------

pub fn keychain_get(account: &str) -> Option<String> {
    let o = Command::new("security")
        .args(["find-generic-password", "-s", SERVICE, "-a", account, "-w"])
        .output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

pub fn keychain_set(account: &str, value: &str) -> Result<()> {
    let s = Command::new("security")
        .args(["add-generic-password", "-U", "-s", SERVICE, "-a", account, "-w", value])
        .status()?;
    if !s.success() { bail!("E_KEYCHAIN"); }
    Ok(())
}

pub fn keychain_delete(account: &str) {
    let _ = Command::new("security")
        .args(["delete-generic-password", "-s", SERVICE, "-a", account]).output();
}

// ---------- OAuth ----------

#[derive(Serialize, Deserialize, Clone, Default)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
}

/// HTTP-клиент с таймаутами: без них оборванное соединение (сон Mac, смена сети) зависает навсегда.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(900))
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(20))
        .build().unwrap_or_default()
}

pub(crate) fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() }

pub struct DriveAuth {
    client_id: String,
    client_secret: String,
    http: reqwest::Client,
    tokens: Mutex<Tokens>,
    /// Уже созданные папки: путь -> id (чтобы файлы одной раздачи не плодили дубли папок).
    folders: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// Создание папок идёт по одной, иначе параллельные файлы плодят дубли.
    folder_lock: tokio::sync::Mutex<()>,
}

#[derive(Deserialize)]
struct TokenResp { access_token: String, refresh_token: Option<String>, expires_in: u64 }

pub(crate) fn rand_string(n: usize) -> String {
    use std::io::Read;
    let mut b = vec![0u8; n];
    std::fs::File::open("/dev/urandom").unwrap().read_exact(&mut b).unwrap();
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

pub(crate) fn pct(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{:02X}", b),
    }).collect()
}

impl DriveAuth {
    /// Данные клиента вводит пользователь (или берутся из Keychain).
    pub fn saved() -> Option<Arc<Self>> {
        let id = keychain_get("client_id")?;
        let secret = keychain_get("client_secret")?;
        let tokens: Tokens = serde_json::from_str(&keychain_get("tokens")?).ok()?;
        Some(Arc::new(Self { client_id: id, client_secret: secret, http: crate::drive::http_client(),
            tokens: Mutex::new(tokens), folders: Default::default(), folder_lock: Default::default() }))
    }

    /// Ключ приложения, зашитый при сборке (GOOGLETOR_CLIENT_ID / GOOGLETOR_CLIENT_SECRET)
    /// или лежащий в ~/Library/Application Support/Googletor/google_client.json.
    pub fn embedded() -> Option<(String, String)> {
        if let (Some(i), Some(s)) = (option_env!("GOOGLETOR_CLIENT_ID"), option_env!("GOOGLETOR_CLIENT_SECRET")) {
            if !i.is_empty() { return Some((i.into(), s.into())); }
        }
        let p = std::path::PathBuf::from(std::env::var("HOME").ok()?)
            .join("Library/Application Support/Googletor/google_client.json");
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
        let v = v.get("installed").unwrap_or(&v);
        Some((v["client_id"].as_str()?.into(), v["client_secret"].as_str()?.into()))
    }

    pub fn saved_email() -> Option<String> { keychain_get("email") }

    pub async fn fetch_email(&self) -> Option<String> {
        let tok = self.token().await.ok()?;
        let v: serde_json::Value = self.http.get("https://www.googleapis.com/drive/v3/about?fields=user(emailAddress)")
            .bearer_auth(tok).send().await.ok()?.json().await.ok()?;
        let e = v["user"]["emailAddress"].as_str()?.to_string();
        let _ = keychain_set("email", &e);
        Some(e)
    }

    /// Только для проверки на тестовом сервере (CLOUTER_DRIVE_BASE): готовый токен без входа в Google.
    pub fn fake() -> Arc<Self> {
        Arc::new(Self { client_id: "test".into(), client_secret: "test".into(), http: crate::drive::http_client(),
            tokens: Mutex::new(Tokens { access_token: "fake".into(), refresh_token: "fake".into(), expires_at: now() + 1_000_000_000 }),
            folders: Default::default(), folder_lock: Default::default() })
    }

    pub fn signed_in() -> bool { keychain_get("tokens").is_some() }

    pub fn sign_out() {
        for a in ["client_id", "client_secret", "tokens", "email"] { keychain_delete(a); }
    }

    /// Вход через браузер. client_id/secret — введённые пользователем.
    pub async fn sign_in(client_id: &str, client_secret: &str, hint: Option<&str>) -> Result<Arc<Self>> {
        let emb = Self::embedded();
        let (client_id, client_secret) = match (client_id.is_empty(), &emb) {
            (true, Some((i, s))) => (i.as_str(), s.as_str()),
            _ => (client_id, client_secret),
        };
        if client_id.is_empty() || client_secret.is_empty() {
            bail!("E_NO_KEY");
        }
        if !client_id.ends_with(".apps.googleusercontent.com") {
            bail!("E_BAD_CLIENT_ID");
        }
        let my_gen = crate::oauth::LOGIN_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let redirect = format!("http://127.0.0.1:{port}");
        let verifier = rand_string(32);
        let challenge = {
            // S256 без внешних крейтов хеша: используем plain, Google это допускает для loopback
            verifier.clone()
        };
        let state = rand_string(8);
        let hint = hint.map(|h| format!("&login_hint={}", pct(h))).unwrap_or_default();
        let url = format!(
            "https://accounts.google.com/o/oauth2/v2/auth?{hint}&client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&state={}&code_challenge={}&code_challenge_method=plain",
            pct(client_id), pct(&redirect), pct(SCOPE), state, challenge);
        let _ = Command::new("open").arg(&url).status();

        let mut sock = crate::oauth::accept_login(&listener, my_gen).await?;
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await?;
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        let line = req.lines().next().unwrap_or("");
        let q = line.split_whitespace().nth(1).unwrap_or("").split('?').nth(1).unwrap_or("");
        let get = |k: &str| q.split('&').find_map(|p| p.strip_prefix(&format!("{k}=")).map(|v| v.to_string()));
        let lang = req.lines().find_map(|l| l.strip_prefix("Accept-Language:").or_else(|| l.strip_prefix("accept-language:"))).unwrap_or("en").to_string();
        let body = crate::oauth::success_page(&lang);
        let _ = Command::new("open").args(["-b", "app.clouter"]).status();
        let _ = sock.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).as_bytes()).await;
        if get("state").as_deref() != Some(&state) { bail!("E_LOGIN_STATE"); }
        let code = get("code").ok_or_else(|| anyhow::anyhow!("E_LOGIN_DENIED"))?;

        let http = crate::drive::http_client();
        let r: TokenResp = http.post("https://oauth2.googleapis.com/token")
            .form(&[("code", code.as_str()), ("client_id", client_id), ("client_secret", client_secret),
                ("redirect_uri", redirect.as_str()), ("grant_type", "authorization_code"),
                ("code_verifier", verifier.as_str())])
            .send().await?.error_for_status()?.json().await?;
        let tokens = Tokens {
            access_token: r.access_token,
            refresh_token: r.refresh_token.context("no refresh_token")?,
            expires_at: now() + r.expires_in,
        };
        keychain_set("client_id", client_id)?;
        keychain_set("client_secret", client_secret)?;
        keychain_set("tokens", &serde_json::to_string(&tokens)?)?;
        let me = Arc::new(Self { client_id: client_id.into(), client_secret: client_secret.into(), http,
            tokens: Mutex::new(tokens), folders: Default::default(), folder_lock: Default::default() });
        me.fetch_email().await;
        Ok(me)
    }

    async fn token(&self) -> Result<String> {
        let mut t = self.tokens.lock().await;
        if t.expires_at > now() + 60 { return Ok(t.access_token.clone()); }
        let r: TokenResp = self.http.post("https://oauth2.googleapis.com/token")
            .form(&[("client_id", self.client_id.as_str()), ("client_secret", self.client_secret.as_str()),
                ("refresh_token", t.refresh_token.as_str()), ("grant_type", "refresh_token")])
            .send().await?.error_for_status()?.json().await?;
        t.access_token = r.access_token;
        t.expires_at = now() + r.expires_in;
        let _ = keychain_set("tokens", &serde_json::to_string(&*t)?);
        Ok(t.access_token.clone())
    }

    /// Создаёт папку внутри parent (None = корень Мой диск), возвращает id.
    pub async fn create_folder(&self, parent: Option<&str>, name: &str) -> Result<String> {
        let tok = self.token().await?;
        let mut body = serde_json::json!({"name": name, "mimeType": "application/vnd.google-apps.folder"});
        if let Some(p) = parent { body["parents"] = serde_json::json!([p]); }
        let v = send_retry(|| self.http.post(format!("{}/drive/v3/files?fields=id", api_base()))
            .bearer_auth(&tok).json(&body)).await?;
        let v: serde_json::Value = check(v).await?.json().await?;
        Ok(v["id"].as_str().context("no folder id")?.to_string())
    }

    /// Создаёт цепочку папок внутри base один раз (дальше берёт из кэша). Новые id попадают в created.
    async fn ensure_folder(&self, base: Option<String>, parts: &[String], created: &mut Vec<String>) -> Result<Option<String>> {
        let _guard = self.folder_lock.lock().await;
        let mut parent = base.clone();
        for k in 0..parts.len() {
            let key = format!("{}|{}", base.as_deref().unwrap_or("root"), parts[..=k].join("/"));
            let cached = self.folders.lock().unwrap().get(&key).cloned();
            let id = match cached {
                Some(id) => id,
                None => {
                    // Папка с таким именем уже есть — используем её, а не создаём копию.
                    let id = match self.find_folder(parent.as_deref(), &parts[k]).await? {
                        Some(id) => id,
                        None => {
                            let id = self.create_folder(parent.as_deref(), &parts[k]).await?;
                            created.push(id.clone());
                            id
                        }
                    };
                    self.folders.lock().unwrap().insert(key, id.clone());
                    id
                }
            };
            parent = Some(id);
        }
        Ok(parent)
    }

    pub fn folders_snapshot(&self) -> std::collections::HashMap<String, String> { self.folders.lock().unwrap().clone() }
    pub fn restore_folders(&self, m: std::collections::HashMap<String, String>) { self.folders.lock().unwrap().extend(m); }

    /// Забыть кэш папок (после удаления или отмены, чтобы не писать в корзину).
    pub fn forget_folders(&self) { self.folders.lock().unwrap().clear(); }

    /// Прерывает незавершённую загрузку (сессия resumable upload).
    pub async fn cancel_session(&self, url: &str) {
        let _ = self.http.delete(url).send().await;
    }

    /// Ищет в папке parent (None = корень) файл или папку с таким именем (среди созданных Clouter).
    pub async fn find_by_name(&self, parent: Option<&str>, name: &str) -> Result<Option<String>> {
        let tok = self.token().await?;
        let esc = |s: &str| s.replace('\\', "\\\\").replace('\'', "\\'");
        let q = format!("name = '{}' and '{}' in parents and trashed=false", esc(name), esc(parent.unwrap_or("root")));
        let r = self.http.get(format!("{}/drive/v3/files", api_base())).bearer_auth(tok)
            .query(&[("q", q.as_str()), ("fields", "files(id)"), ("pageSize", "1")]).send().await?;
        let v: serde_json::Value = check(r).await?.json().await?;
        Ok(v["files"][0]["id"].as_str().map(|s| s.to_string()))
    }

    /// Ищет папку (не файл) с таким именем внутри parent (None = корень).
    async fn find_folder(&self, parent: Option<&str>, name: &str) -> Result<Option<String>> {
        let tok = self.token().await?;
        let esc = |s: &str| s.replace('\\', "\\\\").replace('\'', "\\'");
        let q = format!("name = '{}' and '{}' in parents and mimeType='application/vnd.google-apps.folder' and trashed=false", esc(name), esc(parent.unwrap_or("root")));
        let r = self.http.get(format!("{}/drive/v3/files", api_base())).bearer_auth(tok)
            .query(&[("q", q.as_str()), ("fields", "files(id)"), ("pageSize", "1")]).send().await?;
        let v: serde_json::Value = check(r).await?.json().await?;
        Ok(v["files"][0]["id"].as_str().map(|s| s.to_string()))
    }

    /// Отправляет файл или папку в корзину Google Drive (можно восстановить).
    pub async fn trash(&self, id: &str) -> Result<()> {
        let tok = self.token().await?;
        let r = self.http.patch(format!("{}/drive/v3/files/{id}", api_base()))
            .bearer_auth(tok).json(&serde_json::json!({"trashed": true})).send().await?;
        check(r).await?;
        self.forget_folders();
        Ok(())
    }

    /// Папки внутри parent ("root" = Мой диск). С правом drive.file видны только папки, созданные приложением.
    pub async fn list_folders(&self, parent: &str) -> Result<Vec<(String, String)>> {
        let tok = self.token().await?;
        let q = format!("'{}' in parents and mimeType='application/vnd.google-apps.folder' and trashed=false", parent);
        let r = send_retry(|| self.http.get(format!("{}/drive/v3/files", api_base()))
            .bearer_auth(&tok)
            .query(&[("q", q.as_str()), ("fields", "files(id,name)"), ("orderBy", "name"), ("pageSize", "200")])).await?;
        let v: serde_json::Value = check(r).await?.json().await?;
        Ok(v["files"].as_array().map(|a| a.iter().filter_map(|f|
            Some((f["id"].as_str()?.to_string(), f["name"].as_str()?.to_string()))).collect()).unwrap_or_default())
    }
}

/// Повторяет запрос при сетевых сбоях и ответах 5xx/429 (до 6 раз с нарастающей паузой).
async fn send_retry(make: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response> {
    let mut attempt = 0u32;
    loop {
        match make().send().await {
            Ok(r) if (r.status().is_server_error() || matches!(r.status().as_u16(), 408 | 429)) && attempt < 6 => {}
            Ok(r) => return Ok(r),
            Err(_) if attempt < 6 => {}
            Err(e) => return Err(e.into()),
        }
        attempt += 1;
        tokio::time::sleep(Duration::from_secs(2u64.pow(attempt.min(5)))).await;
    }
}

/// Превращает ответ с ошибкой Google в понятный код: E_API_DISABLED или E_GOOGLE|причина|сообщение.
async fn check(r: reqwest::Response) -> Result<reqwest::Response> {
    if r.status().is_success() { return Ok(r); }
    let status = r.status().as_u16();
    let body: serde_json::Value = r.json().await.unwrap_or_default();
    let reason = body["error"]["errors"][0]["reason"].as_str()
        .or(body["error"]["details"][0]["reason"].as_str()).unwrap_or("").to_string();
    let msg = body["error"]["message"].as_str().unwrap_or("").replace('|', "/");
    if matches!(reason.as_str(), "accessNotConfigured" | "SERVICE_DISABLED") || msg.contains("has not been used") {
        bail!("E_API_DISABLED");
    }
    bail!("E_GOOGLE|{status} {reason}|{msg}")
}

// ---------- Resumable upload ----------

pub struct ResumableUpload {
    auth: Arc<DriveAuth>,
    pub session_url: String,
    total: u64,
    /// Папки, созданные этим файлом, и id самого файла после завершения (нужны для отмены).
    pub new_folders: Vec<String>,
    pub file_id: Option<String>,
}

/// Что Google знает о незавершённой загрузке.
pub enum SessionState {
    /// Принято байт: следующий нужно слать с этого смещения.
    Next(u64),
    /// Файл уже полностью принят (id файла).
    Done(Option<String>),
    /// Сессия устарела или удалена: начинать файл заново.
    Expired,
}

impl ResumableUpload {
    /// Спрашивает у Google, сколько байт сессия уже приняла.
    pub async fn query(auth: &Arc<DriveAuth>, url: &str, total: u64) -> Result<SessionState> {
        let tok = auth.token().await?;
        let r = send_retry(|| auth.http.put(url).bearer_auth(&tok)
            .header("Content-Range", format!("bytes */{total}")).header("Content-Length", "0")).await?;
        let st = r.status().as_u16();
        match st {
            308 => {
                let next = r.headers().get("range").and_then(|h| h.to_str().ok())
                    .and_then(|h| h.rsplit('-').next()).and_then(|n| n.trim().parse::<u64>().ok()).map(|n| n + 1).unwrap_or(0);
                Ok(SessionState::Next(next))
            }
            200 | 201 => {
                let v: serde_json::Value = r.json().await.unwrap_or_default();
                Ok(SessionState::Done(v["id"].as_str().map(|x| x.to_string())))
            }
            404 | 410 => Ok(SessionState::Expired),
            _ => { check(r).await?; Ok(SessionState::Expired) }
        }
    }

    /// Продолжает уже открытую сессию.
    pub fn from_session(auth: Arc<DriveAuth>, session_url: String, total: u64) -> Self {
        Self { auth, session_url, total, new_folders: vec![], file_id: None }
    }

    pub async fn start(auth: Arc<DriveAuth>, rel: &Path, size: u64, base: Option<String>) -> Result<Self> {
        let comps: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
        let (file, dirs) = comps.split_last().context("empty path")?;
        let mut new_folders = vec![];
        let mut retried = false;
        let r = loop {
            let parent = auth.ensure_folder(base.clone(), dirs, &mut new_folders).await?;
            let mut meta = serde_json::json!({"name": file});
            if let Some(p) = parent { meta["parents"] = serde_json::json!([p]); }
            let tok = auth.token().await?;
            let r = send_retry(|| auth.http.post(format!("{}/upload/drive/v3/files?uploadType=resumable", api_base()))
                .bearer_auth(&tok)
                .header("X-Upload-Content-Length", size.to_string())
                .json(&meta)).await?;
            // Закэшированная папка могла быть удалена на Диске (404) — забываем кэш и создаём заново.
            if r.status().as_u16() == 404 && !retried && !dirs.is_empty() {
                retried = true;
                auth.forget_folders();
                continue;
            }
            break check(r).await?;
        };
        let session_url = r.headers().get("location").context("no Location header")?.to_str()?.to_string();
        Ok(Self { auth, session_url, total: size, new_folders, file_id: None })
    }

    /// Порция кратна 256 КиБ, кроме последней. При сбое спрашивает у Google, сколько он принял, и досылает остаток.
    pub async fn put(&mut self, offset: u64, data: Vec<u8>) -> Result<()> {
        let chunk_end = offset + data.len() as u64;
        let mut start = offset;
        let mut attempt = 0u32;
        loop {
            let slice = data[(start - offset) as usize..].to_vec();
            let range = format!("bytes {start}-{}/{}", chunk_end - 1, self.total);
            let tok = self.auth.token().await?;
            let res = self.auth.http.put(&self.session_url)
                .bearer_auth(tok).header("Content-Range", &range).body(slice).send().await;
            match res {
                Ok(r) if r.status().as_u16() == 308 => {
                    // Google подтвердил приём; если принял меньше, досылаем остаток
                    let next = r.headers().get("range").and_then(|h| h.to_str().ok())
                        .and_then(|h| h.rsplit('-').next()).and_then(|n| n.trim().parse::<u64>().ok()).map(|n| n + 1).unwrap_or(chunk_end);
                    if next >= chunk_end { return Ok(()); }
                    if next < offset { bail!("E_UPLOAD_DESYNC"); }
                    start = next;
                    continue;
                }
                Ok(r) if r.status().is_success() => {
                    let v: serde_json::Value = r.json().await.unwrap_or_default();
                    self.file_id = v["id"].as_str().map(|x| x.to_string());
                    return Ok(());
                }
                Ok(r) if (r.status().is_server_error() || matches!(r.status().as_u16(), 408 | 429)) && attempt < 8 => {}
                Ok(r) => {
                    let st = r.status();
                    let body = r.text().await.unwrap_or_default();
                    // HTML-страницу ошибки Google в сообщение не тащим
                    let short = if body.trim_start().starts_with('<') { String::new() } else { body.chars().take(300).collect() };
                    bail!("Drive replied {}: {}", st, short)
                }
                Err(_) if attempt < 8 => {}
                Err(e) => return Err(e.into()),
            }
            // Сбой: узнаём у Google, докуда дошла загрузка, и продолжаем оттуда.
            attempt += 1;
            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt.min(5)))).await;
            match Self::query(&self.auth, &self.session_url, self.total).await {
                Ok(SessionState::Next(n)) if n >= chunk_end => return Ok(()),
                Ok(SessionState::Next(n)) if n >= offset => start = n,
                Ok(SessionState::Done(id)) => { self.file_id = id; return Ok(()); }
                Ok(SessionState::Expired) => bail!("E_UPLOAD_EXPIRED"),
                _ => {}
            }
        }
    }
}
