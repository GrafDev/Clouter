//! Общий OAuth 2.0 (loopback + PKCE S256) для Dropbox и OneDrive. Токены хранятся в Keychain.
use std::{process::Command, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpListener, sync::Mutex};

use std::sync::atomic::{AtomicU64, Ordering};
use crate::drive::{keychain_delete, keychain_get, keychain_set, now, pct, rand_string};

/// Номер последней начатой попытки входа: более старые попытки сами завершаются.
pub static LOGIN_GEN: AtomicU64 = AtomicU64::new(0);

/// Ждёт переход браузера на loopback-адрес; прерывается, если запущена новая попытка входа.
pub async fn accept_login(listener: &TcpListener, my_gen: u64) -> Result<tokio::net::TcpStream> {
    for _ in 0..300 {
        match tokio::time::timeout(Duration::from_secs(1), listener.accept()).await {
            Ok(Ok((s, _))) => return Ok(s),
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => if LOGIN_GEN.load(Ordering::SeqCst) != my_gen { bail!("E_LOGIN_SUPERSEDED"); }
        }
    }
    bail!("E_LOGIN_TIMEOUT")
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
}

#[derive(Deserialize)]
struct TokenResp { access_token: String, refresh_token: Option<String>, expires_in: Option<u64> }

pub struct OAuth {
    name: &'static str,
    client_id: String,
    token_url: &'static str,
    http: reqwest::Client,
    tokens: Mutex<Tokens>,
}

/// Ключ приложения: зашит при сборке или лежит в ~/Library/Application Support/Googletor/clients.json
/// вида {"dropbox":{"client_id":"…"},"onedrive":{"client_id":"…"}}.
pub fn embedded_client(name: &str) -> Option<String> {
    let compiled = match name {
        "dropbox" => option_env!("CLOUTER_DROPBOX_KEY"),
        "onedrive" => option_env!("CLOUTER_ONEDRIVE_ID"),
        _ => None,
    };
    if let Some(c) = compiled.filter(|c| !c.is_empty()) { return Some(c.to_string()); }
    let p = std::path::PathBuf::from(std::env::var("HOME").ok()?)
        .join("Library/Application Support/Googletor/clients.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    Some(v[name]["client_id"].as_str()?.to_string())
}

impl OAuth {
    pub fn signed_in(name: &str) -> bool { keychain_get(&format!("{name}.tokens")).is_some() }

    pub fn sign_out(name: &str) {
        for k in ["client_id", "tokens", "email"] { keychain_delete(&format!("{name}.{k}")); }
    }

    pub fn saved(name: &'static str, token_url: &'static str) -> Option<Arc<Self>> {
        let client_id = keychain_get(&format!("{name}.client_id"))?;
        let tokens: Tokens = serde_json::from_str(&keychain_get(&format!("{name}.tokens"))?).ok()?;
        Some(Arc::new(Self { name, client_id, token_url, http: crate::drive::http_client(), tokens: Mutex::new(tokens) }))
    }

    pub fn http(&self) -> &reqwest::Client { &self.http }

    /// Вход через браузер. port — фиксированный порт loopback (его нужно зарегистрировать у провайдера).
    pub async fn login(
        name: &'static str, auth_url: &str, token_url: &'static str, client_id: &str,
        scope: &str, extra: &[(&str, &str)], redirect_host: &str, port: u16, hint: Option<&str>,
    ) -> Result<Arc<Self>> {
        if client_id.trim().is_empty() { bail!("E_NO_KEY"); }
        let my_gen = LOGIN_GEN.fetch_add(1, Ordering::SeqCst) + 1;
        tokio::time::sleep(Duration::from_millis(1200)).await; // дать старой попытке освободить порт
        let listener = TcpListener::bind(("127.0.0.1", port)).await
            .map_err(|_| anyhow::anyhow!("E_PORT_BUSY|{port}"))?;
        let redirect = format!("http://{redirect_host}:{port}");
        let verifier = rand_string(32);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = rand_string(8);
        let mut url = format!(
            "{auth_url}?client_id={}&redirect_uri={}&response_type=code&scope={}&state={state}&code_challenge={challenge}&code_challenge_method=S256",
            pct(client_id), pct(&redirect), pct(scope));
        for (k, v) in extra { url.push_str(&format!("&{k}={}", pct(v))); }
        if let Some(h) = hint { url.push_str(&format!("&login_hint={}", pct(h))); }
        let _ = Command::new("open").arg(&url).status();

        let mut sock = accept_login(&listener, my_gen).await?;
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await?;
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        let q = req.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("")
            .split('?').nth(1).unwrap_or("").to_string();
        let get = |k: &str| q.split('&').find_map(|p| p.strip_prefix(&format!("{k}=")).map(|v| v.to_string()));
        let lang = req.lines().find_map(|l| l.strip_prefix("Accept-Language:").or_else(|| l.strip_prefix("accept-language:"))).unwrap_or("en").to_string();
        let body = success_page(&lang);
        let _ = Command::new("open").args(["-b", "app.clouter"]).status();
        let _ = sock.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).as_bytes()).await;
        if get("state").as_deref() != Some(&state) { bail!("E_LOGIN_STATE"); }
        let code = get("code").ok_or_else(|| anyhow::anyhow!("E_LOGIN_DENIED"))?;

        let http = crate::drive::http_client();
        let r: TokenResp = http.post(token_url)
            .form(&[("code", code.as_str()), ("client_id", client_id), ("redirect_uri", redirect.as_str()),
                ("grant_type", "authorization_code"), ("code_verifier", verifier.as_str())])
            .send().await?.error_for_status()?.json().await?;
        let tokens = Tokens {
            access_token: r.access_token,
            refresh_token: r.refresh_token.context("no refresh_token")?,
            expires_at: now() + r.expires_in.unwrap_or(3600),
        };
        keychain_set(&format!("{name}.client_id"), client_id)?;
        keychain_set(&format!("{name}.tokens"), &serde_json::to_string(&tokens)?)?;
        Ok(Arc::new(Self { name, client_id: client_id.into(), token_url, http, tokens: Mutex::new(tokens) }))
    }

    pub async fn token(&self) -> Result<String> {
        let mut t = self.tokens.lock().await;
        if t.expires_at > now() + 60 { return Ok(t.access_token.clone()); }
        let r: TokenResp = self.http.post(self.token_url)
            .form(&[("client_id", self.client_id.as_str()), ("refresh_token", t.refresh_token.as_str()),
                ("grant_type", "refresh_token")])
            .send().await?.error_for_status()?.json().await?;
        t.access_token = r.access_token;
        if let Some(rt) = r.refresh_token { t.refresh_token = rt; }
        t.expires_at = now() + r.expires_in.unwrap_or(3600);
        let _ = keychain_set(&format!("{}.tokens", self.name), &serde_json::to_string(&*t)?);
        Ok(t.access_token.clone())
    }
}

/// Страница после входа: брендированная, на языке браузера, прячет длинный адрес и возвращает пользователя в Clouter.
pub fn success_page(accept_language: &str) -> String {
    let lang = accept_language.split(&[',', ';', '-'][..]).next().unwrap_or("en").trim().to_lowercase();
    let (title, text, back) = match lang.as_str() {
        "ru" => ("Готово", "Вход выполнен. Можно закрыть эту вкладку и вернуться в Clouter.", "Вернуться в Clouter"),
        "uk" => ("Готово", "Вхід виконано. Можна закрити цю вкладку й повернутися до Clouter.", "Повернутися до Clouter"),
        "de" => ("Fertig", "Anmeldung abgeschlossen. Sie können diesen Tab schließen und zu Clouter zurückkehren.", "Zurück zu Clouter"),
        "es" => ("Listo", "Sesión iniciada. Puedes cerrar esta pestaña y volver a Clouter.", "Volver a Clouter"),
        "fr" => ("Terminé", "Connexion réussie. Vous pouvez fermer cet onglet et revenir à Clouter.", "Retour à Clouter"),
        "it" => ("Fatto", "Accesso completato. Puoi chiudere questa scheda e tornare a Clouter.", "Torna a Clouter"),
        "pl" => ("Gotowe", "Zalogowano. Możesz zamknąć tę kartę i wrócić do Clouter.", "Wróć do Clouter"),
        "pt" => ("Pronto", "Login concluído. Você pode fechar esta aba e voltar ao Clouter.", "Voltar ao Clouter"),
        "tr" => ("Tamam", "Giriş yapıldı. Bu sekmeyi kapatıp Clouter’a dönebilirsiniz.", "Clouter’a dön"),
        "ja" => ("完了", "サインインしました。このタブを閉じてClouterに戻れます。", "Clouterに戻る"),
        "zh" => ("完成", "登录成功。您可以关闭此标签页并返回 Clouter。", "返回 Clouter"),
        _ => ("Done", "You are signed in. You can close this tab and return to Clouter.", "Back to Clouter"),
    };
    let icon = base64::engine::general_purpose::STANDARD.encode(include_bytes!("brand-icon.png"));
    format!(r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Clouter</title>
<style>
:root{{--bg:#f2f2f4;--card:#fff;--tx:#1d1d1f;--mut:#6e6e73;--ac:#c81e32}}
@media(prefers-color-scheme:dark){{:root{{--bg:#1e1e20;--card:#2a2a2d;--tx:#f5f5f7;--mut:#98989f;--ac:#fb7185}}}}
body{{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--tx);font:16px -apple-system,BlinkMacSystemFont,sans-serif}}
.c{{background:var(--card);border-radius:18px;padding:36px 44px;text-align:center;max-width:420px;box-shadow:0 10px 40px rgba(0,0,0,.18)}}
img{{width:84px;height:84px}}h1{{margin:14px 0 6px;font-size:24px}}p{{color:var(--mut);line-height:1.5;margin:0 0 20px}}
a{{display:inline-block;background:var(--ac);color:#fff;text-decoration:none;font-weight:700;padding:10px 22px;border-radius:10px;font-size:14px}}
.ok{{color:var(--ac)}}
</style></head><body><div class="c"><img src="data:image/png;base64,{icon}" alt="">
<h1><span class="ok">✓</span> {title}</h1><p>{text}</p><a href="javascript:window.close()">{back}</a></div>
<script>history.replaceState(null,'','/');</script></body></html>"#)
}

pub fn saved_email(name: &str) -> Option<String> { keychain_get(&format!("{name}.email")) }
pub fn save_email(name: &str, e: &str) { let _ = keychain_set(&format!("{name}.email"), e); }

/// JSON только из ASCII (нужен заголовку Dropbox-API-Arg): не-ASCII символы экранируются как \uXXXX.
pub fn json_ascii(v: &serde_json::Value) -> String {
    let s = v.to_string();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() { out.push(c); } else {
            let mut b = [0u16; 2];
            for u in c.encode_utf16(&mut b) { out.push_str(&format!("\\u{:04x}", u)); }
        }
    }
    out
}
