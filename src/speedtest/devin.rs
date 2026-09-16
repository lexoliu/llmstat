//! Devin backend: Connect-RPC on server.codeium.com.
//!
//! Auth flow: `credentials.toml` `windsurf_api_key` → `GetUserJwt` → short-lived
//! `userJwt` carried in every request's `metadata`. Inference is
//! `ApiServerService/GetChatMessage`, a server-streaming Connect call with
//! framed protobuf payloads (5-byte envelope: flag + big-endian length).

use std::io::Read;
use std::time::Instant;

use anyhow::Context;
use serde::Deserialize;

use super::proto::{Reader, Value, Writer};
use super::{ModelEntry, Provider, RunStats};

const BASE: &str = "https://server.codeium.com";
const GET_USER_JWT: &str = "/exa.auth_pb.AuthService/GetUserJwt";
const MODEL_CONFIGS: &str = "/exa.api_server_pb.ApiServerService/GetCliModelConfigs";
const CHAT: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";

fn credentials_path() -> std::path::PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| "~".into())
        .join(".local/share/devin/credentials.toml")
}

#[derive(Deserialize)]
struct Credentials {
    windsurf_api_key: String,
}

pub struct Devin {
    api_key: String,
    user_jwt: Option<String>,
}

impl Devin {
    pub fn new() -> anyhow::Result<Self> {
        let path = credentials_path();
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let creds: Credentials = toml::from_str(&text).context("parsing credentials.toml")?;
        Ok(Self {
            api_key: creds.windsurf_api_key,
            user_jwt: None,
        })
    }

    /// `Metadata` — only the fields the CLI sets are populated.
    fn metadata(&self) -> Writer {
        let mut m = Writer::new();
        m.string(1, "windsurf"); // ide_name
        m.string(7, "3.2.23"); // ide_version
        m.string(12, "windsurf"); // extension_name
        m.string(2, "1.48.2"); // extension_version
        m.string(3, &self.api_key); // api_key
        m.string(4, "en"); // locale
        if let Some(j) = &self.user_jwt {
            m.string(21, j); // user_jwt
        }
        m
    }

    fn unary(path: &str, req: Writer) -> anyhow::Result<Vec<u8>> {
        let resp = ureq::post(format!("{BASE}{path}"))
            .content_type("application/proto")
            .header("connect-protocol-version", "1")
            .send(req.into_vec())
            .map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
        let mut body = resp.into_body();
        let bytes = body
            .read_to_vec()
            .with_context(|| format!("{path}: reading body"))?;
        if bytes.starts_with(&[0x1f, 0x8b]) {
            anyhow::bail!("{path}: unexpectedly gzipped response");
        }
        Ok(bytes)
    }

    fn ensure_jwt(&mut self) -> anyhow::Result<()> {
        if self.user_jwt.is_some() {
            return Ok(());
        }
        let mut req = Writer::new();
        req.message(1, &self.metadata());
        let bytes = Self::unary(GET_USER_JWT, req)?;
        for (field, value) in Reader::new(&bytes) {
            if field == 1
                && let Some(j) = value.as_str()
            {
                self.user_jwt = Some(j.to_string());
            }
        }
        if self.user_jwt.is_none() {
            anyhow::bail!(
                "GetUserJwt returned no userJwt: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        Ok(())
    }
}

impl Provider for Devin {
    fn catalog(&mut self) -> anyhow::Result<Vec<ModelEntry>> {
        self.ensure_jwt()?;
        let mut req = Writer::new();
        req.message(1, &self.metadata());
        let bytes = Self::unary(MODEL_CONFIGS, req)?;
        let mut out = Vec::new();
        for (field, value) in Reader::new(&bytes) {
            // client_model_configs: repeated ClientModelConfig
            if field != 1 {
                continue;
            }
            let Value::Bytes(cfg) = value else { continue };
            let mut label = String::new();
            let mut uid = String::new();
            let mut credit = None;
            for (f, v) in Reader::new(cfg) {
                match f {
                    1 => label = v.as_str().unwrap_or_default().to_string(),
                    22 => uid = v.as_str().unwrap_or_default().to_string(),
                    3 => credit = v.as_f64(),
                    _ => {}
                }
            }
            if !uid.is_empty() {
                out.push(ModelEntry {
                    uid,
                    label,
                    cost_hint: credit.map(|c| format!("{c}x credits")).unwrap_or_default(),
                });
            }
        }
        if out.is_empty() {
            anyhow::bail!("empty model catalog");
        }
        Ok(out)
    }

    fn stream(&self, uid: &str, prompt: &str, max_tokens: Option<u64>) -> anyhow::Result<RunStats> {
        self.user_jwt
            .as_ref()
            .context("devin provider used before catalog() authenticated it")?;
        let meta = self.metadata();

        let mut msg_prompt = Writer::new();
        msg_prompt.string(1, &uuid::Uuid::new_v4().to_string()); // message_id
        msg_prompt.varint_field(2, 1); // source = CHAT_MESSAGE_SOURCE_USER
        msg_prompt.string(3, prompt);

        let mut cfg = Writer::new();
        cfg.varint_field(1, 1); // num_completions
        cfg.varint_field(2, max_tokens.unwrap_or(4096)); // max_tokens
        cfg.varint_field(3, 200); // max_newlines
        cfg.double(5, 0.4); // temperature
        cfg.double(6, 0.4); // first_temperature
        cfg.varint_field(7, 50); // top_k
        cfg.double(8, 1.0); // top_p
        for s in [
            "<|user|>",
            "<|bot|>",
            "<|context_request|>",
            "<|endoftext|>",
            "<|end_of_turn|>",
        ] {
            cfg.string(9, s); // stop_patterns
        }
        cfg.double(11, 1.0); // fim_eot_prob_threshold

        let mut tool_choice = Writer::new();
        tool_choice.string(1, "auto"); // option_name

        let mut cache_opts = Writer::new();
        cache_opts.varint_field(1, 1); // type = EPHEMERAL

        let mut req = Writer::new();
        req.message(1, &meta);
        req.string(2, "You are a helpful assistant."); // system prompt
        req.message(3, &msg_prompt); // chat_message_prompts
        req.varint_field(7, 5); // request_type = CASCADE
        req.message(8, &cfg); // configuration
        req.bool_field(11, true); // disable_parallel_tool_calls
        req.message(12, &tool_choice);
        req.message(13, &cache_opts); // system_prompt_cache_options
        req.string(16, &uuid::Uuid::new_v4().to_string()); // cascade_id
        req.varint_field(20, 1); // planner_mode = DEFAULT
        req.string(21, uid); // chat_model_uid
        req.string(22, &uuid::Uuid::new_v4().to_string()); // execution_id

        // Connect envelope: flag(0=uncompressed data) + BE len + payload
        let body = req.into_vec();
        let mut frame = Vec::with_capacity(5 + body.len());
        frame.push(0u8);
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&body);

        let t0 = Instant::now();
        let resp = ureq::post(format!("{BASE}{CHAT}"))
            .content_type("application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("accept-encoding", "identity")
            .send(frame)
            .map_err(|e| anyhow::anyhow!("{CHAT}: {e}"))?;
        let mut reader = resp.into_body().into_reader();

        let mut ttft = None;
        let mut text_chars = 0usize;
        let mut stop = String::new();
        let (mut input, mut output, mut cache_read) = (0u64, 0u64, 0u64);
        let mut server_ttft = None;
        let mut trailer = String::new();

        loop {
            let mut flag = [0u8; 1];
            match reader.read_exact(&mut flag) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e).context("reading stream frame flag"),
            }
            let mut lenb = [0u8; 4];
            reader
                .read_exact(&mut lenb)
                .context("truncated stream frame length")?;
            let flag = flag[0];
            let len = u32::from_be_bytes(lenb) as usize;
            let mut payload = vec![0u8; len];
            reader
                .read_exact(&mut payload)
                .context("reading stream frame payload")?;
            if flag & 2 != 0 {
                trailer = String::from_utf8_lossy(&payload).to_string();
                continue;
            }
            if flag & 1 != 0 {
                anyhow::bail!("unexpected gzip-compressed frame");
            }
            for (field, value) in Reader::new(&payload) {
                match field {
                    3 | 9 => {
                        // delta_text / delta_thinking
                        if let Some(s) = value.as_str()
                            && !s.is_empty()
                        {
                            if ttft.is_none() {
                                ttft = Some(t0.elapsed());
                            }
                            if field == 3 {
                                text_chars += s.chars().count();
                            }
                        }
                    }
                    5 => {
                        if let Value::Varint(v) = value
                            && v != 0
                        {
                            stop = format!("stop={v}");
                        }
                    }
                    7 => {
                        if let Value::Bytes(b) = value {
                            for (f, v) in Reader::new(b) {
                                if let Value::Varint(n) = v {
                                    match f {
                                        2 => input = n,
                                        3 => output = n,
                                        5 => cache_read = n,
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    13 => {
                        // completion_profile: #4 time_to_first_token (double)
                        if let Value::Bytes(b) = value {
                            for (f, v) in Reader::new(b) {
                                if f == 4 {
                                    server_ttft = v.as_f64();
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if let Some(err) = serde_json::from_str::<serde_json::Value>(&trailer)
            .ok()
            .and_then(|t| t.get("error").cloned())
            .filter(|e| !e.is_null())
        {
            anyhow::bail!("stream error: {err}");
        }
        Ok(RunStats {
            ttft: ttft.context("stream ended without any content delta")?,
            total: t0.elapsed(),
            output_tokens: output,
            thinking_tokens: None,
            input_tokens: input,
            cache_read_tokens: cache_read,
            stop,
            server_ttft,
            text_chars,
        })
    }
}
