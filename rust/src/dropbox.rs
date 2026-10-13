//! Dropbox: список/создание папок и загрузка кусками (upload_session).
use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};

use crate::oauth::{self, json_ascii, OAuth};

const AUTH: &str = "https://www.dropbox.com/oauth2/authorize";
const TOKEN: &str = "https://api.dropboxapi.com/oauth2/token";
pub const PORT: u16 = 53682;
const SCOPE: &str = "files.content.write files.content.read files.metadata.read files.metadata.write account_info.read";

pub struct Dropbox { oauth: Arc<OAuth> }

impl Dropbox {
    pub fn saved() -> Option<Arc<Self>> { OAuth::saved("dropbox", TOKEN).map(|o| Arc::new(Self { oauth: o })) }
    pub fn embedded() -> Option<String> { oauth::embedded_client("dropbox") }

    pub async fn sign_in(app_key: &str) -> Result<Arc<Self>> {
        let key = if app_key.trim().is_empty() { Self::embedded().unwrap_or_default() } else { app_key.trim().to_string() };
        let o = OAuth::login("dropbox", AUTH, TOKEN, &key, SCOPE,
            &[("token_access_type", "offline")], "127.0.0.1", PORT, None).await?;
        let me = Arc::new(Self { oauth: o });
        me.fetch_email().await;
        Ok(me)
    }

    async fn fetch_email(&self) -> Option<String> {
        let t = self.oauth.token().await.ok()?;
        let v: serde_json::Value = self.oauth.http().post("https://api.dropboxapi.com/2/users/get_current_account")
            .bearer_auth(t).send().await.ok()?.json().await.ok()?;
        let e = v["email"].as_str()?.to_string();
        oauth::save_email("dropbox", &e);
        Some(e)
    }

    async fn rpc(&self, endpoint: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let t = self.oauth.token().await?;
        let r = self.oauth.http().post(format!("https://api.dropboxapi.com/2/{endpoint}"))
            .bearer_auth(t).json(&body).send().await?;
        if !r.status().is_success() { bail!("Dropbox: {}", r.text().await.unwrap_or_default()); }
        Ok(r.json().await?)
    }

    /// parent: "" — корень. Возвращает (путь, имя).
    pub async fn list_folders(&self, parent: &str) -> Result<Vec<(String, String)>> {
        let mut v = self.rpc("files/list_folder", serde_json::json!({"path": parent, "limit": 2000})).await?;
        let mut out = vec![];
        loop {
            for e in v["entries"].as_array().cloned().unwrap_or_default() {
                if e[".tag"] == "folder" {
                    out.push((e["path_display"].as_str().unwrap_or("").to_string(), e["name"].as_str().unwrap_or("").to_string()));
                }
            }
            if v["has_more"].as_bool() != Some(true) { break; }
            v = self.rpc("files/list_folder/continue", serde_json::json!({"cursor": v["cursor"]})).await?;
        }
        out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        Ok(out)
    }

    pub async fn create_folder(&self, parent: &str, name: &str) -> Result<String> {
        let path = format!("{}/{}", parent.trim_end_matches('/'), name);
        match self.rpc("files/create_folder_v2", serde_json::json!({"path": path, "autorename": false})).await {
            Ok(v) => Ok(v["metadata"]["path_display"].as_str().unwrap_or(&path).to_string()),
            Err(e) if e.to_string().contains("conflict") => Ok(path), // уже есть
            Err(e) => Err(e),
        }
    }
}

pub struct Upload { dbx: Arc<Dropbox>, session: String, path: String, total: u64 }

impl Upload {
    pub async fn start(dbx: Arc<Dropbox>, rel: &Path, size: u64, base: Option<String>) -> Result<Self> {
        let mut path = base.unwrap_or_default().trim_end_matches('/').to_string();
        for c in rel.components() { path.push('/'); path.push_str(&c.as_os_str().to_string_lossy()); }
        let t = dbx.oauth.token().await?;
        let r = dbx.oauth.http().post("https://content.dropboxapi.com/2/files/upload_session/start")
            .bearer_auth(t).header("Dropbox-API-Arg", json_ascii(&serde_json::json!({"close": false})))
            .header("Content-Type", "application/octet-stream").body(Vec::<u8>::new()).send().await?;
        if !r.status().is_success() { bail!("Dropbox: {}", r.text().await.unwrap_or_default()); }
        let v: serde_json::Value = r.json().await?;
        Ok(Self { dbx, session: v["session_id"].as_str().context("no session_id")?.to_string(), path, total: size })
    }

    pub async fn put(&mut self, offset: u64, data: Vec<u8>) -> Result<()> {
        let last = offset + data.len() as u64 >= self.total;
        let cursor = serde_json::json!({"session_id": self.session, "offset": offset});
        let (ep, arg) = if last {
            ("finish", serde_json::json!({"cursor": cursor, "commit": {"path": self.path, "mode": "overwrite", "autorename": false, "mute": true}}))
        } else {
            ("append_v2", serde_json::json!({"cursor": cursor, "close": false}))
        };
        let mut attempt = 0;
        loop {
            let t = self.dbx.oauth.token().await?;
            let r = self.dbx.oauth.http().post(format!("https://content.dropboxapi.com/2/files/upload_session/{ep}"))
                .bearer_auth(t).header("Dropbox-API-Arg", json_ascii(&arg))
                .header("Content-Type", "application/octet-stream").body(data.clone()).send().await;
            match r {
                Ok(r) if r.status().is_success() => return Ok(()),
                Ok(r) if (r.status().is_server_error() || r.status().as_u16() == 429) && attempt < 6 => {}
                Ok(r) => bail!("Dropbox: {}", r.text().await.unwrap_or_default()),
                Err(_) if attempt < 6 => {}
                Err(e) => return Err(e.into()),
            }
            attempt += 1;
            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
        }
    }
}
