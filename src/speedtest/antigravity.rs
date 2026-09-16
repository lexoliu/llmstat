//! Antigravity backend: Google's Cloud Code Assist endpoints
//! (`daily-cloudcode-pa.googleapis.com`), the same API the Antigravity IDE and
//! CLI speak. Auth is an OAuth refresh-token flow using the credentials the
//! local `antigravity-cli` (or CLIProxyAPI) already holds.

use std::io::{BufRead, BufReader};
use std::time::Instant;

use anyhow::Context;
use serde_json::json;

use super::{ModelEntry, Provider, RunStats};

const BASE_DAILY: &str = "https://daily-cloudcode-pa.googleapis.com";
const BASE_PROD: &str = "https://cloudcode-pa.googleapis.com";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// The Antigravity CLI embeds its (public, installed-app) OAuth client
/// credentials in its binary. Rather than duplicating them here — they would
/// trip secret scanning — we extract them from the local install.
fn client_creds_from_binary() -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let home = std::env::home_dir().unwrap_or_else(|| "~".into());
    let candidates = [
        home.join(".local/bin/agy"),
        home.join(".antigravity/antigravity/bin/agy"),
        "/Applications/Antigravity.app/Contents/Resources/app/bin/antigravity".into(),
        "/Applications/Antigravity.app/Contents/Resources/app/extensions/antigravity/bin/language_server_macos_arm".into(),
    ];
    let mut ids = Vec::new();
    let mut secrets = Vec::new();
    for path in &candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        scan_patterns(&bytes, &mut ids, &mut secrets);
        if !ids.is_empty() && !secrets.is_empty() {
            tracing::debug!(bin = %path.display(), "antigravity client creds extracted");
            return Ok((ids, secrets));
        }
    }
    anyhow::bail!(
        "no OAuth client credentials found in a local Antigravity install (looked at {})",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Byte-scan a binary for `<digits>-<alnum>.apps.googleusercontent.com` client
/// ids and `GOCSPX-<…>` client secrets.
fn push_unique(v: &mut Vec<String>, s: &[u8]) {
    if let Ok(s) = std::str::from_utf8(s)
        && !v.iter().any(|x| x == s)
    {
        v.push(s.to_string());
    }
}

fn scan_patterns(bytes: &[u8], ids: &mut Vec<String>, secrets: &mut Vec<String>) {
    let is_id = |b: u8| b.is_ascii_alphanumeric() || b == b'-';
    let is_secret = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
    for (i, w) in bytes
        .windows(b".apps.googleusercontent.com".len())
        .enumerate()
    {
        if w != b".apps.googleusercontent.com" {
            continue;
        }
        let mut start = i;
        while start > 0 && is_id(bytes[start - 1]) {
            start -= 1;
        }
        // Neighbouring strings in a Go binary's table can be adjacent with no
        // separator, so validate by shape rather than trusting the run:
        // <digits>-<hash>.apps.googleusercontent.com, where <digits> is the
        // trailing digit run before the last dash and <hash> is alnum.
        let run = &bytes[start..i]; // everything before the suffix
        if let Some(dash) = run.iter().rposition(|&b| b == b'-') {
            let mut d = dash;
            while d > 0 && run[d - 1].is_ascii_digit() {
                d -= 1;
            }
            let hash = &run[dash + 1..];
            if dash - d >= 6
                && (20..=40).contains(&hash.len())
                && hash.iter().all(|b| b.is_ascii_alphanumeric())
            {
                push_unique(ids, &bytes[start + d..i + w.len()]);
            }
        }
    }
    // GOCSPX secrets are `GOCSPX-` + 28 chars. Adjacent strings may extend the
    // run, so cut at an embedded `GOCSPX-` first, then cap at the known length.
    for (i, w) in bytes.windows(b"GOCSPX-".len()).enumerate() {
        if w != b"GOCSPX-" {
            continue;
        }
        let mut end = i + w.len();
        while end < bytes.len() && is_secret(bytes[end]) {
            end += 1;
        }
        let mut run = &bytes[i..end];
        if let Some(p) = run[7..].windows(7).position(|w| w == b"GOCSPX-") {
            run = &run[..7 + p];
        }
        if run.len() > 35 {
            run = &run[..35];
        }
        if run.len() >= 24 {
            push_unique(secrets, run);
        }
    }
}

fn user_agent() -> String {
    // Antigravity gates models on a minimum client version; 2.9.1 is above it.
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin/arm64",
        ("macos", _) => "darwin/x64",
        ("linux", "aarch64") => "linux/arm64",
        ("linux", _) => "linux/x64",
        ("windows", _) => "windows/x64",
        (os, arch) => return format!("antigravity/2.9.1 {os}/{arch}"),
    };
    format!("antigravity/2.9.1 {platform}")
}

struct StoredCreds {
    access_token: String,
    refresh_token: Option<String>,
    /// RFC3339 expiry, if the file carries one.
    expiry: Option<String>,
    project: Option<String>,
    source: String,
}

fn read_creds() -> anyhow::Result<StoredCreds> {
    let home = std::env::home_dir().unwrap_or_else(|| "~".into());
    let cli = home.join(".gemini/antigravity-cli/antigravity-oauth-token");
    if let Ok(text) = std::fs::read_to_string(&cli)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(tok) = v.get("token")
    {
        let get = |k: &str| tok.get(k).and_then(|x| x.as_str()).map(String::from);
        if let Some(at) = get("access_token") {
            return Ok(StoredCreds {
                access_token: at,
                refresh_token: get("refresh_token"),
                expiry: get("expiry"),
                project: None,
                source: cli.display().to_string(),
            });
        }
    }
    // fallback: CLIProxyAPI account dumps
    let dir = home.join(".cli-proxy-api");
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut files: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("antigravity-") && n.ends_with(".json"))
            })
            .collect();
        files.sort();
        for p in files {
            if let Ok(text) = std::fs::read_to_string(&p)
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
                && let Some(at) = v.get("access_token").and_then(|x| x.as_str())
            {
                return Ok(StoredCreds {
                    access_token: at.to_string(),
                    refresh_token: v
                        .get("refresh_token")
                        .and_then(|x| x.as_str())
                        .map(String::from),
                    expiry: v.get("expired").and_then(|x| x.as_str()).map(String::from),
                    project: v
                        .get("project_id")
                        .and_then(|x| x.as_str())
                        .map(String::from),
                    source: p.display().to_string(),
                });
            }
        }
    }
    anyhow::bail!(
        "no antigravity credentials found (looked at {} and {}/*antigravity-*.json)",
        cli.display(),
        dir.display()
    )
}

fn expired(expiry: &Option<String>) -> bool {
    let Some(e) = expiry else { return true };
    chrono::DateTime::parse_from_rfc3339(e)
        .map(|t| t < chrono::Utc::now() + chrono::Duration::seconds(60))
        .unwrap_or(true)
}

/// Exchange the stored refresh_token for a fresh access_token. The
/// refresh_token is bound to the client that issued it, so only the matching
/// id/secret pair succeeds — wrong pairs fail fast with `invalid_client`.
fn refresh(refresh_token: &str) -> anyhow::Result<String> {
    let (ids, secrets) = client_creds_from_binary()?;
    let mut last_err = anyhow::anyhow!("no client credentials");
    for id in &ids {
        for secret in &secrets {
            let body = format!(
                "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
                id,
                secret,
                percent_encode(refresh_token)
            );
            match ureq::post(TOKEN_URL)
                .content_type("application/x-www-form-urlencoded")
                .send(body.as_bytes())
            {
                Ok(resp) => {
                    let text = resp.into_body().read_to_string()?;
                    let v: serde_json::Value = serde_json::from_str(&text)?;
                    if let Some(at) = v.get("access_token").and_then(|x| x.as_str()) {
                        return Ok(at.to_string());
                    }
                    last_err = anyhow::anyhow!("refresh response had no access_token");
                }
                Err(e) => last_err = anyhow::anyhow!("{e}"),
            }
        }
    }
    Err(last_err.context("oauth token refresh failed for every extracted client pair"))
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub struct Antigravity {
    token: String,
    project: String,
    creds_source: String,
}

impl Antigravity {
    pub fn new() -> anyhow::Result<Self> {
        let mut creds = read_creds()?;
        if expired(&creds.expiry) {
            let rt = creds
                .refresh_token
                .as_deref()
                .context("antigravity token expired and no refresh_token stored")?;
            creds.access_token = refresh(rt)?;
        }
        let mut me = Self {
            token: creds.access_token,
            project: String::new(),
            creds_source: creds.source,
        };
        me.project = me.load_project()?.or(creds.project).unwrap_or_default();
        if me.project.is_empty() {
            anyhow::bail!("no cloudaicompanionProject from loadCodeAssist and none stored");
        }
        tracing::debug!(creds = %me.creds_source, project = %me.project, "antigravity auth");
        Ok(me)
    }

    fn post(
        &self,
        base: &str,
        path: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<ureq::http::Response<ureq::Body>> {
        ureq::post(format!("{base}{path}"))
            .header("authorization", format!("Bearer {}", self.token))
            .header("user-agent", user_agent())
            .content_type("application/json")
            .send(body.to_string())
            .map_err(|e| anyhow::anyhow!("{path}: {e}"))
    }

    /// `loadCodeAssist` — returns the account's companion project id.
    fn load_project(&self) -> anyhow::Result<Option<String>> {
        let body = json!({"metadata": {
            "ideType": "ANTIGRAVITY",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI",
        }});
        for base in [BASE_DAILY, BASE_PROD] {
            match self.post(base, "/v1internal:loadCodeAssist", &body) {
                Ok(resp) => {
                    let text = resp.into_body().read_to_string()?;
                    let v: serde_json::Value = serde_json::from_str(&text)?;
                    return Ok(v
                        .get("cloudaicompanionProject")
                        .and_then(|x| x.as_str())
                        .map(String::from));
                }
                Err(e) => tracing::debug!("loadCodeAssist on {base}: {e:#}"),
            }
        }
        Ok(None)
    }
}

impl Provider for Antigravity {
    fn catalog(&mut self) -> anyhow::Result<Vec<ModelEntry>> {
        let resp = self
            .post(BASE_DAILY, "/v1internal:fetchAvailableModels", &json!({}))
            .or_else(|_| self.post(BASE_PROD, "/v1internal:fetchAvailableModels", &json!({})))
            .context("fetchAvailableModels")?;
        let text = resp.into_body().read_to_string()?;
        let v: serde_json::Value = serde_json::from_str(&text)?;
        let models = v
            .get("models")
            .and_then(|m| m.as_object())
            .context("fetchAvailableModels: no models map")?;
        let mut out: Vec<ModelEntry> = models
            .iter()
            .map(|(id, m)| ModelEntry {
                uid: id.clone(),
                label: m
                    .get("displayName")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                cost_hint: m
                    .get("quotaInfo")
                    .and_then(|q| q.get("remainingFraction"))
                    .and_then(|x| x.as_f64())
                    .map(|f| format!("quota {:.0}%", f * 100.0))
                    .unwrap_or_default(),
            })
            .collect();
        out.sort_by(|a, b| a.uid.cmp(&b.uid));
        Ok(out)
    }

    fn stream(&self, uid: &str, prompt: &str, max_tokens: Option<u64>) -> anyhow::Result<RunStats> {
        let mut request = json!({
            "sessionId": uuid::Uuid::new_v4().to_string(),
            "contents": [{"role": "user", "parts": [{"text": prompt}]}],
        });
        if let Some(mt) = max_tokens {
            request["generationConfig"] = json!({"maxOutputTokens": mt});
        }
        let body = json!({
            "project": self.project,
            "model": uid,
            "userAgent": "antigravity",
            "requestType": "agent",
            "requestId": uuid::Uuid::new_v4().to_string(),
            "request": request,
        });
        let t0 = Instant::now();
        let resp = self
            .post(
                BASE_DAILY,
                "/v1internal:streamGenerateContent?alt=sse",
                &body,
            )
            .or_else(|_| {
                self.post(
                    BASE_PROD,
                    "/v1internal:streamGenerateContent?alt=sse",
                    &body,
                )
            })
            .context("streamGenerateContent")?;
        let lines = BufReader::new(resp.into_body().into_reader()).lines();

        let mut ttft = None;
        let mut text_chars = 0usize;
        let (mut input, mut output, mut thoughts) = (0u64, 0u64, 0u64);
        let mut stop = String::new();
        for line in lines {
            let line = line.context("reading SSE stream")?;
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Ok(ev) = serde_json::from_str::<serde_json::Value>(data.trim()) else {
                continue;
            };
            let resp = ev.get("response").unwrap_or(&ev);
            if let Some(parts) = resp
                .get("candidates")
                .and_then(|c| c.as_array())
                .and_then(|c| c.first())
                .and_then(|c| c.get("content"))
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array())
            {
                for p in parts {
                    if let Some(t) = p.get("text").and_then(|x| x.as_str())
                        && !t.is_empty()
                    {
                        if ttft.is_none() {
                            ttft = Some(t0.elapsed());
                        }
                        if !p.get("thought").and_then(|x| x.as_bool()).unwrap_or(false) {
                            text_chars += t.chars().count();
                        }
                    }
                }
            }
            if let Some(fin) = resp
                .get("candidates")
                .and_then(|c| c.as_array())
                .and_then(|c| c.first())
                .and_then(|c| c.get("finishReason"))
                .and_then(|f| f.as_str())
            {
                stop = format!("finish={fin}");
            }
            if let Some(u) = resp.get("usageMetadata") {
                input = u
                    .get("promptTokenCount")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(input);
                output = u
                    .get("candidatesTokenCount")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(output);
                thoughts = u
                    .get("thoughtsTokenCount")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(thoughts);
            }
        }
        Ok(RunStats {
            ttft: ttft.context("stream ended without any content")?,
            total: t0.elapsed(),
            // parity with devin: hidden thinking tokens count as generated
            output_tokens: output + thoughts,
            input_tokens: input,
            cache_read_tokens: 0,
            stop,
            server_ttft: None,
            text_chars,
        })
    }
}
