//! OneDrive (Microsoft Graph): список/создание папок и загрузка кусками (upload session).
use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};

use crate::{drive::pct, oauth::{self, OAuth}};

const AUTH: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
const TOKEN: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/token";
pub const PORT: u16 = 53683;
const SCOPE: &str = "Files.ReadWrite offline_access User.Read";
const GRAPH: &str = "https://graph.microsoft.com/v1.0";

pub struct OneDrive { oauth: Arc<OAuth> }

fn item_url(id: &str) -> String {
    if id == "root" || id.is_empty() { format!("{GRAPH}/me/drive/root") } else { format!("{GRAPH}/me/drive/items/{id}") }
}

impl OneDrive {
    pub fn saved() -> Option<Arc<Self>> { OAuth::saved("onedrive", TOKEN).map(|o| Arc::new(Self { oauth: o })) }
    pub fn embedded() -> Option<String> { oauth::embedded_client("onedrive") }

    pub async fn sign_in(client_id: &str, hint: Option<&str>) -> Result<Arc<Self>> {
        let id = if client_id.trim().is_empty() { Self::embedded().unwrap_or_default() } else { client_id.trim().to_string() };
        let o = OAuth::login("onedrive", AUTH, TOKEN, &id, SCOPE, &[("prompt", "select_account")], "localhost", PORT, hint).await?;
        let me = Arc::new(Self { oauth: o });
        me.fetch_email().await;
        Ok(me)
    }

    async fn fetch_email(&self) -> Option<String> {
        let t = self.oauth.token().await.ok()?;
        let v: serde_json::Value = self.oauth.http().get(format!("{GRAPH}/me")).bearer_auth(t).send().await.ok()?.json().await.ok()?;
        let e = v["mail"].as_str().or(v["userPrincipalName"].as_str())?.to_string();
        oauth::save_email("onedrive", &e);
        Some(e)
    }

    /// parent: id папки или "root". Возвращает (id, имя).
    pub async fn list_folders(&self, parent: &str) -> Result<Vec<(String, String)>> {
        let mut url = format!("{}/children?$select=id,name,folder&$top=200", item_url(parent));
        let mut out = vec![];
        loop {
            let t = self.oauth.token().await?;
            let r = self.oauth.http().get(&url).bearer_auth(t).send().await?;
            if !r.status().is_success() { bail!("OneDrive: {}", r.text().await.unwrap_or_default()); }
            let v: serde_json::Value = r.json().await?;
            for e in v["value"].as_array().cloned().unwrap_or_default() {
                if e.get("folder").is_some() {
                    out.push((e["id"].as_str().unwrap_or("").to_string(), e["name"].as_str().unwrap_or("").to_string()));
                }
            }
            match v["@odata.nextLink"].as_str() { Some(n) => url = n.to_string(), None => break }
        }
        out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        Ok(out)
    }

    pub async fn create_folder(&self, parent: &str, name: &str) -> Result<String> {
        let t = self.oauth.token().await?;
        let r = self.oauth.http().post(format!("{}/children", item_url(parent))).bearer_auth(t)
            .json(&serde_json::json!({"name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail"}))
            .send().await?;
        if !r.status().is_success() { bail!("OneDrive: {}", r.text().await.unwrap_or_default()); }
        let v: serde_json::Value = r.json().await?;
        Ok(v["id"].as_str().context("no folder id")?.to_string())
    }
}

pub struct Upload { http: reqwest::Client, url: String, total: u64 }

impl Upload {
    pub async fn start(od: Arc<OneDrive>, rel: &Path, size: u64, base: Option<String>) -> Result<Self> {
        let enc: Vec<String> = rel.components().map(|c| pct(&c.as_os_str().to_string_lossy())).collect();
        let base_url = match base.as_deref() {
            Some(id) if id != "root" && !id.is_empty() => format!("{GRAPH}/me/drive/items/{id}"),
            _ => format!("{GRAPH}/me/drive/root"),
        };
        let t = od.oauth.token().await?;
        let r = od.oauth.http().post(format!("{base_url}:/{}:/createUploadSession", enc.join("/")))
            .bearer_auth(t).json(&serde_json::json!({"item": {"@microsoft.graph.conflictBehavior": "replace"}}))
            .send().await?;
        if !r.status().is_success() { bail!("OneDrive: {}", r.text().await.unwrap_or_default()); }
        let v: serde_json::Value = r.json().await?;
        Ok(Self { http: od.oauth.http().clone(), url: v["uploadUrl"].as_str().context("no uploadUrl")?.to_string(), total: size })
    }

    /// Порция кратна 320 КиБ (кроме последней). Авторизация не нужна: ссылка сама подписана.
    pub async fn put(&mut self, offset: u64, data: Vec<u8>) -> Result<()> {
        let end = offset + data.len() as u64 - 1;
        let range = format!("bytes {offset}-{end}/{}", self.total);
        let mut attempt = 0;
        loop {
            let r = self.http.put(&self.url).header("Content-Range", &range).body(data.clone()).send().await;
            match r {
                Ok(r) if r.status().is_success() => return Ok(()), // 200/201/202
                Ok(r) if (r.status().is_server_error() || r.status().as_u16() == 429) && attempt < 6 => {}
                Ok(r) => bail!("OneDrive: {}", r.text().await.unwrap_or_default()),
                Err(_) if attempt < 6 => {}
                Err(e) => return Err(e.into()),
            }
            attempt += 1;
            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
        }
    }
}
