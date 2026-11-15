//! Grok Build: `~/.grok/sessions/<cwd>/<id>/updates.jsonl`.
//!
//! `session/update` records with `sessionUpdate: "turn_completed"` carry
//! per-turn usage (`inputTokens` includes cache; `cachedReadTokens` is the
//! discounted subset; `reasoningTokens` is already inside `outputTokens`).
//! The sibling `summary.json` supplies the generated title and a fallback
//! model id. Files are append-only JSONL — `filecache` resumes mid-file.

use anyhow::Result;
use indicatif::MultiProgress;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::report::Usage;
use crate::sources::SourceOut;
use crate::sources::filecache::{self, CachedCall, Dict, Entry, Jsonl};

#[derive(Deserialize)]
struct Line {
    timestamp: Option<serde_json::Value>,
    params: Option<Params>,
}

#[derive(Deserialize)]
struct Params {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    update: Option<Update>,
}

#[derive(Deserialize)]
struct Update {
    #[serde(rename = "sessionUpdate")]
    kind: Option<String>,
    content: Option<Content>,
    usage: Option<TurnUsage>,
    prompt_id: Option<String>,
    #[serde(rename = "_meta")]
    meta: Option<Meta>,
}

#[derive(Deserialize)]
struct Content {
    text: Option<String>,
}

#[derive(Deserialize)]
struct Meta {
    #[serde(rename = "modelId")]
    model_id: Option<String>,
}

#[derive(Deserialize, Default)]
struct TurnUsage {
    #[serde(default, rename = "inputTokens")]
    input: u64,
    #[serde(default, rename = "outputTokens")]
    output: u64,
    #[serde(default, rename = "cachedReadTokens")]
    cached: u64,
    #[serde(default, rename = "modelUsage")]
    model_usage: HashMap<String, ModelUsage>,
}

#[derive(Deserialize, Default)]
struct ModelUsage {
    #[serde(default, rename = "inputTokens")]
    input: u64,
    #[serde(default, rename = "outputTokens")]
    output: u64,
    #[serde(default, rename = "cachedReadTokens")]
    cached: u64,
}

#[derive(Deserialize)]
struct Summary {
    generated_title: Option<String>,
    session_summary: Option<String>,
    current_model_id: Option<String>,
    info: Option<SummaryInfo>,
}

#[derive(Deserialize)]
struct SummaryInfo {
    id: Option<String>,
}

/// Parser state carried across mid-file resumes.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct State {
    sid: String,
    model: String,
    name: Option<String>,
}

pub struct Grok;

pub fn default_dir() -> PathBuf {
    let home = std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::home_dir()
                .unwrap_or_else(|| PathBuf::from("~"))
                .join(".grok")
        });
    home.join("sessions")
}

fn ts_of(v: Option<&serde_json::Value>) -> Option<i64> {
    match v {
        Some(serde_json::Value::Number(n)) => n.as_i64().or_else(|| n.as_u64().map(|u| u as i64)),
        Some(serde_json::Value::String(s)) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.timestamp()),
        _ => None,
    }
}

fn usage_of(u: &TurnUsage) -> Usage {
    Usage {
        input: u.input.saturating_sub(u.cached),
        cached: u.cached.min(u.input),
        output: u.output,
    }
}

/// Parse `path` from `offset`, extending `dict`. Returns
/// (consumed bytes, final state, dict, new tail entries).
fn parse_file(
    path: &Path,
    offset: u64,
    mut state: State,
    dict: Vec<String>,
) -> std::io::Result<(u64, State, Vec<String>, Vec<CachedCall>)> {
    let file_sid = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if state.sid.is_empty() {
        state.sid = file_sid.clone();
    }
    let mut dict = Dict::from_vec(dict);
    let mut entries = Vec::new();
    let consumed = filecache::read_lines(
        path,
        offset,
        |s| serde_json::from_str::<Line>(s).is_ok(),
        |line| {
            if !line.contains("turn_completed")
                && !line.contains("user_message_chunk")
                && !line.contains("modelId")
            {
                return;
            }
            let Ok(l) = serde_json::from_str::<Line>(line) else {
                return;
            };
            let Some(p) = l.params else { return };
            if let Some(id) = p.session_id {
                state.sid = id;
            }
            let Some(u) = p.update else { return };
            match u.kind.as_deref() {
                Some("user_message_chunk") => {
                    if state.name.is_none()
                        && let Some(t) = u.content.as_ref().and_then(|c| c.text.as_deref())
                    {
                        let t = super::titleize(t);
                        if !t.is_empty() && !t.starts_with('<') {
                            state.name = Some(t);
                        }
                    }
                    if let Some(m) = u.meta.as_ref().and_then(|m| m.model_id.as_deref())
                        && state.model.is_empty()
                    {
                        state.model = m.to_string();
                    }
                }
                Some("turn_completed") => {
                    let Some(usage) = u.usage else { return };
                    let ts = ts_of(l.timestamp.as_ref());
                    let prompt = u.prompt_id.as_deref().unwrap_or("");
                    let models: Vec<(String, Usage)> = if usage.model_usage.is_empty() {
                        vec![(state.model.clone(), usage_of(&usage))]
                    } else {
                        usage
                            .model_usage
                            .iter()
                            .map(|(m, mu)| {
                                (
                                    m.clone(),
                                    Usage {
                                        input: mu.input.saturating_sub(mu.cached),
                                        cached: mu.cached.min(mu.input),
                                        output: mu.output,
                                    },
                                )
                            })
                            .collect()
                    };
                    for (model, us) in models {
                        if us.total() == 0 {
                            continue;
                        }
                        if !model.is_empty() {
                            state.model = model.clone();
                        }
                        let model = if model.is_empty() {
                            "grok".to_string()
                        } else {
                            model
                        };
                        let key = filecache::key_of(&[
                            state.sid.as_bytes(),
                            prompt.as_bytes(),
                            model.as_bytes(),
                        ]);
                        entries.push(CachedCall {
                            key,
                            session: dict.intern(&state.sid),
                            session_name: None,
                            model: dict.intern(&model),
                            ts,
                            usage: us,
                            estimated: false,
                        });
                    }
                }
                _ => {
                    if let Some(m) = u.meta.as_ref().and_then(|m| m.model_id.as_deref())
                        && state.model.is_empty()
                    {
                        state.model = m.to_string();
                    }
                }
            }
        },
    )?;
    Ok((consumed, state, dict.into_strings(), entries))
}

impl Jsonl for Grok {
    type State = State;
    const SOURCE: &'static str = "grok";

    fn fresh(path: &Path) -> State {
        let mut state = State {
            sid: path
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            ..State::default()
        };
        let summary_path = path.with_file_name("summary.json");
        if let Ok(text) = std::fs::read_to_string(&summary_path)
            && let Ok(s) = serde_json::from_str::<Summary>(&text)
        {
            if let Some(id) = s.info.as_ref().and_then(|i| i.id.as_deref()) {
                state.sid = id.to_string();
            }
            state.name = s
                .generated_title
                .or(s.session_summary)
                .map(|t| super::titleize(&t))
                .filter(|t| !t.is_empty());
            state.model = s.current_model_id.unwrap_or_default();
        }
        state
    }

    fn parse(
        path: &Path,
        offset: u64,
        state: State,
        dict: Vec<String>,
    ) -> std::io::Result<(u64, State, Vec<String>, Vec<CachedCall>)> {
        parse_file(path, offset, state, dict)
    }

    fn fixup(e: &mut Entry<State>) {
        let Some(name) = e.state.name.as_ref() else {
            return;
        };
        let i = filecache::dict_get_or_push(&mut e.dict, name);
        for c in &mut e.entries {
            c.session_name = Some(i);
        }
    }
}

pub type Scanner = filecache::Scanner<Grok>;

/// `updates.jsonl` under each sessions tree.
pub fn walk(dirs: &[PathBuf]) -> Vec<(PathBuf, u64)> {
    let mut files = Vec::new();
    for dir in dirs {
        files.extend(
            walkdir::WalkDir::new(dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name() == "updates.jsonl")
                .filter_map(|e| e.metadata().ok().map(|m| (e.into_path(), m.len()))),
        );
    }
    files
}

pub fn load(dirs: &[PathBuf], mp: &MultiProgress) -> Result<SourceOut> {
    let t0 = std::time::Instant::now();
    let mut sc = Scanner::open(dirs.to_vec());
    let found = walk(dirs);
    let t = sc.tick(&found, mp);
    sc.save();
    tracing::debug!(
        files = t.files,
        parsed = t.parsed,
        bytes = t.bytes,
        calls = t.calls.len(),
        dupes = t.dupes,
        elapsed = ?t0.elapsed(),
        "grok source"
    );
    let mut note = format!("grok: {} session logs · {} reparsed", t.files, t.parsed);
    if t.dupes > 0 {
        note.push_str(&format!(" · {} dupes skipped", t.dupes));
    }
    Ok(SourceOut {
        calls: t.calls,
        note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_session(dir: &Path, sid: &str, summary: &str, jsonl: &str) -> PathBuf {
        let sess = dir.join("cwd").join(sid);
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("summary.json"), summary).unwrap();
        let p = sess.join("updates.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(jsonl.as_bytes()).unwrap();
        p
    }

    #[test]
    fn turn_completed_splits_cache_and_keeps_cjk_title() {
        let tmp = std::env::temp_dir().join(format!("llmstat-grok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let jsonl = concat!(
            r#"{"timestamp":1794726432,"method":"session/update","params":{"sessionId":"01a1de07-96f2-7a20-a761-7c980c90e702","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"用我们预制的工具扫描 マージ済み worktree 병합된 세션"},"_meta":{"modelId":"grok-4.6"}}}}"#,
            "\n",
            r#"{"timestamp":1794726496,"method":"session/update","params":{"sessionId":"01a1de07-96f2-7a20-a761-7c980c90e702","update":{"sessionUpdate":"turn_completed","prompt_id":"7d29f830-e1ae-431c-8cb2-feae7d8a9e2d","usage":{"inputTokens":188363,"outputTokens":3647,"cachedReadTokens":92032,"reasoningTokens":2839,"modelUsage":{"grok-4.6-build":{"inputTokens":188363,"outputTokens":3647,"cachedReadTokens":92032,"reasoningTokens":2839}}}}}}"#,
            "\n",
        );
        let path = write_session(
            &tmp,
            "01a1de07-96f2-7a20-a761-7c980c90e702",
            r#"{"generated_title":"llmstat Grok Antigravity Support Status","current_model_id":"grok-4.6","info":{"id":"01a1de07-96f2-7a20-a761-7c980c90e702"}}"#,
            jsonl,
        );
        let state = Grok::fresh(&path);
        assert_eq!(
            state.name.as_deref(),
            Some("llmstat Grok Antigravity Support Status")
        );
        let (consumed, state, dict, entries) = parse_file(&path, 0, state, Vec::new()).unwrap();
        assert!(consumed > 0);
        assert_eq!(entries.len(), 1);
        let c = &entries[0];
        assert_eq!(c.usage.input, 188363 - 92032);
        assert_eq!(c.usage.cached, 92032);
        assert_eq!(c.usage.output, 3647);
        assert_eq!(dict[c.model as usize], "grok-4.6-build");
        assert_eq!(c.ts, Some(1794726496));
        let mut e = Entry {
            offset: consumed,
            mtime: 0,
            boundary: Vec::new(),
            state,
            dict,
            entries,
        };
        Grok::fixup(&mut e);
        let name_i = e.entries[0].session_name.unwrap();
        assert_eq!(
            e.dict[name_i as usize],
            "llmstat Grok Antigravity Support Status"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
