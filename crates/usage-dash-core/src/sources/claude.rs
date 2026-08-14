//! Claude Code usage source — reads PER-SESSION data directly, no statusline:
//!   • context tokens + model  ← the session transcript
//!       ~/.claude/projects/<sanitized-cwd>/<sid>.jsonl
//!       (last assistant message `usage`: input + cache_read + cache_creation)
//!   • 5h / weekly limit %      ← Anthropic usage API (see anthropic_usage.rs)
//!   • daily 12% tracker        ← weekly% delta since midnight (budget.rs)
//!
//! Everything shown for a pane comes from THAT pane's own session. The model,
//! the reasoning effort and the CLI version are stamped per turn in the
//! transcript, so they are read from its tail; the focused transcript is
//! resolved from the session reference herdr reports, never from "newest file in
//! this cwd", which is a sibling pane's conversation as often as it is this
//! one's.
//!
//! The 1M context window is the one fact no transcript records. Session evidence
//! still comes first — a session that has already exceeded 200k is on the 1M
//! window whatever any setting says — and only then does the pane's OWN account
//! root (`CLAUDE_CONFIG_DIR`) `settings.json` decide. Same rule for a model or
//! effort the transcript never stamped: the root's configured default is a last
//! resort, not the source of truth.

use crate::budget;
use crate::model::{Cli, UsageSnapshot};
use crate::sources::anthropic_usage;
use crate::util;
use std::path::{Path, PathBuf};

const WINDOW_200K: u64 = 200_000;
const WINDOW_1M: u64 = 1_000_000;

fn projects_root() -> PathBuf {
    util::claude_config_root().join("projects")
}

/// Locate the transcript `<sid>.jsonl` under any project dir of the Claude Code
/// projects root; newest mtime wins.
pub fn transcript_for_sid(sid: &str) -> Option<PathBuf> {
    let want = format!("{sid}.jsonl");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    if let Ok(projects) = std::fs::read_dir(projects_root()) {
        for proj in projects.flatten() {
            let cand = proj.path().join(&want);
            if let Ok(meta) = std::fs::metadata(&cand) {
                let mt = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                if best.as_ref().map(|(b, _)| mt > *b).unwrap_or(true) {
                    best = Some((mt, cand));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

#[derive(Debug, Default, Clone)]
pub struct Transcript {
    pub model: Option<String>,
    pub ctx_tokens: Option<u64>, // current context = last assistant prompt size
    pub max_ctx_tokens: u64,     // largest seen this session (for window inference)
    // fork(wave-2): true when the LAST assistant message carried a `thinking`
    // content block (extended thinking on). This is only an OBSERVATION that the
    // model thought on that turn, not the selected level — see `effort`.
    pub thinking: bool,
    /// fork: the SELECTED thinking/reasoning level ("none"/"low"/"medium"/
    /// "high"/…), read from the top-level `effort` field Claude Code stamps on
    /// every assistant record. Unlike `thinking` this is the *setting*, so it
    /// stays correct on a turn that happened not to emit a thinking block.
    /// Last real assistant record wins, so changing the level mid-session is
    /// reflected on the next turn. `None` on transcripts that predate the field.
    pub effort: Option<String>,
    /// fork: the Claude Code version, from the top-level `version` field on the
    /// same record (e.g. "2.1.220").
    pub version: Option<String>,
}

/// fork: the model-display suffix for a thread's reasoning/thinking mode.
///
/// Prefers the SELECTED level (`effort`) over the observed `thinking` flag,
/// because the level is what the user actually chose and what they asked to
/// see. The flag remains the fallback for transcripts written before Claude
/// Code stamped `effort`, so old sessions keep the previous behaviour instead
/// of silently losing their mode. Empty when there is nothing to say, so
/// callers can append it unconditionally.
///
/// `default_effort` is the pane's own root `settings.json` `effortLevel`, used
/// only when the session says nothing at all — a transcript that predates the
/// `effort` stamp and never emitted a thinking block. Session evidence, explicit
/// or observed, always wins over a setting.
fn thinking_suffix(thinking: bool, effort: Option<&str>, default_effort: Option<&str>) -> String {
    let session_level = effort.map(str::trim).filter(|e| !e.is_empty());
    match session_level {
        Some(level) => format!(" · {}", abbrev_effort(level)),
        // `think` rather than `thinking` — same meaning, 3 columns cheaper, and
        // this is the fallback for pre-`effort` transcripts only.
        None if thinking => " · think".to_string(),
        None => match default_effort.map(str::trim).filter(|e| !e.is_empty()) {
            Some(level) => format!(" · {}", abbrev_effort(level)),
            None => String::new(),
        },
    }
}

/// fork: shorten the reasoning level for the strip. The caption sets the box
/// width, so every column here widens the dash for all panes. Only `medium` is
/// actually long; the rest already fit and stay verbatim so the value remains
/// readable rather than encoded.
fn abbrev_effort(level: &str) -> String {
    let lower = level.to_ascii_lowercase();
    match lower.as_str() {
        "medium" => "med".to_string(),
        _ => lower,
    }
}

/// fork: the `<version>` prefix carried INSIDE the model string.
///
/// It belongs on its own snapshot field, and was written that way first — but
/// the running host deserializes the collector's JSON into its own compiled-in
/// `UsageSnapshot` and silently drops fields it predates, so a new field cannot
/// reach the screen without a host rebuild and a restart. The model string is
/// passed through untouched, so the data rides there instead. Unwind this in
/// favour of real fields at the next host promote.
///
/// Empty when the transcript carries no version (pre-`version` records), so
/// callers can prepend it unconditionally.
fn version_prefix(version: Option<&str>) -> String {
    match version.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => format!("{v} · "),
        None => String::new(),
    }
}

/// Scan a transcript for the LAST assistant message's model + context size.
/// Context size = input_tokens + cache_read_input_tokens + cache_creation_input_tokens.
pub fn parse_transcript(content: &str) -> Transcript {
    let mut out = Transcript::default();
    for line in content.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // fork(wave-2): a `/compact` writes a `compact_boundary` system entry
        // immediately, carrying the post-compaction context size in
        // `compactMetadata.postTokens` (both manual and auto triggers). Because
        // lines are processed in order, setting `ctx_tokens` here and letting a
        // LATER assistant `usage` overwrite it yields the right precedence: while
        // no assistant turn has happened since compaction the boundary's
        // postTokens is current; the next turn's usage naturally takes over.
        // postTokens is small and never feeds `max_ctx_tokens` (window inference).
        if v.get("type").and_then(|t| t.as_str()) == Some("system")
            && v.get("subtype").and_then(|t| t.as_str()) == Some("compact_boundary")
        {
            if let Some(post) = v
                .pointer("/compactMetadata/postTokens")
                .and_then(|x| x.as_u64())
            {
                out.ctx_tokens = Some(post);
            }
            continue;
        }
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let msg = match v.get("message") {
            Some(m) => m,
            None => continue,
        };
        let usage = match msg.get("usage") {
            Some(u) => u,
            None => continue,
        };
        let get = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        let ctx = get("input_tokens")
            + get("cache_read_input_tokens")
            + get("cache_creation_input_tokens");
        if ctx > 0 {
            out.ctx_tokens = Some(ctx);
            out.max_ctx_tokens = out.max_ctx_tokens.max(ctx);
        }
        // fork(wave-2): Claude Code writes synthetic assistant entries (model
        // "<synthetic>", zero usage) while initializing or as error placeholders.
        // Only a REAL model id updates the model + thinking flag, so a subagent
        // whose tail is still just a synthetic entry yields no model (callers fall
        // back to the meta.json alias) rather than rendering "<synthetic>". The
        // `ctx`/`max` update above is unaffected — synthetic entries have zero
        // usage and never overwrite a real context size.
        if let Some(m) = msg
            .get("model")
            .and_then(|x| x.as_str())
            .filter(|m| is_real_model(m))
        {
            out.model = Some(m.to_string());
            // fork: `effort` and `version` live on the ENVELOPE (`v`), not on
            // `message`. They are read here, under the same last-real-assistant
            // gate as the model, so a synthetic or subagent-tail record can
            // never move them out of step with the model they belong to.
            out.effort = v
                .get("effort")
                .and_then(|x| x.as_str())
                .filter(|e| !e.trim().is_empty())
                .map(|e| e.to_string());
            out.version = v
                .get("version")
                .and_then(|x| x.as_str())
                .filter(|e| !e.trim().is_empty())
                .map(|e| e.to_string());
            // last real assistant message wins — a plain tool-use continuation
            // with no thinking block clears the flag, matching the visible state.
            out.thinking = msg
                .get("content")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("thinking"))
                })
                .unwrap_or(false);
        }
    }
    out
}

/// fork(wave-2): a real model id — anything not empty and not a Claude Code
/// synthetic placeholder (`<synthetic>` and any other `<…>`-wrapped sentinel).
fn is_real_model(m: &str) -> bool {
    !(m.is_empty() || m.starts_with('<') && m.ends_with('>'))
}

/// Friendly model name from the raw API id. Unknown ids pass through.
pub fn model_display(id: &str) -> String {
    match id {
        "claude-opus-4-8" => "Opus 4.8".into(),
        "claude-opus-4-7" => "Opus 4.7".into(),
        "claude-sonnet-4-6" => "Sonnet 4.6".into(),
        "claude-haiku-4-5" | "claude-haiku-4-5-20251001" => "Haiku 4.5".into(),
        // fork(wave-2): bare subagent spawn aliases (`.meta.json` `model` field)
        // used as a display fallback when the subagent tail has no real-model
        // assistant entry yet. `opus[1m]` and other variants stay passthrough.
        "opus" => "Opus 4.8".into(),
        "sonnet" => "Sonnet 4.6".into(),
        "haiku" => "Haiku 4.5".into(),
        // fork: the 5-family. Without these the ids fall through raw as
        // `claude-opus-5`, which is 7 characters of vendor prefix on the widest
        // line in the box — and the caption is what sets the box width.
        "claude-opus-5" => "Opus 5".into(),
        "claude-sonnet-5" => "Sonnet 5".into(),
        "claude-fable-5" => "Fable 5".into(),
        // fork: generic fallback for ids we don't know yet — drop the `claude-`
        // vendor prefix rather than render it. Every model in this box is
        // already labelled by CLI, so the prefix carries no information and
        // costs 7 columns. Unknown non-claude ids stay verbatim.
        other => other.strip_prefix("claude-").unwrap_or(other).to_string(),
    }
}

/// The CONFIGURED defaults from the focused pane's own account root, used only
/// where the session itself gives no answer.
///
/// These are settings, not facts about a session: `settings.json` says what a
/// NEW session would start as, while a running pane can be on any model and any
/// effort level. So every field here is a last resort behind the transcript, and
/// the root is the pane's own (`CLAUDE_CONFIG_DIR`), never a global one.
#[derive(Debug, Default, Clone, PartialEq)]
struct RootDefaults {
    /// `.model` with any `[1m]` marker stripped, e.g. `claude-fable-5`.
    model: Option<String>,
    /// `.effortLevel`, e.g. `low`.
    effort: Option<String>,
    /// `.model` carried a `[1m]` marker and 1M is not disabled by env.
    one_m: bool,
}

fn root_defaults() -> RootDefaults {
    let mut out = RootDefaults::default();
    let raw = match std::fs::read_to_string(util::claude_config_root().join("settings.json")) {
        Ok(raw) => raw,
        Err(_) => return out,
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return out;
    };
    if let Some(model) = v.get("model").and_then(|x| x.as_str()) {
        let disabled = std::env::var("CLAUDE_CODE_DISABLE_1M_CONTEXT")
            .ok()
            .as_deref()
            == Some("1");
        out.one_m = !disabled && model.contains("1m");
        let bare = model.split('[').next().unwrap_or(model).trim();
        if !bare.is_empty() {
            out.model = Some(bare.to_string());
        }
    }
    out.effort = v
        .get("effortLevel")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string);
    out
}

/// Pick the context window for a model.
///
/// Session evidence first: a session that has ALREADY exceeded 200k is on the 1M
/// window, whatever any setting says. Absent that, the pane's own root setting
/// decides — and only for opus, which is the family the 1M beta applies to.
fn window_for(model_id: &str, observed_max: u64, defaults: &RootDefaults) -> u64 {
    if observed_max > WINDOW_200K {
        return WINDOW_1M;
    }
    if defaults.one_m && one_m_applies_to(model_id, defaults.model.as_deref()) {
        return WINDOW_1M;
    }
    WINDOW_200K
}

/// Does the root's `[1m]` marker describe THIS session's model?
///
/// The marker is attached to one configured model (`claude-fable-5[1m]`), so it
/// says nothing about a pane running something else. Matching is containment in
/// either direction because the two spellings differ by design: settings hold a
/// user-facing alias (`opus`), transcripts hold the API id (`claude-opus-4-8`).
///
/// This used to be a bare `model_id.contains("opus")`, from when 1M was an
/// opus-only beta — which both missed a 1M-enabled non-opus model and claimed
/// 1M for any opus pane on an account whose 1M model was something else.
fn one_m_applies_to(session_model: &str, configured: Option<&str>) -> bool {
    let session = session_model.to_ascii_lowercase();
    match configured {
        Some(configured) => {
            let configured = configured.to_ascii_lowercase();
            !configured.is_empty()
                && (session.contains(&configured) || configured.contains(&session))
        }
        // Settings that name no model at all: fall back to the historical
        // opus-only assumption rather than granting 1M to everything.
        None => session.contains("opus"),
    }
}

pub fn snapshot_for_sid(sid: &str) -> Option<UsageSnapshot> {
    snapshot_for_sid_with_subagent(sid, None)
}

/// fork(wave-1): like [`snapshot_for_sid`], but when `subagent` is `Some(name)`
/// the focused Claude pane is VIEWING that subagent — override the snapshot's
/// model + context with the subagent's own (resolved from the sibling
/// `<sid>/subagents/` transcripts), while keeping the account's plan-quota rows
/// (5h / weekly / today) and per-session state/stats from the MAIN session. A
/// missing/unmatched subagent falls back silently to the main-session snapshot.
pub fn snapshot_for_sid_with_subagent(sid: &str, subagent: Option<&str>) -> Option<UsageSnapshot> {
    let path = transcript_for_sid(sid)?;
    snapshot_from_transcript(&path, subagent)
}

/// fork(wave-2): the newest transcript for `cwd`'s Claude project dir under
/// `~/.claude/projects`, by mtime. Claude Code names a project dir by mapping
/// each non-alphanumeric char of the cwd to `-` (no run collapsing, no
/// wrapping), e.g. `/home/andres` -> `-home-andres` — distinct from pi's
/// `--…--` scheme, so it is computed here.
fn latest_transcript_for_cwd(cwd: &str) -> Option<PathBuf> {
    let slug: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    if let Ok(entries) = std::fs::read_dir(projects_root().join(&slug)) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let mt = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().map(|(b, _)| mt > *b).unwrap_or(true) {
                best = Some((mt, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// fork(wave-2): resolve the focused Claude pane's transcript, then build its
/// snapshot. The reported session id is authoritative when its `<sid>.jsonl`
/// exists (located by [`transcript_for_sid`]); on a sid MISS — herdr can report
/// a session id that never got a transcript on disk — fall back to the newest
/// transcript for the focused `cwd`, so the pane shows the session it is really
/// in rather than sticking on a stale snapshot.
/// Resolve the FOCUSED pane's own transcript.
///
/// `refs` are the session references herdr reported for that pane, most
/// authoritative first (`session_id`, then the native `session_ref` — for Claude
/// both are the session uuid, but herdr fills them from different places and
/// either can be the one that is populated).
///
/// The cwd fallback is deliberately reachable ONLY when the pane has no session
/// reference at all. Several panes routinely share one cwd, so "newest
/// transcript in this project dir" is another pane's conversation as often as it
/// is this one's — using it to paper over a reference that simply has no file
/// yet is what made every pane in a repo report the same model and effort.
/// With a reference in hand and no transcript for it, the honest answer is none:
/// a brand-new session has nothing to show for a tick or two, and the panel is
/// sticky, so it keeps the previous frame rather than borrowing a sibling's.
pub fn snapshot_for_focus(
    refs: &[Option<&str>],
    cwd: Option<&str>,
    subagent: Option<&str>,
) -> Option<UsageSnapshot> {
    let mut named = false;
    for candidate in refs.iter().flatten().filter(|r| !r.is_empty()) {
        named = true;
        if let Some(path) = transcript_for_sid(candidate) {
            return snapshot_from_transcript(&path, subagent);
        }
    }
    if named {
        return None;
    }
    let path = cwd.and_then(latest_transcript_for_cwd)?;
    snapshot_from_transcript(&path, subagent)
}

/// Build a Claude snapshot from an already-resolved transcript path.
fn snapshot_from_transcript(path: &Path, subagent: Option<&str>) -> Option<UsageSnapshot> {
    let content = std::fs::read_to_string(path).ok()?;
    let t = parse_transcript(&content);
    let model_id = t.model.clone().unwrap_or_default();

    let mut s = UsageSnapshot::empty(Cli::Claude);
    let defaults = root_defaults();
    // The session's own model, or — only when this transcript has produced no
    // real assistant record yet — the model a new session in this root starts
    // as. Never another pane's session.
    let model_id = if model_id.is_empty() {
        defaults.model.clone().unwrap_or_default()
    } else {
        model_id
    };
    let window = window_for(&model_id, t.max_ctx_tokens, &defaults);
    s.model = Some(model_id.as_str())
        .filter(|id| !id.is_empty())
        .map(|id| {
            let name = model_display(id);
            // fork: `1M` not `(1M context)` — 9 columns saved on the line that
            // sets the box width, with the meaning intact next to a model name.
            let base = if window == WINDOW_1M {
                format!("{name} 1M")
            } else {
                name
            };
            // fork: version · model · mode (e.g. `2.1.220 · Opus 5 1M · high`).
            format!(
                "{}{base}{}",
                version_prefix(t.version.as_deref()),
                thinking_suffix(t.thinking, t.effort.as_deref(), defaults.effort.as_deref())
            )
        });
    // Version freshness for the caption's clickable tokens. `t.version` is what
    // this pane is RUNNING, which is the number the user can act on — not
    // whatever happens to be installed on disk.
    s.versions = crate::sources::cliver::versions_for(t.version.as_deref(), false);
    s.ctx_used_tokens = t.ctx_tokens;
    s.ctx_window = Some(window);
    s.fill_ctx_pct();
    s.source_age_secs = util::file_age_secs(path);

    // main-thread state, inferred from the same transcript (no extra read).
    let (st, note) = crate::state::infer_claude(&content, s.source_age_secs);
    s.state = st;
    s.state_note = note;
    // Full untruncated last message for the click-to-expand view (only when the
    // turn is finished/awaiting — not mid-work, where the last message is stale).
    s.state_full = match st {
        crate::model::ThreadState::Waiting
        | crate::model::ThreadState::AwaitingReview
        | crate::model::ThreadState::Done
        | crate::model::ThreadState::Degraded => crate::state::full_claude(&content),
        _ => None,
    };

    // Per-session STATS (incremental — folds only bytes appended since last tick).
    s.stats = crate::stats::session_stats(&path.to_string_lossy(), path);

    // 5h / weekly limits straight from Anthropic's usage API (account-global).
    if let Some((h5, wk)) = anthropic_usage::windows() {
        s.five_hour = h5;
        s.weekly = wk;
        if let Some(wkp) = s.weekly.used_pct {
            s.daily_used_pct = Some(budget::daily_delta("claude", &util::today_local(), wkp));
        }
    }

    // fork(wave-1): quota rows above stay the account's; only model + context are
    // swapped to the viewed subagent's.
    if let Some(name) = subagent {
        apply_subagent_override(&mut s, path, name);
    }
    Some(s)
}

/// fork(wave-1): resolved facts about the subagent a pane is viewing: its real
/// model id + context from the LAST assistant record of its own transcript, plus
/// the `.meta.json` `model` alias as a display fallback when the transcript has
/// no assistant record yet.
struct SubagentResolved {
    model_id: String,
    ctx_tokens: Option<u64>,
    max_ctx_tokens: u64,
    meta_alias: String,
    // fork(wave-2): the subagent's own thinking state (its transcript tail).
    thinking: bool,
    /// fork: the subagent's own SELECTED reasoning level (its transcript tail).
    effort: Option<String>,
}

/// fork(wave-1): find the subagent named `name` next to the main transcript
/// (`<proj>/<sid>/subagents/*.meta.json` whose `name` field == `name`, newest
/// mtime wins — filenames like `agent-aprobe1-<hex>` are NOT reliable), then read
/// its sibling `.jsonl` tail for the real model + context. Returns `None` when
/// the dir or a matching agent is absent.
fn resolve_subagent(main_transcript: &Path, name: &str) -> Option<SubagentResolved> {
    // `<proj>/<sid>.jsonl` -> `<proj>/<sid>/subagents`.
    let subdir = main_transcript.with_extension("").join("subagents");
    let mut best: Option<(std::time::SystemTime, PathBuf, String)> = None;
    for entry in std::fs::read_dir(&subdir).ok()?.flatten() {
        let meta_path = entry.path();
        let is_meta = meta_path
            .to_str()
            .map(|s| s.ends_with(".meta.json"))
            .unwrap_or(false);
        if !is_meta {
            continue;
        }
        let raw = match std::fs::read_to_string(&meta_path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("name").and_then(|x| x.as_str()) != Some(name) {
            continue;
        }
        let alias = v
            .get("model")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        let mt = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map(|(b, _, _)| mt > *b).unwrap_or(true) {
            best = Some((mt, meta_path, alias));
        }
    }
    let (_, meta_path, meta_alias) = best?;

    // Sibling transcript: `agent-<slug>-<hex>.meta.json` -> `.jsonl`.
    let meta_str = meta_path.to_string_lossy();
    let jsonl = PathBuf::from(format!(
        "{}.jsonl",
        meta_str.strip_suffix(".meta.json").unwrap_or(&meta_str)
    ));
    // fork: `effort` comes from the SUBAGENT's own transcript, not the main
    // thread's — a subagent is routinely spawned at a different level than the
    // orchestrator, and showing the parent's level while an @agent is focused
    // would misreport the very thing being displayed.
    let (model_id, ctx_tokens, max_ctx_tokens, thinking, effort) =
        match std::fs::read_to_string(&jsonl) {
            Ok(content) => {
                let t = parse_transcript(&content);
                (
                    t.model.unwrap_or_default(),
                    t.ctx_tokens,
                    t.max_ctx_tokens,
                    t.thinking,
                    t.effort,
                )
            }
            Err(_) => (String::new(), None, 0, false, None),
        };
    Some(SubagentResolved {
        model_id,
        ctx_tokens,
        max_ctx_tokens,
        meta_alias,
        thinking,
        effort,
    })
}

/// fork(wave-1): override a main-session snapshot's model + context in place with
/// the viewed subagent's, appending ` · @<name>` to the model display. Quota rows,
/// state, and stats are left untouched (same plan account).
fn apply_subagent_override(s: &mut UsageSnapshot, main_transcript: &Path, name: &str) {
    let Some(r) = resolve_subagent(main_transcript, name) else {
        return; // no matching subagent files -> silent fallback to main session
    };
    // Prefer the transcript's real model id; fall back to the meta.json alias.
    let display_id = if r.model_id.is_empty() {
        r.meta_alias.clone()
    } else {
        r.model_id.clone()
    };
    // fork(wave-2): no real model AND no meta alias (a subagent whose tail is only
    // a synthetic placeholder and whose meta omits `model`) -> keep the
    // main-session model rather than rendering an empty/placeholder display.
    if display_id.is_empty() {
        return;
    }

    // The subagent runs under the same account root as its parent, so the same
    // defaults apply where its own tail is silent.
    let defaults = root_defaults();
    let window = window_for(&display_id, r.max_ctx_tokens, &defaults);
    s.ctx_window = Some(window);
    s.ctx_approx = false;
    let name_disp = model_display(&display_id);
    let with_win = if window == WINDOW_1M {
        format!("{name_disp} 1M")
    } else {
        name_disp
    };
    // fork(wave-2): model · mode · @subagent.
    s.model = Some(format!(
        "{with_win}{} · @{name}",
        thinking_suffix(r.thinking, r.effort.as_deref(), defaults.effort.as_deref())
    ));
    if r.ctx_tokens.is_some() {
        s.ctx_used_tokens = r.ctx_tokens;
    }
    s.fill_ctx_pct();
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSCRIPT: &str = r#"{"type":"user","message":{"role":"user"}}
{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":60,"cache_read_input_tokens":38634,"cache_creation_input_tokens":85989,"output_tokens":349}}}
{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":2,"cache_read_input_tokens":149701,"cache_creation_input_tokens":4352,"output_tokens":136}}}"#;

    // fork: real-shaped records carrying the top-level `effort` + `version`
    // fields Claude Code stamps on the ENVELOPE (not inside `message`). The
    // level changes mid-transcript — the last real assistant record must win,
    // so switching thinking mode is reflected rather than latched.
    const TRANSCRIPT_EFFORT: &str = r#"{"type":"user","message":{"role":"user"}}
{"type":"assistant","version":"2.1.220","effort":"high","message":{"model":"claude-opus-5","usage":{"input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":0,"output_tokens":5}}}
{"type":"assistant","version":"2.1.220","effort":"low","message":{"model":"claude-opus-5","usage":{"input_tokens":12,"cache_read_input_tokens":120,"cache_creation_input_tokens":0,"output_tokens":6}}}"#;

    #[test]
    fn parses_last_effort_and_version_from_envelope() {
        let t = parse_transcript(TRANSCRIPT_EFFORT);
        assert_eq!(t.effort.as_deref(), Some("low")); // last record wins
        assert_eq!(t.version.as_deref(), Some("2.1.220"));
    }

    #[test]
    fn transcript_without_effort_leaves_none() {
        // Pre-`effort` transcripts must not fabricate a level.
        let t = parse_transcript(TRANSCRIPT);
        assert!(t.effort.is_none());
        assert!(t.version.is_none());
    }

    #[test]
    fn selected_effort_wins_over_observed_thinking_flag() {
        // The whole point of preferring `effort`: a turn that emitted no
        // thinking block still reports the SELECTED level, instead of
        // silently reading as "thinking off".
        assert_eq!(thinking_suffix(false, Some("high"), None), " · high");
        // Long levels are abbreviated so the caption stays narrow; see abbrev_effort.
        assert_eq!(thinking_suffix(true, Some("Medium"), None), " · med");
        // "none" is a real selected level and must be shown as such, never
        // collapsed into "no suffix" — off and unknown must stay distinct.
        assert_eq!(thinking_suffix(false, Some("none"), None), " · none");
    }

    #[test]
    fn thinking_flag_is_the_fallback_for_old_transcripts() {
        assert_eq!(thinking_suffix(true, None, None), " · think");
        assert_eq!(thinking_suffix(false, None, None), "");
        // Whitespace-only is treated as absent, not as a level.
        assert_eq!(thinking_suffix(true, Some("  "), None), " · think");
    }

    #[test]
    fn the_root_default_effort_is_the_last_resort_only() {
        // A session that stamped its own level ignores the root setting even
        // when they disagree — the pane is running what the transcript says.
        assert_eq!(thinking_suffix(false, Some("high"), Some("low")), " · high");
        // An observed thinking block is still session evidence, so it also
        // outranks the setting.
        assert_eq!(thinking_suffix(true, None, Some("low")), " · think");
        // Only with nothing at all from the session does the root answer.
        assert_eq!(thinking_suffix(false, None, Some("low")), " · low");
        assert_eq!(thinking_suffix(false, None, Some("Medium")), " · med");
        // A blank setting is not an answer.
        assert_eq!(thinking_suffix(false, None, Some("  ")), "");
    }

    #[test]
    fn caption_abbreviations_keep_the_box_narrow() {
        // The caption sets the usage box width, so these are width fixes, not
        // cosmetics. Each assertion is a column count the dash no longer pays.
        assert_eq!(model_display("claude-opus-5"), "Opus 5");
        // Unknown 5-family / future ids lose the vendor prefix, not their identity.
        assert_eq!(model_display("claude-neptune-9"), "neptune-9");
        // Non-claude ids are untouched.
        assert_eq!(model_display("gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(abbrev_effort("medium"), "med");
        assert_eq!(abbrev_effort("HIGH"), "high");
    }

    #[test]
    fn version_prefix_renders_running_version() {
        assert_eq!(version_prefix(Some("2.1.220")), "2.1.220 · ");
        // Pre-`version` transcripts get no separator to lead the caption with.
        assert_eq!(version_prefix(None), "");
        assert_eq!(version_prefix(Some("  ")), "");
    }

    #[test]
    fn parses_last_assistant_context() {
        let t = parse_transcript(TRANSCRIPT);
        assert_eq!(t.model.as_deref(), Some("claude-opus-4-8"));
        // last message: 2 + 149701 + 4352
        assert_eq!(t.ctx_tokens, Some(154055));
        // max over session: first message 60+38634+85989 = 124683 < 154055
        assert_eq!(t.max_ctx_tokens, 154055);
    }

    // fork(wave-2): real-shaped assistant message carrying a thinking content
    // block (keys: type/thinking/signature) followed by text, then a plain
    // tool-use continuation with no thinking block.
    const THINKING_TRANSCRIPT: &str = r#"{"type":"user","message":{"role":"user"}}
{"type":"assistant","message":{"model":"claude-opus-4-8","content":[{"type":"thinking","thinking":"let me plan","signature":"EtwHCpMBCA8YAipA"},{"type":"text","text":"hi"}],"usage":{"input_tokens":10,"cache_read_input_tokens":90000,"cache_creation_input_tokens":0,"output_tokens":50}}}"#;

    #[test]
    fn parse_detects_thinking_block_on_last_assistant() {
        let t = parse_transcript(THINKING_TRANSCRIPT);
        assert!(t.thinking);
    }

    #[test]
    fn parse_thinking_false_when_last_assistant_has_no_thinking_block() {
        // last assistant is a plain tool-use continuation -> thinking clears.
        let transcript = r#"{"type":"assistant","message":{"model":"claude-opus-4-8","content":[{"type":"thinking","thinking":"x","signature":"s"}],"usage":{"input_tokens":1,"cache_read_input_tokens":1,"cache_creation_input_tokens":0,"output_tokens":1}}}
{"type":"assistant","message":{"model":"claude-opus-4-8","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}],"usage":{"input_tokens":2,"cache_read_input_tokens":2,"cache_creation_input_tokens":0,"output_tokens":1}}}"#;
        let t = parse_transcript(transcript);
        assert!(!t.thinking);
    }

    #[test]
    fn thinking_suffix_appended_to_model_display() {
        let h = TempHome::new("thinking-main");
        let sid = "sid-think";
        h.write(
            &format!(".claude/projects/-tmp-proj/{sid}.jsonl"),
            THINKING_TRANSCRIPT,
        );
        h.write(".claude/dash2/anthropic_usage.json", ANTHROPIC_CACHE);
        let s = snapshot_for_sid(sid).expect("snapshot");
        assert_eq!(s.model.as_deref(), Some("Opus 4.8 · think"));
    }

    #[test]
    fn subagent_thinking_orders_model_mode_then_marker() {
        let h = TempHome::new("sub-think");
        let sid = "sid-sub-think";
        let meta = r#"{"agentType":"probe1","name":"probe1","model":"sonnet"}"#;
        // subagent's own tail has a thinking block.
        let sub = r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6","content":[{"type":"thinking","thinking":"y","signature":"s"},{"type":"text","text":"ok"}],"usage":{"input_tokens":5,"cache_read_input_tokens":12000,"cache_creation_input_tokens":0,"output_tokens":20}}}"#;
        write_subagent_session(&h, sid, meta, sub);
        let s = snapshot_for_sid_with_subagent(sid, Some("probe1")).expect("snapshot");
        // ordering: model · mode · @subagent
        assert_eq!(s.model.as_deref(), Some("Sonnet 4.6 · think · @probe1"));
    }

    // fork(wave-2): real-shaped compact_boundary system entry (manual /compact).
    const COMPACT_BOUNDARY: &str = r#"{"type":"system","subtype":"compact_boundary","content":"Conversation compacted","compactMetadata":{"trigger":"manual","preTokens":59698,"postTokens":4982,"cumulativeDroppedTokens":54716},"uuid":"81618df8","timestamp":"2026-07-15T23:48:27.257Z"}"#;

    #[test]
    fn compact_boundary_after_last_assistant_uses_post_tokens() {
        // assistant turn, then a /compact with no assistant turn since -> ctx = postTokens.
        let transcript = format!(
            "{}\n{}",
            r#"{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":10,"cache_read_input_tokens":59000,"cache_creation_input_tokens":688,"output_tokens":50}}}"#,
            COMPACT_BOUNDARY
        );
        let t = parse_transcript(&transcript);
        assert_eq!(t.ctx_tokens, Some(4982)); // postTokens, not the pre-compact 59698
                                              // window inference still sees the pre-compact peak.
        assert_eq!(t.max_ctx_tokens, 59698);
    }

    #[test]
    fn assistant_turn_after_compact_boundary_takes_over() {
        // a turn AFTER the boundary -> its usage is current, boundary is stale.
        let transcript = format!(
            "{}\n{}",
            COMPACT_BOUNDARY,
            r#"{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":5,"cache_read_input_tokens":7000,"cache_creation_input_tokens":0,"output_tokens":20}}}"#
        );
        let t = parse_transcript(&transcript);
        assert_eq!(t.ctx_tokens, Some(7005)); // the post-boundary turn's context
    }

    #[test]
    fn no_compact_boundary_leaves_ctx_from_last_assistant() {
        let t = parse_transcript(TRANSCRIPT);
        assert_eq!(t.ctx_tokens, Some(154055)); // unchanged behavior
    }

    #[test]
    fn model_names_map() {
        assert_eq!(model_display("claude-opus-4-8"), "Opus 4.8");
        assert_eq!(model_display("claude-sonnet-4-6"), "Sonnet 4.6");
        assert_eq!(model_display("future-model"), "future-model");
    }

    #[test]
    fn window_self_corrects_above_200k() {
        // sonnet has no 1M, but observed usage > 200k forces the 1M window
        let off = RootDefaults::default();
        assert_eq!(window_for("claude-sonnet-4-6", 50_000, &off), WINDOW_200K);
        assert_eq!(window_for("claude-sonnet-4-6", 250_000, &off), WINDOW_1M);
    }

    #[test]
    fn the_1m_window_prefers_session_evidence_over_the_root_setting() {
        // Settings that name no model keep the historical opus-only rule.
        let bare = RootDefaults {
            one_m: true,
            ..RootDefaults::default()
        };
        let off = RootDefaults::default();
        assert_eq!(window_for("claude-opus-5", 10_000, &bare), WINDOW_1M);
        assert_eq!(window_for("claude-sonnet-5", 10_000, &bare), WINDOW_200K);
        // Root says nothing, but this session has ALREADY passed 200k, so it
        // demonstrably has the bigger window whatever the setting claims.
        assert_eq!(window_for("claude-sonnet-5", 250_000, &off), WINDOW_1M);
    }

    #[test]
    fn the_1m_marker_applies_only_to_the_model_it_was_configured_for() {
        let fable_1m = RootDefaults {
            model: Some("claude-fable-5".into()),
            one_m: true,
            ..RootDefaults::default()
        };
        // The configured model gets the window it was configured with, even
        // though it is not opus.
        assert_eq!(window_for("claude-fable-5", 10_000, &fable_1m), WINDOW_1M);
        // A pane on a different model does not inherit that account setting.
        assert_eq!(window_for("claude-opus-5", 10_000, &fable_1m), WINDOW_200K);

        // Settings hold a user-facing alias, transcripts hold the API id; the
        // two must still be recognised as the same model.
        let opus_alias = RootDefaults {
            model: Some("opus".into()),
            one_m: true,
            ..RootDefaults::default()
        };
        assert_eq!(
            window_for("claude-opus-4-8", 10_000, &opus_alias),
            WINDOW_1M
        );
        assert_eq!(
            window_for("claude-sonnet-5", 10_000, &opus_alias),
            WINDOW_200K
        );
    }

    #[test]
    fn empty_transcript_yields_nothing() {
        let t = parse_transcript("garbage\n{not json}");
        assert!(t.model.is_none());
        assert!(t.ctx_tokens.is_none());
    }

    // ---- transcript-tree integration fixtures ----

    // Each test gets its own temp HOME. `cargo nextest` isolates tests in separate
    // processes, but plain `cargo test` runs them as THREADS of one process, so
    // HOME is shared state and concurrent `TempHome`s clobber each other — which
    // is why this file used to fail a dozen tests under `cargo test` while
    // passing under nextest and under `--test-threads=1`. The lock serializes the
    // env-mutating tests, and the previous value is restored on drop so a later
    // test never inherits a HOME pointing at a deleted directory. Same pattern as
    // `goal-dash-core/src/store.rs`.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct TempHome {
        dir: PathBuf,
        prev_home: Option<std::ffi::OsString>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl TempHome {
        fn new(tag: &str) -> Self {
            // A test that panics while holding the lock poisons it; the data is
            // just `()`, so recover rather than cascading a second failure.
            let guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let uniq = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!("herdr-usage-{tag}-{uniq}"));
            std::fs::create_dir_all(&dir).unwrap();
            let prev_home = std::env::var_os("HOME");
            std::env::set_var("HOME", &dir);
            TempHome {
                dir,
                prev_home,
                _guard: guard,
            }
        }

        fn write(&self, rel: &str, content: &str) -> PathBuf {
            let p = self.dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
            p
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match self.prev_home.take() {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A Claude-model transcript carrying the envelope fields the caption reads
    /// (`version`, `effort`) plus a 1M context window. Distinct from the older
    /// `CLAUDE_TRANSCRIPT` fixture below, which predates those fields. The
    /// observed context deliberately exceeds 200k so `window_for` resolves the
    /// 1M window from evidence rather than from the `one_m_enabled()` env
    /// toggle, which would make the expected caption environment-dependent.
    const CLAUDE_V_TRANSCRIPT: &str = r#"{"type":"user","message":{"role":"user"}}
{"type":"assistant","version":"2.1.220","effort":"high","message":{"model":"claude-opus-5","usage":{"input_tokens":250000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":300}}}"#;

    /// The caption a pane's snapshot renders end to end: the RUNNING Claude Code
    /// version, the model (with its window), then the selected reasoning level.
    #[test]
    fn claude_pane_caption_is_version_model_mode() {
        let h = TempHome::new("caption");
        let sid = "sid-caption";
        h.write(
            &format!(".claude/projects/-tmp-proj/{sid}.jsonl"),
            CLAUDE_V_TRANSCRIPT,
        );
        // Fresh anthropic cache so the Claude path never hits the network.
        h.write(".claude/dash2/anthropic_usage.json", ANTHROPIC_CACHE);

        let s = snapshot_for_sid(sid).expect("snapshot");
        assert_eq!(s.cli, Cli::Claude);
        assert_eq!(s.model.as_deref(), Some("2.1.220 · Opus 5 1M · high"));
    }

    // ---- fork(wave-1): subagent-view override fixtures ----

    const ANTHROPIC_CACHE: &str = r#"{"five_hour":{"utilization":10.0,"resets_at":"2026-07-15T12:00:00+00:00"},"seven_day":{"utilization":20.0,"resets_at":"2026-07-20T00:00:00+00:00"}}"#;

    const MAIN_TRANSCRIPT: &str = r#"{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":10,"cache_read_input_tokens":90000,"cache_creation_input_tokens":0,"output_tokens":50}}}"#;

    // A subagent whose jsonl reports a different model + smaller context than main.
    const SUB_TRANSCRIPT: &str = r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":5,"cache_read_input_tokens":12000,"cache_creation_input_tokens":0,"output_tokens":20}}}"#;

    // filename slug ('aprobe1') deliberately differs from the meta `name` ('probe1').
    fn write_subagent_session(h: &TempHome, sid: &str, meta: &str, jsonl: &str) {
        let proj = PROJ_SLUG;
        h.write(
            &format!(".claude/projects/{proj}/{sid}.jsonl"),
            MAIN_TRANSCRIPT,
        );
        h.write(".claude/dash2/anthropic_usage.json", ANTHROPIC_CACHE);
        h.write(
            &format!(
                ".claude/projects/{proj}/{sid}/subagents/agent-aprobe1-a5e2b92b5f55e765.meta.json"
            ),
            meta,
        );
        h.write(
            &format!(
                ".claude/projects/{proj}/{sid}/subagents/agent-aprobe1-a5e2b92b5f55e765.jsonl"
            ),
            jsonl,
        );
    }

    #[test]
    fn subagent_resolves_by_meta_name_not_filename_and_overrides_ctx_and_model() {
        let h = TempHome::new("sub-name");
        let sid = "80430a4e-a993-4076-96cf-2e625a3efdde";
        let meta = r#"{"agentType":"probe1","name":"probe1","model":"opus[1m]","teamName":"session-80430a4e"}"#;
        write_subagent_session(&h, sid, meta, SUB_TRANSCRIPT);

        // Match by meta `name` ("probe1"), though the filename slug is "aprobe1".
        let s = snapshot_for_sid_with_subagent(sid, Some("probe1")).expect("snapshot");
        assert_eq!(s.cli, Cli::Claude); // stays the account's plan
                                        // ctx swapped to the subagent's tail: 5 + 12000 + 0 = 12005 (main was 90010).
        assert_eq!(s.ctx_used_tokens, Some(12005));
        // model swapped to the subagent's real model id, with the @name marker.
        assert_eq!(s.model.as_deref(), Some("Sonnet 4.6 · @probe1"));
        // account plan-quota rows still come from the MAIN session path.
        assert_eq!(s.weekly.used_pct, Some(20.0));
        assert_eq!(s.five_hour.used_pct, Some(10.0));
    }

    #[test]
    fn subagent_falls_back_to_meta_alias_when_jsonl_has_no_assistant_record() {
        let h = TempHome::new("sub-alias");
        let sid = "sid-alias";
        let meta = r#"{"agentType":"probe1","name":"probe1","model":"opus[1m]"}"#;
        // jsonl present but no assistant turn yet.
        write_subagent_session(
            &h,
            sid,
            meta,
            r#"{"type":"user","message":{"role":"user"}}"#,
        );
        // 1M enabled on the account so the opus alias infers the 1M window.
        h.write(".claude/settings.json", r#"{"model":"opus[1m]"}"#);

        let s = snapshot_for_sid_with_subagent(sid, Some("probe1")).expect("snapshot");
        // alias "opus[1m]" -> model_display passthrough + @name; window inferred 1M (contains "opus").
        assert_eq!(s.model.as_deref(), Some("opus[1m] 1M · @probe1"));
        assert_eq!(s.ctx_window, Some(WINDOW_1M));
    }

    #[test]
    fn missing_subagent_falls_back_to_main_session() {
        let h = TempHome::new("sub-missing");
        let sid = "sid-missing";
        let meta = r#"{"name":"probe1","model":"opus[1m]"}"#;
        write_subagent_session(&h, sid, meta, SUB_TRANSCRIPT);

        // Viewing a name that has no meta.json -> silent fallback to main.
        let s = snapshot_for_sid_with_subagent(sid, Some("ghost")).expect("snapshot");
        assert_eq!(s.ctx_used_tokens, Some(90010)); // main transcript tail
        assert_eq!(s.model.as_deref(), Some("Opus 4.8")); // main model, no @marker
    }

    #[test]
    fn no_subagent_arg_is_plain_main_session() {
        let h = TempHome::new("sub-none");
        let sid = "sid-none";
        let meta = r#"{"name":"probe1","model":"opus[1m]"}"#;
        write_subagent_session(&h, sid, meta, SUB_TRANSCRIPT);

        let s = snapshot_for_sid_with_subagent(sid, None).expect("snapshot");
        assert_eq!(s.ctx_used_tokens, Some(90010));
        assert_eq!(s.model.as_deref(), Some("Opus 4.8"));
    }

    #[test]
    fn newest_meta_wins_on_duplicate_names() {
        let h = TempHome::new("sub-dup");
        let sid = "sid-dup";
        let proj = PROJ_SLUG;
        h.write(
            &format!(".claude/projects/{proj}/{sid}.jsonl"),
            MAIN_TRANSCRIPT,
        );
        h.write(".claude/dash2/anthropic_usage.json", ANTHROPIC_CACHE);
        // Two agents both named "probe1"; the newer one's jsonl must win.
        let older = h.write(
            &format!(".claude/projects/{proj}/{sid}/subagents/agent-old-1111.meta.json"),
            r#"{"name":"probe1","model":"opus[1m]"}"#,
        );
        h.write(
            &format!(".claude/projects/{proj}/{sid}/subagents/agent-old-1111.jsonl"),
            r#"{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":1,"cache_read_input_tokens":1,"cache_creation_input_tokens":0,"output_tokens":1}}}"#,
        );
        h.write(
            &format!(".claude/projects/{proj}/{sid}/subagents/agent-new-2222.meta.json"),
            r#"{"name":"probe1","model":"sonnet"}"#,
        );
        h.write(
            &format!(".claude/projects/{proj}/{sid}/subagents/agent-new-2222.jsonl"),
            SUB_TRANSCRIPT,
        );
        // Make the "new" meta newest, the older one older (mtime is the tiebreak).
        let now = std::time::SystemTime::now();
        set_mtime(&older, now - std::time::Duration::from_secs(100));
        set_mtime(
            &h.dir.join(format!(
                ".claude/projects/{proj}/{sid}/subagents/agent-new-2222.meta.json"
            )),
            now,
        );

        let s = snapshot_for_sid_with_subagent(sid, Some("probe1")).expect("snapshot");
        assert_eq!(s.model.as_deref(), Some("Sonnet 4.6 · @probe1")); // newer agent
        assert_eq!(s.ctx_used_tokens, Some(12005));
    }

    fn set_mtime(path: &Path, t: std::time::SystemTime) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }

    // ---- fork(wave-2): synthetic-entry skip in the subagent tail ----

    // Real Claude Code shape: a synthetic assistant placeholder (model
    // "<synthetic>", zero usage) — written while initializing / as an error stub.
    const SYNTHETIC_ENTRY: &str = r#"{"type":"assistant","message":{"model":"<synthetic>","role":"assistant","content":[{"type":"text","text":"(no content)"}],"usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;

    #[test]
    fn parse_skips_synthetic_model_keeping_last_real() {
        // real haiku turn, then a synthetic placeholder -> model stays the real id.
        let transcript = format!(
            "{}\n{}",
            r#"{"type":"assistant","message":{"model":"claude-haiku-4-5","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":5,"cache_read_input_tokens":8000,"cache_creation_input_tokens":0,"output_tokens":20}}}"#,
            SYNTHETIC_ENTRY
        );
        let t = parse_transcript(&transcript);
        assert_eq!(t.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(t.ctx_tokens, Some(8005)); // synthetic zero-usage never overwrote
    }

    #[test]
    fn parse_only_synthetic_yields_no_model() {
        let t = parse_transcript(SYNTHETIC_ENTRY);
        assert!(t.model.is_none());
    }

    #[test]
    fn subagent_tail_real_after_synthetic_uses_real_model() {
        let h = TempHome::new("sub-synth-real");
        let sid = "sid-synth-real";
        let meta = r#"{"agentType":"general-purpose","name":"probe-view","model":"haiku"}"#;
        let sub = format!(
            "{}\n{}",
            r#"{"type":"assistant","message":{"model":"claude-haiku-4-5","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":5,"cache_read_input_tokens":8000,"cache_creation_input_tokens":0,"output_tokens":20}}}"#,
            SYNTHETIC_ENTRY
        );
        write_subagent_session(&h, sid, meta, &sub);
        let s = snapshot_for_sid_with_subagent(sid, Some("probe-view")).expect("snapshot");
        assert_eq!(s.model.as_deref(), Some("Haiku 4.5 · @probe-view"));
    }

    #[test]
    fn subagent_tail_only_synthetic_falls_back_to_meta_alias() {
        let h = TempHome::new("sub-synth-alias");
        let sid = "sid-synth-alias";
        // meta alias "sonnet"; tail is ONLY a synthetic entry (no real model yet).
        let meta = r#"{"agentType":"general-purpose","name":"probe-view","model":"sonnet"}"#;
        write_subagent_session(&h, sid, meta, SYNTHETIC_ENTRY);
        let s = snapshot_for_sid_with_subagent(sid, Some("probe-view")).expect("snapshot");
        // never "<synthetic>": prettified alias fallback.
        assert_eq!(s.model.as_deref(), Some("Sonnet 4.6 · @probe-view"));
    }

    #[test]
    fn subagent_only_synthetic_and_no_alias_keeps_main_model() {
        let h = TempHome::new("sub-synth-none");
        let sid = "sid-synth-none";
        // no `model` in meta AND tail only synthetic -> keep the main-session model.
        let meta = r#"{"agentType":"general-purpose","name":"probe-view"}"#;
        write_subagent_session(&h, sid, meta, SYNTHETIC_ENTRY);
        let s = snapshot_for_sid_with_subagent(sid, Some("probe-view")).expect("snapshot");
        assert_eq!(s.model.as_deref(), Some("Opus 4.8")); // main model, unchanged
    }

    // ---- fork(wave-2): sid-miss cwd fallback across both roots ----

    const CLAUDE_TRANSCRIPT: &str = r#"{"type":"user","message":{"role":"user"}}
{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":10,"cache_read_input_tokens":40000,"cache_creation_input_tokens":0,"output_tokens":50}}}"#;

    // A fixed, machine-independent cwd and the Claude project slug it encodes
    // to (each non-alphanumeric char mapped to `-`, per
    // `latest_transcript_for_cwd`). The two MUST stay in sync: the cwd fallback
    // looks the transcript up by this slug. Nothing is read from the real home
    // directory — `TempHome` redirects the roots — so the path need not exist.
    const PROJ_CWD: &str = "/tmp/herdrproj";
    const PROJ_SLUG: &str = "-tmp-herdrproj";

    /// An OLDER transcript for the same cwd, with a model the assertions can
    /// tell apart from `CLAUDE_TRANSCRIPT`'s, so "newest wins" is really tested.
    const OLDER_CWD_TRANSCRIPT: &str = r#"{"type":"assistant","message":{"model":"claude-fable-5","usage":{"input_tokens":10,"cache_read_input_tokens":1000,"cache_creation_input_tokens":0,"output_tokens":50}}}"#;

    #[test]
    fn a_named_session_never_borrows_a_siblings_transcript() {
        let h = TempHome::new("fallback-cwd");
        let older = h.write(
            &format!(".claude/projects/{PROJ_SLUG}/aaaaaaaa-older.jsonl"),
            OLDER_CWD_TRANSCRIPT,
        );
        let newer = h.write(
            &format!(".claude/projects/{PROJ_SLUG}/bbbbbbbb-newer.jsonl"),
            CLAUDE_TRANSCRIPT,
        );
        h.write(".claude/dash2/anthropic_usage.json", ANTHROPIC_CACHE);
        let now = std::time::SystemTime::now();
        set_mtime(&older, now - std::time::Duration::from_secs(600));
        set_mtime(&newer, now);

        // A pane that HAS a session reference whose transcript does not exist
        // yet reports nothing. Falling back to "newest in this cwd" here is what
        // made every pane in one repo show the same model: several panes share a
        // cwd, so the newest transcript is a sibling pane as often as it is this
        // one. The panel is sticky, so nothing shown beats something wrong.
        assert!(
            snapshot_for_focus(&[Some("00000000-missing")], Some(PROJ_CWD), None).is_none(),
            "a named-but-missing session must not resolve to another pane"
        );

        // With NO reference at all, the cwd fallback is still the best guess
        // available and stays: newest wins.
        let s = snapshot_for_focus(&[None, Some("")], Some(PROJ_CWD), None)
            .expect("unnamed focus still falls back to the cwd");
        assert_eq!(s.cli, Cli::Claude);
        assert_eq!(s.model.as_deref(), Some("Opus 4.8"));
        let _ = &older;
        let _ = &newer;
    }

    #[test]
    fn exact_sid_match_beats_cwd_fallback() {
        let h = TempHome::new("sid-priority");
        let sid = "12345678-real";
        // The real sid transcript …
        h.write(
            &format!(".claude/projects/{PROJ_SLUG}/{sid}.jsonl"),
            CLAUDE_TRANSCRIPT,
        );
        // … while a NEWER transcript for the same cwd exists but must be ignored
        // because the sid resolves.
        let newer = h.write(
            &format!(".claude/projects/{PROJ_SLUG}/bbbbbbbb-newer.jsonl"),
            OLDER_CWD_TRANSCRIPT,
        );
        h.write(".claude/dash2/anthropic_usage.json", ANTHROPIC_CACHE);
        set_mtime(&newer, std::time::SystemTime::now());

        let s = snapshot_for_focus(&[Some(sid)], Some(PROJ_CWD), None).expect("snapshot");
        assert_eq!(s.cli, Cli::Claude); // exact sid wins over newer cwd candidate
        assert_eq!(s.model.as_deref(), Some("Opus 4.8"));
    }

    #[test]
    fn the_root_default_model_fills_in_only_when_the_session_has_none() {
        let h = TempHome::new("root-default-model");
        let sid = "12345678-synthetic";
        // A transcript whose only assistant record is a synthetic placeholder:
        // a real session, but no model evidence in it yet.
        h.write(
            &format!(".claude/projects/{PROJ_SLUG}/{sid}.jsonl"),
            r#"{"type":"assistant","message":{"model":"<synthetic>","usage":{"input_tokens":1,"cache_read_input_tokens":1,"cache_creation_input_tokens":0,"output_tokens":1}}}"#,
        );
        h.write(".claude/dash2/anthropic_usage.json", ANTHROPIC_CACHE);
        h.write(
            ".claude/settings.json",
            r#"{"model":"claude-fable-5[1m]","effortLevel":"high"}"#,
        );

        let s = snapshot_for_focus(&[Some(sid)], Some(PROJ_CWD), None).expect("snapshot");
        // Model and effort come from the ROOT settings, since the session has
        // neither, and the `[1m]` marker is stripped from the display NAME while
        // still selecting the window — the marker was configured for exactly the
        // model this session fell back to.
        assert_eq!(s.model.as_deref(), Some("Fable 5 1M · high"));
        assert_eq!(s.ctx_window, Some(WINDOW_1M));
    }

    #[test]
    fn the_native_session_ref_resolves_when_herdr_reports_no_session_id() {
        let h = TempHome::new("session-ref");
        let sid = "12345678-native";
        h.write(
            &format!(".claude/projects/{PROJ_SLUG}/{sid}.jsonl"),
            CLAUDE_TRANSCRIPT,
        );
        // A NEWER sibling in the same cwd that must not be picked.
        let newer = h.write(
            &format!(".claude/projects/{PROJ_SLUG}/bbbbbbbb-newer.jsonl"),
            OLDER_CWD_TRANSCRIPT,
        );
        h.write(".claude/dash2/anthropic_usage.json", ANTHROPIC_CACHE);
        set_mtime(&newer, std::time::SystemTime::now());

        // session_id empty, session_ref populated — the shape herdr reports for
        // a pane it adopted rather than spawned.
        let s = snapshot_for_focus(&[None, Some(sid)], Some(PROJ_CWD), None)
            .expect("session_ref must resolve");
        assert_eq!(s.model.as_deref(), Some("Opus 4.8"));
    }
}
