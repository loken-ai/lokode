//! Signing in to a tool server over HTTP, the way the protocol's authorization says.
//!
//! A server that wants a signed-in user answers 401 and names the document that says
//! which authorization server it trusts. That server's own document gives the addresses to
//! sign in at and to get a token from. A client unknown to it registers itself when it
//! offers registration, or uses the `client-id` the configuration gives.
//!
//! The person signs in in their browser. The browser comes back to a port this process
//! listens on, on the loopback address only, with a code bound to a secret that never left
//! the process. The code is traded for a token, which is kept in the data directory,
//! readable by the user alone, and renewed when it runs out.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long the browser has to come back once the address to sign in at is given.
pub const SIGN_IN_WAIT: Duration = Duration::from_secs(300);

/// A token this close to its end is renewed before it is used.
const RENEW_BEFORE: Duration = Duration::from_secs(60);

/// Random bytes in the secret the code is bound to, and in the state the browser returns.
const SECRET_BYTES: usize = 32;

/// What is kept for one server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub token_endpoint: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Seconds since the epoch when the token stops working, when the server said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl Tokens {
    /// Whether the token is past its end, or about to be.
    fn stale(&self) -> bool {
        self.expires_at
            .is_some_and(|at| now() + RENEW_BEFORE.as_secs() >= at)
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Where the tokens are kept, by server url.
fn store_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("mcp-tokens.json"))
}

fn read_store() -> BTreeMap<String, Tokens> {
    store_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_store(store: &BTreeMap<String, Tokens>) -> Result<()> {
    let path = store_path().ok_or_else(|| anyhow!("there is no data directory"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(store)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options
        .open(&path)
        .with_context(|| path.display().to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(text.as_bytes())?;
    Ok(())
}

/// What is kept for the server at `url`.
pub fn stored(url: &str) -> Option<Tokens> {
    read_store().remove(url)
}

/// Keep `tokens` for the server at `url`.
pub fn keep(url: &str, tokens: Tokens) -> Result<()> {
    let mut store = read_store();
    store.insert(url.to_string(), tokens);
    write_store(&store)
}

/// Forget what is kept for the server at `url`. Says whether there was anything.
pub fn forget(url: &str) -> Result<bool> {
    let mut store = read_store();
    let had = store.remove(url).is_some();
    if had {
        write_store(&store)?;
    }
    Ok(had)
}

/// A token for the server at `url` that works now: the one kept, renewed first when it
/// has run out. `None` when nothing is kept.
pub async fn bearer(http: &reqwest::Client, url: &str) -> Result<Option<String>> {
    let Some(tokens) = stored(url) else {
        return Ok(None);
    };
    if !tokens.stale() {
        return Ok(Some(tokens.access_token));
    }
    let renewed = renew(http, &tokens).await?;
    let token = renewed.access_token.clone();
    keep(url, renewed)?;
    Ok(Some(token))
}

/// Trade the refresh token for a new access token.
async fn renew(http: &reqwest::Client, tokens: &Tokens) -> Result<Tokens> {
    let Some(refresh) = &tokens.refresh_token else {
        bail!("the sign-in has run out and cannot be renewed")
    };
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh.clone()),
        ("client_id", tokens.client_id.clone()),
    ];
    if let Some(secret) = &tokens.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let got = post_form(http, &tokens.token_endpoint, &form).await?;
    let mut renewed = read_token(tokens.clone(), &got)?;
    // A server that does not hand out a new refresh token keeps the old one valid.
    if renewed.refresh_token.is_none() {
        renewed.refresh_token = Some(refresh.clone());
    }
    Ok(renewed)
}

/// What the configuration says about signing in to one server.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// The client to sign in as, for an authorization server that does not register.
    pub client_id: Option<String>,
    /// What to ask for, when the server's document does not say.
    pub scopes: Vec<String>,
}

/// The addresses of an authorization server.
#[derive(Debug, Clone, PartialEq)]
struct Endpoints {
    authorization: String,
    token: String,
    registration: Option<String>,
}

/// A sign-in under way: the address to open, and what finishes it.
pub struct Pending {
    /// Where the person signs in.
    pub url: String,
    server_url: String,
    listener: tokio::net::TcpListener,
    redirect: String,
    verifier: String,
    state: String,
    client_id: String,
    client_secret: Option<String>,
    token_endpoint: String,
    http: reqwest::Client,
}

/// Find out where to sign in to the server at `server_url`, and get ready for the browser
/// to come back.
pub async fn begin(server_url: &str, settings: &Settings) -> Result<Pending> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let server = url::Url::parse(server_url).context("the server's url")?;
    let (issuer, scopes_supported) = protected_resource(&http, &server).await;
    let issuer = issuer.unwrap_or_else(|| origin(&server));
    let endpoints = authorization_server(&http, &issuer).await?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let redirect = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );

    let (client_id, client_secret) = match &settings.client_id {
        Some(id) => (id.clone(), None),
        None => {
            let Some(registration) = &endpoints.registration else {
                bail!(
                    "{issuer} does not register clients: give the server a client-id in the \
                     configuration"
                )
            };
            register(&http, registration, &redirect).await?
        }
    };

    let verifier = random();
    let state = random();
    let scopes = match settings.scopes.is_empty() {
        true => scopes_supported,
        false => settings.scopes.clone(),
    };
    let mut url = url::Url::parse(&endpoints.authorization).context("the sign-in address")?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect)
            .append_pair("code_challenge", &challenge(&verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("resource", server_url);
        if !scopes.is_empty() {
            q.append_pair("scope", &scopes.join(" "));
        }
    }
    Ok(Pending {
        url: url.to_string(),
        server_url: server_url.to_string(),
        listener,
        redirect,
        verifier,
        state,
        client_id,
        client_secret,
        token_endpoint: endpoints.token,
        http,
    })
}

impl Pending {
    /// Wait for the browser to come back, trade its code for a token and keep it.
    pub async fn finish(self) -> Result<()> {
        let code = tokio::time::timeout(SIGN_IN_WAIT, self.callback())
            .await
            .map_err(|_| {
                anyhow!(
                    "nobody signed in within {} minutes",
                    SIGN_IN_WAIT.as_secs() / 60
                )
            })??;
        let mut form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code),
            ("redirect_uri", self.redirect.clone()),
            ("client_id", self.client_id.clone()),
            ("code_verifier", self.verifier.clone()),
            ("resource", self.server_url.clone()),
        ];
        if let Some(secret) = &self.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        let got = post_form(&self.http, &self.token_endpoint, &form).await?;
        let blank = Tokens {
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            token_endpoint: self.token_endpoint.clone(),
            access_token: String::new(),
            refresh_token: None,
            expires_at: None,
        };
        keep(&self.server_url, read_token(blank, &got)?)
    }

    /// The code the browser brings back, once one comes with the right state.
    async fn callback(&self) -> Result<String> {
        loop {
            let (mut stream, _) = self.listener.accept().await?;
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let target = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or_default()
                .to_string();
            let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
                continue;
            };
            if url.path() != "/callback" {
                let _ = answer(&mut stream, "404 Not Found", "nothing here").await;
                continue;
            }
            let pairs: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
            if pairs.get("state") != Some(&self.state) {
                let _ = answer(
                    &mut stream,
                    "400 Bad Request",
                    "this sign-in is not lokode's",
                )
                .await;
                continue;
            }
            if let Some(error) = pairs.get("error") {
                let _ = answer(
                    &mut stream,
                    "200 OK",
                    "Signing in failed. You can close this page.",
                )
                .await;
                let about = pairs.get("error_description").cloned().unwrap_or_default();
                bail!("the sign-in was refused: {error} {about}");
            }
            let Some(code) = pairs.get("code") else {
                let _ = answer(&mut stream, "400 Bad Request", "no code came back").await;
                continue;
            };
            let _ = answer(
                &mut stream,
                "200 OK",
                "Signed in. You can close this page and go back to lokode.",
            )
            .await;
            return Ok(code.clone());
        }
    }
}

async fn answer(stream: &mut tokio::net::TcpStream, status: &str, text: &str) -> Result<()> {
    let body = format!("<!doctype html><meta charset=utf-8><title>lokode</title><p>{text}</p>");
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(reply.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

/// The scheme, host and port of `url`.
fn origin(url: &url::Url) -> String {
    url.origin().ascii_serialization()
}

/// `{origin}/.well-known/{name}{path}`, the way a document about a url with a path is
/// placed, then without the path.
fn well_known(url: &url::Url, name: &str) -> Vec<String> {
    let base = origin(url);
    let path = url.path().trim_end_matches('/');
    let mut out = Vec::new();
    if !path.is_empty() {
        out.push(format!("{base}/.well-known/{name}{path}"));
    }
    out.push(format!("{base}/.well-known/{name}"));
    out
}

/// The authorization server the server at `server` trusts, and the scopes it names, from
/// its protected resource document. Found through the 401 it answers, or where such a
/// document is placed.
async fn protected_resource(
    http: &reqwest::Client,
    server: &url::Url,
) -> (Option<String>, Vec<String>) {
    let mut places = Vec::new();
    if let Ok(resp) = http
        .post(server.as_str())
        .header("Accept", "application/json, text/event-stream")
        .json(&serde_json::json!({"jsonrpc": "2.0", "id": 0, "method": "ping"}))
        .send()
        .await
    {
        if let Some(named) = resp
            .headers()
            .get("WWW-Authenticate")
            .and_then(|v| v.to_str().ok())
            .and_then(resource_metadata)
        {
            places.push(named);
        }
    }
    places.extend(well_known(server, "oauth-protected-resource"));
    for place in places {
        let Some(doc) = get_json(http, &place).await else {
            continue;
        };
        let issuer = doc
            .get("authorization_servers")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(|a| a.as_str())
            .map(str::to_string);
        let scopes = doc
            .get("scopes_supported")
            .and_then(|s| s.as_array())
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect();
        return (issuer, scopes);
    }
    (None, Vec::new())
}

/// The `resource_metadata` a `WWW-Authenticate` header names.
fn resource_metadata(header: &str) -> Option<String> {
    let at = header.find("resource_metadata=")? + "resource_metadata=".len();
    let rest = &header[at..];
    let value = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        None => rest.split([',', ' ']).next()?,
    };
    (!value.is_empty()).then(|| value.to_string())
}

/// The addresses of the authorization server `issuer`, from its document, or the ones an
/// earlier revision of the protocol placed under it when it has none.
async fn authorization_server(http: &reqwest::Client, issuer: &str) -> Result<Endpoints> {
    let url = url::Url::parse(issuer).context("the authorization server's url")?;
    let places = well_known(&url, "oauth-authorization-server")
        .into_iter()
        .chain(well_known(&url, "openid-configuration"));
    for place in places {
        let Some(doc) = get_json(http, &place).await else {
            continue;
        };
        let text = |k: &str| doc.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let (Some(authorization), Some(token)) =
            (text("authorization_endpoint"), text("token_endpoint"))
        else {
            continue;
        };
        let methods = doc.get("code_challenge_methods_supported");
        let s256 = methods
            .and_then(|m| m.as_array())
            .is_none_or(|m| m.iter().any(|v| v == "S256"));
        if !s256 {
            bail!("{issuer} does not bind a sign-in to a secret the way lokode does (S256)");
        }
        return Ok(Endpoints {
            authorization,
            token,
            registration: text("registration_endpoint"),
        });
    }
    let base = origin(&url);
    Ok(Endpoints {
        authorization: format!("{base}/authorize"),
        token: format!("{base}/token"),
        registration: Some(format!("{base}/register")),
    })
}

/// Register lokode with the authorization server, for the browser to come back to
/// `redirect`. Gives the client id, and its secret when the server issues one.
async fn register(
    http: &reqwest::Client,
    endpoint: &str,
    redirect: &str,
) -> Result<(String, Option<String>)> {
    let resp = http
        .post(endpoint)
        .json(&serde_json::json!({
            "client_name": "lokode",
            "redirect_uris": [redirect],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        }))
        .send()
        .await
        .context("registering with the authorization server")?;
    let status = resp.status();
    let doc: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("the authorization server would not register lokode: {status} {doc}");
    }
    let id = doc
        .get("client_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("the authorization server registered lokode without an id"))?;
    let secret = doc
        .get("client_secret")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok((id.to_string(), secret))
}

async fn get_json(http: &reqwest::Client, url: &str) -> Option<Value> {
    let resp = http.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

async fn post_form(http: &reqwest::Client, url: &str, form: &[(&str, String)]) -> Result<Value> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter().map(|(k, v)| (*k, v.as_str())))
        .finish();
    let resp = http
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .await
        .context("reaching the authorization server")?;
    let status = resp.status();
    let doc: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let why = doc
            .get("error_description")
            .or_else(|| doc.get("error"))
            .and_then(|e| e.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| doc.to_string());
        bail!("the authorization server answered {status}: {why}");
    }
    Ok(doc)
}

/// `tokens` with what a token answer gives.
fn read_token(mut tokens: Tokens, got: &Value) -> Result<Tokens> {
    tokens.access_token = got
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("the authorization server gave no token"))?
        .to_string();
    if let Some(refresh) = got.get("refresh_token").and_then(|v| v.as_str()) {
        tokens.refresh_token = Some(refresh.to_string());
    }
    tokens.expires_at = got
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .map(|s| now() + s);
    Ok(tokens)
}

/// Random bytes, written for a url.
pub(crate) fn random() -> String {
    let mut bytes = [0u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).expect("the system gives no random bytes");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// What the sign-in address carries in place of the secret: its SHA-256, written for a url.
fn challenge(verifier: &str) -> String {
    use sha2::Digest;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier))
}

/// Open `url` in the person's browser: the one `BROWSER` names, or the system's.
pub fn open_browser(url: &str) -> bool {
    let program = std::env::var("BROWSER").ok().filter(|b| !b.is_empty());
    let program = program.unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            "open".into()
        } else {
            "xdg-open".into()
        }
    });
    std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The challenge is the one the standard gives for its example secret.
    #[test]
    fn the_challenge_is_the_secret_hashed() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_ne!(random(), random());
    }

    /// The document is named by the 401, quoted or not, and placed by the server's path.
    #[test]
    fn where_the_documents_are() {
        assert_eq!(
            resource_metadata(
                r#"Bearer error="invalid_token", resource_metadata="https://a.example/.well-known/oauth-protected-resource""#
            )
            .as_deref(),
            Some("https://a.example/.well-known/oauth-protected-resource")
        );
        assert_eq!(
            resource_metadata("Bearer resource_metadata=https://b.example/m, x=1").as_deref(),
            Some("https://b.example/m")
        );
        assert_eq!(resource_metadata("Bearer realm=x"), None);
        let url = url::Url::parse("https://mcp.example.com:8443/team/mcp").unwrap();
        assert_eq!(
            well_known(&url, "oauth-protected-resource"),
            [
                "https://mcp.example.com:8443/.well-known/oauth-protected-resource/team/mcp",
                "https://mcp.example.com:8443/.well-known/oauth-protected-resource"
            ]
        );
    }

    /// A token answer fills the token, keeps a refresh token it does not replace, and
    /// dates the end.
    #[test]
    fn a_token_answer_is_read() {
        let blank = Tokens {
            client_id: "c".into(),
            client_secret: None,
            token_endpoint: "t".into(),
            access_token: String::new(),
            refresh_token: Some("old".into()),
            expires_at: None,
        };
        let got = serde_json::json!({"access_token": "a", "expires_in": 30});
        let tokens = read_token(blank.clone(), &got).unwrap();
        assert_eq!(tokens.access_token, "a");
        assert_eq!(tokens.refresh_token.as_deref(), Some("old"));
        assert!(tokens.stale(), "30 seconds left is within the margin");
        assert!(read_token(blank, &serde_json::json!({})).is_err());
    }
}
