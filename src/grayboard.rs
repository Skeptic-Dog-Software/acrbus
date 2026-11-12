//! A Grayboard team server, with acrbus as an OAuth client of its own: the
//! server's operator registers it (`GRAYBOARD_OAUTH_CLIENTS`, client ID
//! `acrbus`, with `sessionSource`), you sign in once with
//! `acrbus grayboard login`, and the daemon holds one hosted MCP session
//! over streamable HTTP. That session binds a name of yours, sends and reads
//! direct messages and holds claims.
//!
//! Only the daemon uses the grant after sign-in: refresh tokens rotate, so
//! two processes refreshing one would sign it out.

use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::{APP_NAME, Config, GrayboardConfig, Paths};
use crate::util::now_ms;

/// The `_meta` key a `sessionSource` client's raw session ID goes under.
pub const SESSION_META: &str = "io.github.nolnoch.grayboard/sessionID";
/// Beside it, how the session receives messages: acrbus polls.
pub const DELIVERY_META: &str = "io.github.nolnoch.grayboard/delivery";
const PROTOCOL: &str = "2025-06-18";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const SCOPES: &str = "grayboard:tools grayboard:delivery";
/// How long a sign-in waits for the browser.
const LOGIN_WAIT: Duration = Duration::from_secs(15 * 60);
/// An access token this close to expiry is refreshed first.
const REFRESH_EARLY_MS: i64 = 60_000;

/// What went wrong, so the daemon can tell a server that's away from one
/// that won't have us.
#[derive(Debug)]
pub enum Trouble {
    /// The server can't be reached now, or asked us to come back later.
    Unreachable(String),
    /// No sign-in, or one the server no longer takes.
    SignedOut(String),
    /// The server refused a tool call, saying why.
    Refused(String),
}

impl fmt::Display for Trouble {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Trouble::Unreachable(why) => write!(f, "Grayboard is unreachable: {why}"),
            Trouble::SignedOut(why) => write!(f, "not signed in to Grayboard: {why}"),
            Trouble::Refused(why) => write!(f, "Grayboard refused: {why}"),
        }
    }
}

impl std::error::Error for Trouble {}

pub fn trouble(e: &anyhow::Error) -> Option<&Trouble> {
    e.downcast_ref::<Trouble>()
}

/// Where acrbus keeps what it holds of Grayboard.
pub fn dir(paths: &Paths) -> PathBuf {
    paths.state.join("grayboard")
}

fn grant_path(paths: &Paths) -> PathBuf {
    dir(paths).join("grant.json")
}

/// The `[grayboard]` section, if config.toml has one.
pub fn settings(paths: &Paths) -> Option<GrayboardConfig> {
    Config::load(&paths.config).ok()?.grayboard
}

/// A sign-in: the server's endpoints, and the tokens it issued. Kept 0600,
/// and never printed.
#[derive(Clone, Serialize, Deserialize)]
pub struct Grant {
    pub server: String,
    pub resource: String,
    pub issuer: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
    pub client_id: String,
    pub scope: String,
    access: String,
    access_expires_at: i64,
    #[serde(default)]
    refresh: Option<String>,
    pub signed_in_at: i64,
}

impl fmt::Debug for Grant {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Grant")
            .field("server", &self.server)
            .field("client_id", &self.client_id)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

pub fn read_grant(paths: &Paths) -> Option<Grant> {
    serde_json::from_slice(&fs::read(grant_path(paths)).ok()?).ok()
}

/// Writes `bytes` to `path` so only you can read it, replacing it whole.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()))
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

fn write_grant(paths: &Paths, grant: &Grant) -> Result<()> {
    write_private(&grant_path(paths), &serde_json::to_vec_pretty(grant)?)
}

// Encodings.

fn random(n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

/// Base64url without padding, as PKCE wants.
pub fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..=chunk.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// The PKCE S256 challenge of `verifier`.
pub fn challenge(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

/// Percent-encodes all but RFC 3986's unreserved characters.
pub fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            ) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A query string's pairs, decoded.
pub fn query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

/// An URL's origin and path, without a trailing slash.
fn split_url(url: &str) -> Result<(String, String)> {
    let url = url.trim_end_matches('/');
    let rest = url
        .strip_prefix("https://")
        .map(|r| ("https://", r))
        .or_else(|| url.strip_prefix("http://").map(|r| ("http://", r)))
        .with_context(|| format!("{url} isn't an http(s) URL"))?;
    let (host, path) = rest.1.split_once('/').unwrap_or((rest.1, ""));
    if host.is_empty() {
        bail!("{url} has no host");
    }
    let path = if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    };
    Ok((format!("{}{host}", rest.0), path))
}

// HTTP.

fn agent(overall: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(overall))
        .http_status_as_error(false)
        .build()
        .into()
}

/// A transport failure as `Trouble::Unreachable`.
fn unreachable(e: ureq::Error) -> anyhow::Error {
    anyhow!(Trouble::Unreachable(e.to_string()))
}

struct Reply {
    status: u16,
    content_type: String,
    session: Option<String>,
    text: String,
}

fn read(mut resp: ureq::http::Response<ureq::Body>) -> Result<Reply> {
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let (content_type, session) = (
        header("content-type").unwrap_or_default(),
        header("mcp-session-id"),
    );
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().map_err(unreachable)?;
    Ok(Reply {
        status,
        content_type,
        session,
        text,
    })
}

fn get_json(http: &ureq::Agent, url: &str) -> Result<Value> {
    let r = read(http.get(url).call().map_err(unreachable)?)?;
    if r.status != 200 {
        bail!("{url}: HTTP {}", r.status);
    }
    serde_json::from_str(&r.text).with_context(|| format!("reading {url}"))
}

/// The server's resource and its authorization server's endpoints, from
/// their published metadata.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub resource: String,
    pub issuer: String,
    pub authorization: String,
    pub token: String,
    pub revocation: Option<String>,
}

pub fn discover(http: &ureq::Agent, server: &str) -> Result<Endpoints> {
    let (origin, prefix) = split_url(server)?;
    let prm = get_json(
        http,
        &format!("{origin}/.well-known/oauth-protected-resource{prefix}/mcp"),
    )?;
    let resource = prm["resource"]
        .as_str()
        .context("the server's metadata names no resource")?
        .to_string();
    let issuer = prm["authorization_servers"][0]
        .as_str()
        .context("the server's metadata names no authorization server")?
        .to_string();
    let (io, ip) = split_url(&issuer)?;
    let asm = get_json(
        http,
        &format!("{io}/.well-known/oauth-authorization-server{ip}"),
    )?;
    let field = |k: &str| asm[k].as_str().map(str::to_string);
    Ok(Endpoints {
        resource,
        issuer,
        authorization: field("authorization_endpoint").context("no authorization endpoint")?,
        token: field("token_endpoint").context("no token endpoint")?,
        revocation: field("revocation_endpoint"),
    })
}

fn token_request(http: &ureq::Agent, endpoint: &str, body: &str) -> Result<Value> {
    let r = read(
        http.post(endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .send(body.as_bytes())
            .map_err(unreachable)?,
    )?;
    let v: Value = serde_json::from_str(&r.text).unwrap_or(Value::Null);
    if r.status == 200 {
        return Ok(v);
    }
    if matches!(r.status, 429 | 502..=504) {
        bail!(Trouble::Unreachable(format!(
            "token endpoint: HTTP {}",
            r.status
        )));
    }
    let code = v["error"].as_str().unwrap_or("unknown error");
    bail!(Trouble::SignedOut(format!(
        "the token endpoint refused ({}: {code})",
        r.status
    )))
}

/// The grant a token response gives.
fn granted(paths: &Paths, base: &Grant, v: &Value) -> Result<Grant> {
    let access = v["access_token"]
        .as_str()
        .context("the token response has no access token")?
        .to_string();
    let expires = v["expires_in"].as_i64().unwrap_or(3600);
    let mut g = base.clone();
    g.access = access;
    g.access_expires_at = now_ms() + expires * 1000;
    if let Some(r) = v["refresh_token"].as_str() {
        g.refresh = Some(r.to_string());
    }
    if let Some(s) = v["scope"].as_str() {
        g.scope = s.to_string();
    }
    write_grant(paths, &g)?;
    Ok(g)
}

// Signing in.

/// The page the browser lands on after the server sends it back.
const DONE: &str = "<!doctype html><title>acrbus</title><p>Signed in. You can close this tab.";

/// Prints the sign-in link, and with `browse` opens it too.
pub fn show_link(browse: bool) -> impl Fn(&str) {
    move |url: &str| {
        println!("Open this link to sign in:\n\n{url}\n");
        if browse {
            let opener = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            let _ = std::process::Command::new(opener)
                .arg(url)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}

/// `acrbus grayboard login`: the authorization code flow with PKCE and a
/// loopback redirect on a port of its own. `present` shows the link to
/// open, as `show_link` does.
pub fn login(paths: &Paths, cfg: &GrayboardConfig, present: &dyn Fn(&str)) -> Result<Grant> {
    let http = agent(Duration::from_secs(30));
    let ep = discover(&http, &cfg.server)
        .with_context(|| format!("finding {}'s sign-in", cfg.server))?;
    let verifier = b64url(&random(32)?);
    let state = b64url(&random(16)?);
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|e| anyhow!("listening for the sign-in: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .context("no loopback port")?
        .port();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let url = format!(
        "{}?{}",
        ep.authorization,
        form(&[
            ("response_type", "code"),
            ("client_id", &cfg.client_id),
            ("redirect_uri", &redirect),
            ("scope", SCOPES),
            ("resource", &ep.resource),
            ("state", &state),
            ("code_challenge", &challenge(&verifier)),
            ("code_challenge_method", "S256"),
        ])
    );
    present(&url);
    let deadline = std::time::Instant::now() + LOGIN_WAIT;
    let code = loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            bail!("the sign-in timed out: nothing came back to {redirect}");
        }
        let Some(req) = server.recv_timeout(left)? else {
            continue;
        };
        let (path, q) = req.url().split_once('?').unwrap_or((req.url(), ""));
        if path != "/callback" {
            let _ = req.respond(tiny_http::Response::empty(404));
            continue;
        }
        let pairs = query(q);
        let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        if get("state").as_deref() != Some(state.as_str()) {
            let _ = req.respond(
                tiny_http::Response::from_string("Unexpected sign-in.").with_status_code(400),
            );
            continue;
        }
        if let Some(err) = get("error") {
            let _ = req.respond(tiny_http::Response::from_string(format!(
                "Sign-in refused: {err}"
            )));
            bail!("the sign-in was refused: {err}");
        }
        let Some(code) = get("code") else {
            let _ = req.respond(tiny_http::Response::empty(400));
            continue;
        };
        let html = tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8")
            .map_err(|()| anyhow!("header"))?;
        let _ = req.respond(tiny_http::Response::from_string(DONE).with_header(html));
        break code;
    };
    let v = token_request(
        &http,
        &ep.token,
        &form(&[
            ("grant_type", "authorization_code"),
            ("client_id", &cfg.client_id),
            ("code", &code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect),
            ("resource", &ep.resource),
        ]),
    )?;
    let base = Grant {
        server: cfg.server.clone(),
        resource: ep.resource.clone(),
        issuer: ep.issuer.clone(),
        token_endpoint: ep.token.clone(),
        revocation_endpoint: ep.revocation.clone(),
        client_id: cfg.client_id.clone(),
        scope: SCOPES.to_string(),
        access: String::new(),
        access_expires_at: 0,
        refresh: None,
        signed_in_at: now_ms(),
    };
    granted(paths, &base, &v)
}

/// `acrbus grayboard logout`: revokes the refresh token, if the server
/// takes revocations, and forgets the grant.
pub fn logout(paths: &Paths) -> Result<bool> {
    let Some(g) = read_grant(paths) else {
        return Ok(false);
    };
    if let (Some(endpoint), Some(refresh)) = (&g.revocation_endpoint, &g.refresh) {
        let http = agent(Duration::from_secs(20));
        let body = form(&[
            ("token", refresh),
            ("token_type_hint", "refresh_token"),
            ("client_id", &g.client_id),
        ]);
        if let Err(e) = http
            .post(endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send(body.as_bytes())
        {
            eprintln!("{APP_NAME}: couldn't revoke the sign-in on the server: {e}");
        }
    }
    write_private(
        &grant_path(paths),
        &serde_json::to_vec_pretty(&json!({ "signed_out_at": now_ms(), "server": g.server }))?,
    )?;
    Ok(true)
}

// The session.

/// The raw session ID this machine's acrbus reports, kept across restarts
/// so its hosted session, and the name it holds, carry over.
fn raw_session(paths: &Paths) -> Result<String> {
    let path = dir(paths).join("session-id");
    if let Ok(id) = fs::read_to_string(&path) {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    let id = format!("acrbus-{}", crate::util::new_id());
    write_private(&path, id.as_bytes())?;
    Ok(id)
}

/// One hosted MCP session: the daemon's, which is the only one.
pub struct Session {
    paths: Paths,
    http: ureq::Agent,
    grant: Grant,
    raw: String,
    mcp: Option<String>,
    next: u64,
    /// The account signed in, once `whoami` has said.
    pub principal: Option<String>,
}

impl Session {
    pub fn open(paths: &Paths) -> Result<Session> {
        let grant = read_grant(paths).ok_or_else(|| {
            anyhow!(Trouble::SignedOut(format!(
                "run `{APP_NAME} grayboard login`"
            )))
        })?;
        Ok(Session {
            paths: paths.clone(),
            http: agent(Duration::from_secs(30)),
            raw: raw_session(paths)?,
            grant,
            mcp: None,
            next: 1,
            principal: None,
        })
    }

    fn meta(&self) -> Value {
        json!({
            SESSION_META: self.raw,
            DELIVERY_META: { "mode": "poll", "source": "acrbus" },
        })
    }

    /// A current access token, refreshed first if it's about to lapse.
    fn access(&mut self) -> Result<String> {
        if self.grant.access_expires_at - REFRESH_EARLY_MS > now_ms() {
            return Ok(self.grant.access.clone());
        }
        self.refresh()?;
        Ok(self.grant.access.clone())
    }

    fn refresh(&mut self) -> Result<()> {
        // A sign-in since we read it is the newer one.
        if let Some(on_disk) = read_grant(&self.paths)
            && on_disk.signed_in_at > self.grant.signed_in_at
        {
            self.grant = on_disk;
            if self.grant.access_expires_at - REFRESH_EARLY_MS > now_ms() {
                return Ok(());
            }
        }
        let Some(refresh) = self.grant.refresh.clone() else {
            bail!(Trouble::SignedOut(format!(
                "the sign-in has lapsed: run `{APP_NAME} grayboard login`"
            )));
        };
        let v = token_request(
            &self.http,
            &self.grant.token_endpoint,
            &form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &self.grant.client_id),
                ("refresh_token", &refresh),
                ("resource", &self.grant.resource),
            ]),
        )?;
        // Saved before it's used: the old refresh token no longer works.
        self.grant = granted(&self.paths, &self.grant, &v)?;
        Ok(())
    }

    /// Posts one JSON-RPC message; its answer, if it has an ID.
    fn post(&mut self, message: &Value) -> Result<Reply> {
        let token = self.access()?;
        let mut req = self
            .http
            .post(&self.grant.resource)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL);
        if let Some(s) = &self.mcp {
            req = req.header("Mcp-Session-Id", s);
        }
        read(
            req.send(serde_json::to_vec(message)?.as_slice())
                .map_err(unreachable)?,
        )
    }

    fn initialize(&mut self) -> Result<()> {
        self.mcp = None;
        let id = self.id();
        let init = json!({
            "jsonrpc": "2.0", "id": id, "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": { "name": APP_NAME.to_string(), "version": VERSION },
                "_meta": self.meta(),
            },
        });
        let r = self.post(&init)?;
        let answer = answer(&r, id)?;
        if answer.get("error").is_some() {
            bail!("initialize: {}", answer["error"]);
        }
        self.mcp = r.session;
        let done = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        self.post(&done)?;
        Ok(())
    }

    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// Calls `tool`; its result, parsed from JSON where it is JSON.
    pub fn call(&mut self, tool: &str, args: Value) -> Result<Value> {
        if self.mcp.is_none() {
            self.initialize()?;
        }
        // One fresh token, and one fresh session, per call.
        let (mut refreshed, mut reopened) = (false, false);
        loop {
            let id = self.id();
            let msg = json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": tool, "arguments": args, "_meta": self.meta() },
            });
            let r = self.post(&msg)?;
            // A session the server has dropped, as after its restart.
            let gone = r.status == 404 || r.status == 400 && r.text.contains("not initialized");
            match r.status {
                401 if !refreshed => {
                    refreshed = true;
                    self.refresh()?;
                    continue;
                }
                _ if gone && !reopened => {
                    reopened = true;
                    self.initialize()?;
                    continue;
                }
                401 => bail!(Trouble::SignedOut(format!(
                    "the server no longer takes the sign-in: run `{APP_NAME} grayboard login`"
                ))),
                403 => bail!(Trouble::SignedOut(format!(
                    "the sign-in lacks access ({}): run `{APP_NAME} grayboard login`",
                    r.text.chars().take(200).collect::<String>()
                ))),
                429 | 502..=504 => bail!(Trouble::Unreachable(format!("HTTP {}", r.status))),
                _ => {}
            }
            let a = answer(&r, id)?;
            if let Some(err) = a.get("error") {
                bail!(Trouble::Refused(
                    err["message"].as_str().unwrap_or("error").to_string()
                ));
            }
            return tool_result(&a["result"]);
        }
    }
}

/// The JSON-RPC answer with ID `id` in a reply, as JSON or as server-sent
/// events.
fn answer(r: &Reply, id: u64) -> Result<Value> {
    if !(200..300).contains(&r.status) {
        bail!(
            "HTTP {}: {}",
            r.status,
            r.text.chars().take(300).collect::<String>()
        );
    }
    let messages: Vec<Value> = if r.content_type.starts_with("text/event-stream") {
        r.text
            .split("\n\n")
            .filter_map(|event| {
                let data: Vec<&str> = event
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect();
                (!data.is_empty())
                    .then(|| serde_json::from_str(&data.join("\n")).ok())
                    .flatten()
            })
            .collect()
    } else {
        vec![serde_json::from_str(&r.text).context("reading the server's answer")?]
    };
    messages
        .into_iter()
        .find(|m| m["id"].as_u64() == Some(id))
        .with_context(|| format!("the server's answer had no reply to request {id}"))
}

/// A tool's result: its text, parsed as JSON where it is.
fn tool_result(result: &Value) -> Result<Value> {
    let text: String = result["content"]
        .as_array()
        .map(|c| {
            c.iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let value = result
        .get("structuredContent")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| serde_json::from_str(&text).unwrap_or(Value::String(text.clone())));
    if result["isError"].as_bool() == Some(true) {
        let why = value["message"]
            .as_str()
            .map(str::to_string)
            .unwrap_or(text);
        bail!(Trouble::Refused(why));
    }
    Ok(value)
}

// The daemon's service.

/// What `acrbus grayboard status` shows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub server: Option<String>,
    pub name: Option<String>,
    pub signed_in: bool,
    /// The handle it holds, such as `you@example.com/acr`.
    pub handle: Option<String>,
    pub problem: Option<String>,
    /// When it last heard from the server.
    pub last_ok: Option<i64>,
}

pub struct Service {
    pub paths: Paths,
    pub session: Mutex<Option<Session>>,
    pub status: Mutex<Status>,
    pub cfg: GrayboardConfig,
}

static SERVICE: OnceLock<Arc<Service>> = OnceLock::new();

pub fn service() -> Option<Arc<Service>> {
    SERVICE.get().cloned()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Service {
    /// Runs `f` with the session, opening it and binding the name first.
    pub fn with<T>(&self, f: impl FnOnce(&mut Session) -> Result<T>) -> Result<T> {
        let mut guard = lock(&self.session);
        if guard.is_none() {
            *guard = Some(Session::open(&self.paths)?);
        }
        let session = guard.as_mut().expect("opened");
        if session.principal.is_none() {
            bind(session, &self.cfg.name)?;
        }
        let out = f(session);
        if let Err(e) = &out
            && matches!(trouble(e), Some(Trouble::SignedOut(_)))
        {
            *guard = None;
        }
        out
    }

    pub fn status(&self) -> Status {
        lock(&self.status).clone()
    }
}

/// Holds `name`, creating it the first time; notes the account it's under.
pub fn bind(session: &mut Session, name: &str) -> Result<()> {
    let who = session.call("whoami", json!({}))?;
    let held: Vec<String> = who["held"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|h| h.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let principal = who["principal"].as_str().map(str::to_string);
    let mine = principal.as_ref().map(|p| format!("{p}/{name}"));
    if mine.as_ref().is_none_or(|m| !held.contains(m)) {
        session
            .call("bind", json!({ "name": name, "create": true }))
            .map_err(|e| match trouble(&e) {
                Some(Trouble::Refused(why)) => {
                    anyhow!(Trouble::Refused(format!("couldn't bind {name}: {why}")))
                }
                _ => e,
            })?;
    }
    session.principal = match principal {
        Some(p) => Some(p),
        None => session.call("whoami", json!({}))?["principal"]
            .as_str()
            .map(str::to_string),
    };
    Ok(())
}

/// One pass of the service's work, given the session and the log.
pub type Tick = fn(&Paths, &mut Session, &dyn Fn(String)) -> Result<()>;

/// Starts the daemon's Grayboard service: a thread that, every
/// `poll_secs`, keeps the name bound and runs `tick` with the session.
/// `log` goes to the daemon's log.
pub fn start(paths: Paths, cfg: GrayboardConfig, tick: Tick, log: Box<dyn Fn(String) + Send>) {
    let svc = Arc::new(Service {
        paths: paths.clone(),
        session: Mutex::new(None),
        status: Mutex::new(Status {
            server: Some(cfg.server.clone()),
            name: Some(cfg.name.clone()),
            ..Status::default()
        }),
        cfg: cfg.clone(),
    });
    if SERVICE.set(svc.clone()).is_err() {
        return;
    }
    thread::spawn(move || {
        let every = Duration::from_secs(cfg.poll_secs.max(5));
        let mut said: Option<String> = None;
        loop {
            let result = svc.with(|s| tick(&paths, s, &*log).map(|()| s.principal.clone()));
            let mut st = lock(&svc.status);
            match result {
                Ok(principal) => {
                    st.signed_in = true;
                    st.handle = principal.map(|p| format!("{p}/{}", cfg.name));
                    st.problem = None;
                    st.last_ok = Some(now_ms());
                    if said.take().is_some() {
                        log(format!("grayboard: back in touch with {}", cfg.server));
                    }
                }
                Err(e) => {
                    let why = format!("{e:#}");
                    st.signed_in = !matches!(trouble(&e), Some(Trouble::SignedOut(_)));
                    if !st.signed_in {
                        st.handle = None;
                    }
                    st.problem = Some(why.clone());
                    if said.as_deref() != Some(why.as_str()) {
                        log(format!("grayboard: {why}"));
                        said = Some(why);
                    }
                }
            }
            drop(st);
            thread::sleep(every);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_and_encodings_are_as_the_rfcs_say() {
        // RFC 7636 appendix B.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(enc("a b/c:d~"), "a%20b%2Fc%3Ad~");
        assert_eq!(
            query("code=a%2Fb&state=x+y&e="),
            [
                ("code".into(), "a/b".into()),
                ("state".into(), "x y".into()),
                ("e".into(), String::new())
            ]
        );
        assert_eq!(
            split_url("https://gb.example.com/coord/").unwrap(),
            ("https://gb.example.com".into(), "/coord".into())
        );
        assert_eq!(
            split_url("http://127.0.0.1:9").unwrap(),
            ("http://127.0.0.1:9".into(), String::new())
        );
    }

    #[test]
    fn an_answer_comes_from_json_or_server_sent_events() {
        let sse = Reply {
            status: 200,
            content_type: "text/event-stream".into(),
            session: None,
            text: "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"note\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"ok\\\":true}\"}]}}\n\n".into(),
        };
        let a = answer(&sse, 7).unwrap();
        assert_eq!(tool_result(&a["result"]).unwrap(), json!({ "ok": true }));
        let refused = json!({ "content": [{ "type": "text", "text": "{\"code\":\"alias_held\",\"message\":\"held\"}" }], "isError": true });
        let e = tool_result(&refused).unwrap_err();
        assert!(matches!(trouble(&e), Some(Trouble::Refused(m)) if m == "held"));
    }
}
