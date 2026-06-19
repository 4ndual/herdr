//! Goal inference + observer + sort, carved verbatim from goal-widget
//! `src/main.rs`. These shell out to `claude -p` (like usage-dash-core's
//! sources shell out) and persist through `GoalStore`. The `run_*` fns are the
//! callable leaves; herdr dispatches them as a subcommand. The goal-widget CLI
//! `spawn_*` re-exec wrappers are intentionally omitted — herdr owns dispatch.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::context::{claude_md_goal, recent_activity, user_prompts};
use crate::model::{
    clip_inline, has_real, norm, normalize_parent, now_stamp, GoalDoc, GoalState, Proposal,
    PromptEntry, Sub, MAX_INLINE, SUB_STATES,
};
use crate::store::{
    most_recent_session, move_goal_store, session_cwd, session_jsonl, GoalStore,
};
use crate::timegate::{
    candidate_projects, expand_tilde, fill_estimate, plan_inventory_block, project_goal_file,
    project_root,
};

fn claude_infer(prompt: &str, timeout: Duration) -> Option<String> {
    // `--strict-mcp-config` with no `--mcp-config` ⇒ no MCP servers loaded (inference needs no tools;
    // skipping the MCP load keeps the cold `claude -p` start fast and inside the timeout).
    let mut child = Command::new("claude")
        .arg("-p")
        .arg(prompt)
        .arg("--strict-mcp-config")
        .args(["--model", "haiku"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(_) => return None,
        }
    }
}

/// True when a model reply is a refusal / "I have no activity to work with" message rather than a
/// real goal. These leak in when inference runs against an empty transcript: the model answers
/// conversationally ("Your prompt appears incomplete…") and that text would otherwise be stored as
/// the goal. Signatures are specific phrases that essentially never appear in a one-line imperative goal.
fn looks_like_refusal(s: &str) -> bool {
    let l = s.to_lowercase();
    const SIGS: &[&str] = &[
        "appears incomplete",
        "got cut off",
        "cut off",
        "instruction template",
        "--- activity ---",
        "no recent activity",
        "no activity provided",
        "no activity data",
        "unable to determine",
        "provide activity",
        "provide the activity",
        "activity data to analyze",
        "once data is available",
        "once you provide",
        "haven't included",
        "haven't provided",
        "hasn't been provided",
        "please provide",
        "please share",
        "share the developer",
        "session activity",
        "goal breakdown once",
        "i'm unable",
        "i am unable",
        "cannot infer",
        "cannot determine",
        "no goal",
    ];
    SIGS.iter().any(|sig| l.contains(sig))
}

fn infer_goal(context: &str) -> Option<String> {
    if context.trim().is_empty() {
        return None;
    }
    let prompt = format!(
        "You are naming the MAIN, long-running NORTH-STAR goal of a developer's project — the \
         overarching mission the whole session serves, NOT the immediate task they happen to be \
         doing right now. Weigh the project's stated goal (CLAUDE.md) and the throughline across \
         ALL of the developer's prompts MORE than the latest activity. In ONE line (max 14 words, \
         imperative, no quotes, no preamble) state that north-star goal. Output ONLY that line.\n\n{context}"
    );
    let out = claude_infer(&prompt, Duration::from_secs(90))?;
    for ln in out.lines() {
        let ln = ln.trim().trim_start_matches(['-', '*', '•', ' ']).trim().trim_matches('"').trim();
        if ln.is_empty() { continue; }
        if looks_like_refusal(ln) { return None; }  // empty-activity refusal, not a goal
        return Some(ln.chars().take(MAX_INLINE).collect());
    }
    None
}

/// Infer once for `sid` (or the most-recent session) and persist under that session's state file.
pub fn run_infer(sid: Option<String>) -> i32 {
    let path = match &sid {
        Some(s) => session_jsonl(s),
        None => most_recent_session(),
    };
    // The session id we key the state by.
    let target_sid = sid.clone().or_else(|| {
        path.as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
    });
    let Some(target_sid) = target_sid else {
        eprintln!("infer: no session found");
        return 1;
    };
    // Rich north-star context: project CLAUDE.md ## Goal + the sum of the developer's prompts +
    // recent activity. The MAIN goal should track the project's mission, not the latest task.
    let mut context = String::new();
    let mut has_signal = false;
    if let Some(cwd) = session_cwd(&target_sid) {
        if let Some(root) = project_root(&cwd) {
            if let Some(g) = claude_md_goal(&root) {
                context.push_str(&format!("=== PROJECT CLAUDE.md \"## Goal\" (authoritative north-star) ===\n{g}\n\n"));
                has_signal = true;
            }
        }
    }
    if let Some(p) = &path {
        let prompts = user_prompts(p, 40);
        if !prompts.is_empty() {
            context.push_str(&format!("=== THE DEVELOPER'S PROMPTS (what they've asked for, oldest→newest) ===\n{prompts}\n\n"));
            has_signal = true;
        }
        let activity = recent_activity(p, 18);
        if !activity.trim().is_empty() {
            context.push_str(&format!("=== RECENT ACTIVITY ===\n{activity}\n"));
            has_signal = true;
        }
    }
    // No prompts, no activity, no project goal ⇒ nothing to summarize. Running the model here is what
    // produced the stored "Your prompt appears incomplete…" garbage goals — so we refuse to infer.
    if !has_signal {
        eprintln!("infer: no activity to infer from — leaving goal untouched");
        return 1;
    }
    let store = GoalStore::resolve(&target_sid);
    let mut doc = store.load();
    match infer_goal(&context) {
        Some(g) => {
            doc.main = GoalState { text: g.clone(), source: "agent".into(), approved: false, state: String::new(), updated_at: now_stamp(), set_at: now_stamp(), reason: None, ..Default::default() };
            fill_estimate(&mut doc.main, &store.root());
            let _ = store.save(&doc);
            store.append_history("main", &doc.main);
            println!("{g}");
            0
        }
        None => {
            // Honest placeholder; not logged to history (keeps the log free of noise).
            doc.main = GoalState { text: "(could not infer a goal — press e to set one)".into(), source: "agent".into(), approved: false, state: String::new(), updated_at: now_stamp(), set_at: now_stamp(), reason: None, ..Default::default() };
            let _ = store.save(&doc);
            eprintln!("infer: no goal inferred");
            1
        }
    }
}

// ---- structured tree inference (current goal + sub-goal list) ----

/// Ask the model for `{ "current": "...", "subgoals": ["...", ...] }` from recent activity.
fn infer_tree(activity: &str) -> Option<(String, Vec<String>)> {
    if activity.trim().is_empty() {
        return None;
    }
    let prompt = format!(
        "Below is recent activity from a developer's coding session. Identify the single goal they \
         are working on RIGHT NOW, and break it into a few concrete sub-tasks. Reply with STRICT \
         JSON only, no prose, no markdown fences, of the exact shape: \
         {{\"current\":\"<one short imperative line>\",\"subgoals\":[\"<short task>\",\"<short task>\"]}}. \
         Use 2-5 subgoals. Output ONLY the JSON object.\n\n--- activity ---\n{activity}"
    );
    let out = claude_infer(&prompt, Duration::from_secs(90))?;
    parse_infer_tree_json(&out)
}

/// Pure parser: pull `{ "current", "subgoals":[...] }` out of a possibly-wrapped model reply.
fn parse_infer_tree_json(out: &str) -> Option<(String, Vec<String>)> {
    let start = out.find('{')?;
    let end = out.rfind('}')?;
    if end < start { return None; }
    let json = &out[start..=end];
    #[derive(Deserialize)]
    struct Parsed { #[serde(default)] current: String, #[serde(default)] subgoals: Vec<String> }
    let p: Parsed = serde_json::from_str(json).ok()?;
    let current: String = p.current.trim().trim_matches('"').chars().take(MAX_INLINE).collect();
    let current = if looks_like_refusal(&current) { String::new() } else { current };
    let subs: Vec<String> = p.subgoals.into_iter()
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty() && !looks_like_refusal(s))
        .map(|s| s.chars().take(MAX_INLINE).collect())
        .collect();
    if current.is_empty() && subs.is_empty() { return None; }
    Some((current, subs))
}

/// Infer the current goal + subgoals for `sid` and MERGE into its tree (never clobber user work):
/// set `current` only if empty/agent-unapproved; add only agent subgoals whose text is new (dedup),
/// never touching user-added, approved, or discarded subs.
pub fn run_infer_tree(sid: &str) -> i32 {
    let activity = session_jsonl(sid).map(|p| recent_activity(&p, 18)).unwrap_or_default();
    let Some((current, subs)) = infer_tree(&activity) else {
        eprintln!("infer-tree: nothing inferred");
        return 1;
    };
    let store = GoalStore::resolve(sid);
    let mut doc = store.load();
    // current: only fill if there is no user/approved current goal yet.
    let mut changed_current = false;
    if !current.is_empty() && (!has_real(&doc.current.text) || (doc.current.source == "agent" && !doc.current.approved)) {
        doc.current = GoalState { text: current.clone(), source: "agent".into(), approved: false, state: String::new(), updated_at: now_stamp(), set_at: now_stamp(), reason: None, ..Default::default() };
        fill_estimate(&mut doc.current, &store.root());
        changed_current = true;
    }
    // subs: add new agent suggestions, deduped by normalized text against ALL existing subs.
    let mut seen: std::collections::HashSet<String> = doc.subs.iter().map(|s| norm(&s.text)).collect();
    let mut new_subs: Vec<Sub> = Vec::new();
    for s in subs {
        let n = norm(&s);
        if n.is_empty() || seen.contains(&n) { continue; }
        seen.insert(n);
        let id = doc.next_id;
        doc.next_id += 1;
        new_subs.push(Sub { id, text: s.clone(), source: "agent".into(), approved: false, state: "active".into(), parent: "current".into(), created_at: now_stamp(), updated_at: now_stamp(), pinned: false });
    }
    let added = new_subs.len();
    for sub in &new_subs { doc.subs.push(sub.clone()); }
    let _ = store.save(&doc);
    if changed_current { store.append_history("current", &doc.current); }
    for sub in &new_subs { store.append_history(&format!("sub-{}", sub.id), &sub.snapshot()); }
    println!("current set; +{added} subgoals");
    0
}

// ============================ Goal Observer (per-prompt, full-context, Opus) ============================

/// Run `claude -p --model opus` with the prompt fed on STDIN (prompts can be large), small JSON out.
fn claude_observe(prompt: String, timeout: Duration) -> Option<String> {
    // `--strict-mcp-config` with no `--mcp-config` ⇒ ZERO MCP servers. The observer needs no tools,
    // and loading the user's full MCP set on a cold `claude -p` adds enough startup latency to blow
    // past the timeout. Disabling MCP makes the cold start fast and deterministic.
    let mut child = Command::new("claude")
        .arg("-p")
        .arg("--strict-mcp-config")
        .args(["--model", "opus"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut si) = child.stdin.take() {
        std::thread::spawn(move || { use std::io::Write; let _ = si.write_all(prompt.as_bytes()); });
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() { return None; }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(None) => {
                if start.elapsed() > timeout { let _ = child.kill(); let _ = child.wait(); return None; }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(_) => return None,
        }
    }
}

/// Render every transcript line from index `since` onward (user prompts + Claude replies + tool
/// results, truncated), and return that rendering plus the new total non-empty-line count (the cursor).
pub fn transcript_delta(path: &Path, since: usize) -> (String, usize) {
    use serde_json::Value;
    let Ok(content) = fs::read_to_string(path) else { return (String::new(), since) };
    let mut idx = 0usize;
    let mut out: Vec<String> = Vec::new();
    for raw in content.lines() {
        let raw = raw.trim();
        if raw.is_empty() { continue; }
        let cur = idx; idx += 1;
        if cur < since { continue; }
        let Ok(e) = serde_json::from_str::<Value>(raw) else { continue };
        let role = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if role != "user" && role != "assistant" { continue; }
        let content = e.get("message").and_then(|m| m.get("content"));
        let mut text = String::new();
        match content {
            Some(Value::String(s)) => text = s.clone(),
            Some(Value::Array(blocks)) => {
                let mut parts: Vec<String> = Vec::new();
                for b in blocks {
                    match b.get("type").and_then(|t| t.as_str()) {
                        Some("text") => if let Some(t) = b.get("text").and_then(|t| t.as_str()) { parts.push(t.to_string()); },
                        Some("tool_use") => if let Some(n) = b.get("name").and_then(|t| t.as_str()) { parts.push(format!("[tool:{n}]")); },
                        Some("tool_result") => {
                            let s = match b.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                Some(Value::Array(a)) => a.iter().filter_map(|x| x.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join(" "),
                                _ => String::new(),
                            };
                            if !s.is_empty() { parts.push(format!("[result: {}]", s.chars().take(200).collect::<String>())); }
                        }
                        _ => {}
                    }
                }
                text = parts.join(" ");
            }
            _ => {}
        }
        let text = text.trim().replace('\n', " ");
        if !text.is_empty() {
            let snippet: String = text.chars().take(500).collect();
            out.push(format!("{role}: {snippet}"));
        }
    }
    (out.join("\n"), idx)
}

/// Count of non-empty transcript lines (the observer's cursor unit).
pub fn transcript_line_count(path: &Path) -> usize {
    fs::read_to_string(path).map(|c| c.lines().filter(|l| !l.trim().is_empty()).count()).unwrap_or(0)
}

/// True if a genuine USER PROMPT (text, not a tool-result echo) appears at/after line index `since`.
pub fn has_new_user_prompt(path: &Path, since: usize) -> bool {
    use serde_json::Value;
    let Ok(content) = fs::read_to_string(path) else { return false };
    let mut idx = 0usize;
    for raw in content.lines() {
        let raw = raw.trim();
        if raw.is_empty() { continue; }
        let cur = idx; idx += 1;
        if cur < since { continue; }
        let Ok(e) = serde_json::from_str::<Value>(raw) else { continue };
        if e.get("type").and_then(|v| v.as_str()) != Some("user") { continue; }
        match e.get("message").and_then(|m| m.get("content")) {
            Some(Value::String(s)) if !s.trim().is_empty() => return true,
            Some(Value::Array(blocks)) => {
                if blocks.iter().any(|b| b.get("type").and_then(|t| t.as_str()) == Some("text")
                    && b.get("text").and_then(|t| t.as_str()).is_some_and(|t| !t.trim().is_empty())) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

#[derive(Deserialize, Default)]
struct TextReason { #[serde(default)] text: String, #[serde(default)] reason: String }
#[derive(Deserialize, Default)]
struct CurrentOp { #[serde(default)] action: String, #[serde(default)] text: String, #[serde(default)] reason: String }
#[derive(Deserialize, Default)]
struct SubOp {
    #[serde(default)] op: String,
    #[serde(default)] id: Option<u64>,
    #[serde(default)] text: Option<String>,
    #[serde(default)] state: Option<String>,
    #[serde(default)] parent: Option<String>,
    #[serde(default)] reason: String,
}
#[derive(Deserialize, Default)]
struct ConstraintSuggestion { #[serde(default)] entity: String, #[serde(default)] text: String, #[serde(default)] reason: String }
#[derive(Deserialize, Default)]
struct ObserverDecision {
    #[serde(default)] paraphrase: String,
    #[serde(default)] main_suggestion: Option<TextReason>,
    #[serde(default)] current: Option<CurrentOp>,
    #[serde(default)] subs: Vec<SubOp>,
    #[serde(default)] constraints: Vec<ConstraintSuggestion>,
}

/// Pull the JSON object out of the model's output (tolerates ``` fences / prose) and parse it.
fn parse_observer(out: &str) -> Option<ObserverDecision> {
    let start = out.find('{')?;
    let end = out.rfind('}')?;
    if end < start { return None; }
    serde_json::from_str(&out[start..=end]).ok()
}

/// The sorter agent's decision: which existing folder a session's goal docs belong to.
#[derive(Clone, Debug, PartialEq)]
struct SortDecision {
    project_dir: String,        // an absolute path copied verbatim from the candidate folders
    project_key: String,        // short stable key to index the project's plans by (THREAD project)
    plan_basenames: Vec<String>,// plan files the agent thinks belong here (advisory; never moved)
    reason: String,
}

/// Defensively parse the sorter's reply: tolerate code fences / prose around the JSON, reject refusals
/// ("NO MATCH" carries no braces, so it falls out here). `project_dir` is required and non-empty; the
/// rest default. Pure — invents nothing. Returns `None` on a refusal or unparseable / dir-less output.
fn parse_sort(out: &str) -> Option<SortDecision> {
    if looks_like_refusal(out) { return None; }
    let start = out.find('{')?;
    let end = out.rfind('}')?;
    if end < start { return None; }
    let v: serde_json::Value = serde_json::from_str(&out[start..=end]).ok()?;
    let project_dir = v.get("project_dir").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if project_dir.is_empty() { return None; }
    let project_key = v.get("project_key").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let plan_basenames = v.get("plan_basenames").and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).collect())
        .unwrap_or_default();
    let reason = v.get("reason").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    Some(SortDecision { project_dir, project_key, plan_basenames, reason })
}

/// A picked folder is valid ONLY if it exists AND is one of the candidates we offered (no fabricated /
/// off-list folder, no path traversal to somewhere we never proposed). Compares by canonical path when
/// possible so `~`-expanded / symlinked forms still match. Pure.
fn validate_pick(dir: &Path, candidates: &[PathBuf]) -> bool {
    if !dir.is_dir() { return false; }
    let canon = std::fs::canonicalize(dir).ok();
    candidates.iter().any(|c| c == dir || (canon.is_some() && std::fs::canonicalize(c).ok() == canon))
}

/// Compact JSON view of the tree handed to the observer (ownership made explicit).
fn goal_context_json(doc: &GoalDoc) -> String {
    let owner = |source: &str, approved: bool, pinned: bool| if pinned || source == "user" || approved { "user" } else { "agent" };
    let subs: Vec<serde_json::Value> = doc.subs.iter().map(|s| serde_json::json!({
        "id": s.id, "text": s.text, "state": s.state, "parent": s.parent,
        "owner": owner(&s.source, s.approved, s.pinned)
    })).collect();
    serde_json::json!({
        "main":    { "text": doc.main.text,    "owner": owner(&doc.main.source, doc.main.approved, false) },
        "current": { "text": doc.current.text, "owner": owner(&doc.current.source, doc.current.approved, false) },
        "subs": subs
    }).to_string()
}

/// Queue a proposal (MAIN change, or a CURRENT change the user owns). Skips dismissed/duplicate ideas.
fn propose(doc: &mut GoalDoc, kind: &str, text: &str, reason: &str) -> bool {
    let n = norm(text);
    if n.is_empty() || doc.dismissed.iter().any(|d| *d == n) { return false; }
    if doc.proposals.iter().any(|p| p.kind == kind && norm(&p.text) == n) { return false; }
    doc.proposals.push(Proposal { kind: kind.into(), text: clip_inline(text), reason: clip_inline(reason), ts: now_stamp() });
    true
}

/// Apply the observer's decision to the doc with WIDGET-ENFORCED ownership (never trust the model to
/// respect ownership): MAIN is always a suggestion; CURRENT auto-changes only if agent-owned else a
/// suggestion; subs are agent-managed but user-pinned subs are untouchable. Returns the prompt-log row.
fn apply_observer(doc: &mut GoalDoc, store: &GoalStore, d: ObserverDecision) -> PromptEntry {
    let mut actions: Vec<&str> = Vec::new();
    let mut reason_acc = String::new();
    let mut hist: Vec<(String, GoalState)> = Vec::new();

    // ---- CURRENT: auto iff agent-owned, else downgrade to a suggestion ----
    if let Some(c) = d.current {
        let text: String = c.text.trim().chars().take(MAX_INLINE).collect();
        let wants = matches!(c.action.as_str(), "replace" | "suggest") && has_real(&text) && !looks_like_refusal(&text) && norm(&text) != norm(&doc.current.text);
        if wants {
            let user_owns = doc.current.source == "user" || doc.current.approved;
            if user_owns {
                if propose(doc, "current", &text, &c.reason) { actions.push("current?"); if reason_acc.is_empty() { reason_acc = c.reason.clone(); } }
            } else {
                doc.current = GoalState { text, source: "agent".into(), approved: false, state: String::new(), updated_at: now_stamp(), set_at: now_stamp(), reason: Some(c.reason.clone()), ..Default::default() };
                fill_estimate(&mut doc.current, &store.root());
                hist.push(("current".into(), doc.current.clone()));
                actions.push("current");
                if reason_acc.is_empty() { reason_acc = c.reason.clone(); }
            }
        }
    }

    // ---- MAIN: never auto — always a suggestion ----
    if let Some(m) = d.main_suggestion {
        let text: String = clip_inline(m.text.trim());
        if has_real(&text) && !looks_like_refusal(&text) && norm(&text) != norm(&doc.main.text)
            && propose(doc, "main", &text, &m.reason) { actions.push("main?"); if reason_acc.is_empty() { reason_acc = m.reason.clone(); } }
    }

    // ---- SUBS: agent-managed; user-pinned subs are off-limits ----
    for op in d.subs {
        match op.op.as_str() {
            "add" => {
                let text: String = op.text.unwrap_or_default().trim().chars().take(MAX_INLINE).collect();
                let n = norm(&text);
                if n.is_empty() || looks_like_refusal(&text) || doc.subs.iter().any(|s| norm(&s.text) == n) { continue; }
                let parent = normalize_parent(op.parent.as_deref().unwrap_or("current"));
                let id = doc.next_id; doc.next_id += 1;
                let sub = Sub { id, text, source: "agent".into(), approved: false, state: "active".into(), parent, created_at: now_stamp(), updated_at: now_stamp(), pinned: false };
                let mut snap = sub.snapshot(); snap.reason = Some(op.reason.clone());
                hist.push((format!("sub-{id}"), snap));
                doc.subs.push(sub);
                actions.push("sub+");
            }
            "update" | "state" | "discard" | "reparent" => {
                let Some(id) = op.id else { continue };
                let Some(sub) = doc.subs.iter_mut().find(|s| s.id == id) else { continue };
                if sub.pinned || sub.source == "user" || sub.approved { continue; } // your override sticks
                let mut touched = false;
                match op.op.as_str() {
                    "discard" => { sub.state = "discarded".into(); touched = true; }
                    "state" => if let Some(st) = op.state.as_deref() { if SUB_STATES.contains(&st) { sub.state = st.into(); touched = true; } },
                    "reparent" => if let Some(p) = op.parent.as_deref() { sub.parent = normalize_parent(p); touched = true; },
                    "update" => if let Some(t) = op.text.as_deref() { if has_real(t) && !looks_like_refusal(t) { sub.text = t.trim().chars().take(MAX_INLINE).collect(); touched = true; } },
                    _ => {}
                }
                if touched {
                    sub.updated_at = now_stamp();
                    let mut snap = sub.snapshot(); snap.reason = Some(op.reason.clone());
                    hist.push((format!("sub-{id}"), snap));
                    actions.push("sub~");
                }
            }
            _ => {}
        }
    }

    // ---- CONSTRAINTS: agent only ever SUGGESTS (as a proposal); user must accept. Never auto-added.
    for cs in d.constraints {
        let entity = if cs.entity == "main" { "main" } else { "current" };
        let text: String = cs.text.trim().chars().take(MAX_INLINE).collect();
        if !has_real(&text) { continue; }
        let n = norm(&text);
        if doc.constraints.iter().any(|c| c.entity == entity && norm(&c.text) == n) { continue; } // already a bound
        if propose(doc, &format!("constraint:{entity}"), &text, &cs.reason) { actions.push("constraint?"); }
    }

    for (entity, snap) in &hist { store.append_history(entity, snap); }
    let action = if actions.is_empty() { "noop".to_string() } else { actions.join(",") };
    PromptEntry { ts: now_stamp(), paraphrase: d.paraphrase.trim().chars().take(240).collect(), action, reason: reason_acc.chars().take(200).collect() }
}

/// Build the full observer prompt (instructions + ownership rules + tree + prompt-log + new activity).
fn build_observer_prompt(doc: &GoalDoc, log_tail: &[PromptEntry], delta: &str) -> String {
    let mut log = String::new();
    for e in log_tail { log.push_str(&format!("- ({}) {} [{}]\n", e.ts, e.paraphrase, e.action)); }
    if log.is_empty() { log.push_str("(none yet)\n"); }
    format!(
        "You are the GOAL OBSERVER for a developer's coding session. You silently maintain a goal tree:\n\
         MAIN = the long-running north-star goal; CURRENT = the goal being worked on right now; SUBS =\n\
         the task list under them. You run once per developer prompt with full context and decide what\n\
         (if anything) changed. You output STRICT JSON only — no prose, no markdown fences.\n\n\
         RULES:\n\
         1. paraphrase: ALWAYS restate the developer's latest intent in plain words, <=240 chars. This\n\
            is shown back to them to confirm you understood — be faithful, not creative.\n\
         2. current: set action=\"replace\" with text+reason ONLY if the active goal genuinely shifted to\n\
            something new. If it's the same goal, or just steering/continuation/a fix, use action=\"keep\".\n\
            Do NOT churn on minor wording. (Ownership is handled by the app — you just judge the shift.)\n\
            Keep the goal text <=240 chars — a single concise line. NEVER pack detail into the goal text;\n\
            if more is needed, express it as subs (tasks) or constraints (bounds), not a longer title.\n\
         3. main_suggestion: include ONLY if the OVERARCHING goal itself has changed (rare). Never for\n\
            normal work. text (<=240 chars, one concise line) + reason. Always shown as a suggestion,\n\
            never auto-applied. Extra detail belongs in subs/constraints, never in a longer goal line.\n\
         4. subs: manage the task list from what ACTUALLY happened (the developer's prompt AND Claude's\n\
            replies/tool results below): op=\"add\" {{text,parent,reason}} for a new task (parent one of\n\
            current|main|independent, default current); op=\"state\" {{id,state,reason}} with state one of\n\
            active|paused|done|discarded; op=\"discard\" {{id,reason}}; op=\"update\" {{id,text,reason}};\n\
            op=\"reparent\" {{id,parent,reason}}. Reference existing subs by id. Mark a sub done/discarded\n\
            based on real evidence in the activity, not speculation. Don't re-add an existing task.\n\
         5. constraints: OPTIONALLY suggest a hard boundary the goal must respect — a rule that, if broken,\n\
            means the goal was NOT reached (e.g. \"no new dependencies\", \"merge-commit only\"). Only when a\n\
            real bound is evident in the activity; entity one of main|current, with text+reason. Always a\n\
            SUGGESTION (shown for the user to accept), never auto-applied. Omit when none is evident.\n\
         6. If nothing changed (pure steering/continuation), return ONLY the paraphrase and empty/omitted\n\
            current/main/subs/constraints.\n\n\
         Subs marked owner=\"user\" were set by the developer — DO NOT modify or discard them.\n\n\
         OUTPUT SCHEMA (JSON):\n\
         {{\"paraphrase\":\"...\",\"current\":{{\"action\":\"keep|replace\",\"text\":\"\",\"reason\":\"\"}},\
         \"main_suggestion\":{{\"text\":\"\",\"reason\":\"\"}},\"subs\":[{{\"op\":\"add|state|discard|update|reparent\",\
         \"id\":0,\"text\":\"\",\"state\":\"\",\"parent\":\"\",\"reason\":\"\"}}],\
         \"constraints\":[{{\"entity\":\"main|current\",\"text\":\"\",\"reason\":\"\"}}]}}\n\
         Omit main_suggestion if MAIN didn't change. Use current.action=\"keep\" and subs=[]/constraints=[] when nothing changed.\n\n\
         === CURRENT GOAL TREE ===\n{tree}\n\n\
         === RECENT PROMPT LOG (newest last) ===\n{log}\n\
         === NEW ACTIVITY THIS TURN (developer prompts + Claude replies/results) ===\n{delta}\n\n\
         Respond with the JSON object only.",
        tree = goal_context_json(doc), log = log, delta = delta
    )
}

/// `goal-widget --observe <sid>`: process new turns since the cursor, update the tree, log the prompt.
pub fn run_observe(sid: &str) -> i32 {
    let Some(path) = session_jsonl(sid) else { eprintln!("observe: no transcript for {sid}"); return 1; };
    let store = GoalStore::resolve(sid);
    let mut doc = store.load();
    let since = doc.last_turn as usize;
    let (delta, total) = transcript_delta(&path, since);
    if total <= since || delta.trim().is_empty() {
        // nothing new to interpret — advance cursor so we don't rescan, write nothing.
        if total != since { doc.last_turn = total as u64; let _ = store.save(&doc); }
        return 0;
    }
    let log = store.read_prompt_log();
    let tail = &log[log.len().saturating_sub(12)..];
    let prompt = build_observer_prompt(&doc, tail, &delta);
    let Some(out) = claude_observe(prompt, Duration::from_secs(180)) else {
        eprintln!("observe: agent produced no output");
        return 1;
    };
    let Some(decision) = parse_observer(&out) else {
        // couldn't interpret — advance cursor (avoid a rescan storm), invent nothing.
        doc.last_turn = total as u64;
        let _ = store.save(&doc);
        eprintln!("observe: could not parse agent output");
        return 1;
    };
    let entry = apply_observer(&mut doc, &store, decision);
    doc.paraphrase = entry.paraphrase.clone();
    doc.last_turn = total as u64;
    let _ = store.save(&doc);
    store.append_prompt_log(&entry);
    println!("observe: {} ({})", entry.action, entry.paraphrase);
    0
}

/// Build the SORTER prompt: from the goals + recent prompts, pick which EXISTING candidate folder these
/// docs belong to. Folders + plans are given verbatim so the agent copies a real path (never invents one).
fn build_sort_prompt(doc: &GoalDoc, prompts: &str, candidates: &[PathBuf], plans: &str) -> String {
    let cand = candidates.iter().map(|p| format!("- {}", p.display())).collect::<Vec<_>>().join("\n");
    let main = if has_real(&doc.main.text) { doc.main.text.as_str() } else { "—" };
    let current = if has_real(&doc.current.text) { doc.current.text.as_str() } else { "—" };
    let prompts = if prompts.trim().is_empty() { "—" } else { prompts };
    format!(
        "You are a documentation SORTER for an unorganized developer workspace. Given a session's goals\n\
         and recent requests, decide which EXISTING project folder these goal documents belong to. Pick\n\
         project_dir ONLY from the candidate list, copied EXACTLY — never invent or guess a folder.\n\n\
         === OVERALL (MAIN) GOAL ===\n{main}\n\n\
         === CURRENT GOAL ===\n{current}\n\n\
         === RECENT USER PROMPTS (oldest first) ===\n{prompts}\n\n\
         === CANDIDATE PROJECT FOLDERS (project_dir MUST be one of these, verbatim) ===\n{cand}\n\n\
         === EXISTING PLANS (basename · PROJECT · title) ===\n{plans}\n\n\
         Respond with ONLY this JSON object (no prose, no fences):\n\
         {{\"project_dir\": \"<one absolute path copied verbatim from the candidates>\", \
         \"project_key\": \"<short stable key to index this project's plans by, e.g. the folder name>\", \
         \"plan_basenames\": [\"<plans that belong to this project>\"], \"reason\": \"<one line>\"}}\n\
         If you cannot confidently match one of the candidate folders, reply with exactly: NO MATCH"
    )
}

/// `goal-widget --sort <sid>`: AI-infer which existing project folder this session's goal docs belong to,
/// set the goal's `project_key` (so its plans index), and MOVE the goal docs into that folder. Headless —
/// the TUI spawns it detached and reloads from the repointed link. Changes nothing on a refusal / bad pick.
pub fn run_sort(sid: &str) -> i32 {
    let store = GoalStore::resolve(sid);
    let mut doc = store.load();
    let cwd = session_cwd(sid);
    let candidates = candidate_projects(cwd.as_deref());
    if candidates.is_empty() { eprintln!("sort: no candidate project folders under ~/Projects"); return 1; }
    let prompts = session_jsonl(sid).map(|p| user_prompts(&p, 8)).unwrap_or_default();
    let prompt = build_sort_prompt(&doc, &prompts, &candidates, &plan_inventory_block());
    let Some(out) = claude_observe(prompt, Duration::from_secs(180)) else {
        eprintln!("sort: agent produced no output");
        return 1;
    };
    let Some(dec) = parse_sort(&out) else { eprintln!("sort: no confident match (refusal / unparseable)"); return 1; };
    let dir = expand_tilde(&dec.project_dir);
    if !validate_pick(&dir, &candidates) {
        eprintln!("sort: '{}' is not an existing candidate folder — nothing moved", dir.display());
        return 1;
    }
    let dest = project_goal_file(&dir);
    let new_key = dec.project_key.trim();
    let key_changes = !new_key.is_empty() && new_key != doc.project_key.trim();
    if dest == store.path && !key_changes {
        println!("sort: already sorted in {}", dir.display());
        return 0;
    }
    if key_changes { doc.project_key = new_key.to_string(); }
    // persist the key on the current store first (covers the already-in-folder, key-only case)
    let _ = store.save(&doc);
    match move_goal_store(&store, &doc, &dir) {
        Ok(_) => { println!("sort: moved to {} (key={})", dir.display(), doc.project_key); 0 }
        Err(e) => { eprintln!("sort: move failed: {e}"); 1 }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_goal_empty_activity_is_none() {
        assert!(infer_goal("   ").is_none());
    }

    #[test]
    fn refusal_detector_flags_empty_activity_replies() {
        // The exact strings that leaked into stored goals (the bug this guards against).
        for s in [
            "Your prompt appears incomplete. You've provided the instruction template but haven't included:",
            "Provide activity data to analyze",
            "No recent activity provided after '--- activity ---' line",
            "Please share the developer's session activity",
            "Will return goal breakdown once data is available",
            "Your message got cut off.",
            "Unable to determine - no activity data provided",
            "I'm unable to infer a goal without context",
        ] {
            assert!(looks_like_refusal(s), "should flag refusal: {s:?}");
        }
        // Real imperative goals must NOT be flagged.
        for s in [
            "Build session-aware goal inference and management system",
            "Add a project-scoped plans section to the goal widget",
            "Ship the warm-REPL migration to production",
            "Refactor the observer to dedup subgoals",
        ] {
            assert!(!looks_like_refusal(s), "should NOT flag real goal: {s:?}");
        }
    }

    #[test]
    fn parse_infer_tree_json_drops_refusal_text() {
        // A refusal disguised as JSON yields no usable goal.
        let out = r#"{"current":"Provide activity data to analyze","subgoals":["No recent activity provided after '--- activity ---' line","Please share the developer's session activity"]}"#;
        assert!(parse_infer_tree_json(out).is_none(), "refusal JSON must be rejected");
        // A real reply still parses, and any stray refusal sub is filtered out.
        let out2 = r#"{"current":"Wire up the plans section","subgoals":["Add list_project_plans","Please provide activity"]}"#;
        let (cur, subs) = parse_infer_tree_json(out2).unwrap();
        assert_eq!(cur, "Wire up the plans section");
        assert_eq!(subs, vec!["Add list_project_plans".to_string()]);
    }
}
