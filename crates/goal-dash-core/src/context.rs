//! Transcript / project-goal readers and the carry-forward `/clear` rebrief
//! composer, carved verbatim from goal-widget `src/main.rs`. Pure: filesystem
//! reads + string composition only — no `App`, no ratatui, no cmux execution
//! (the host runs the command vectors `clear_rebrief_cmds` returns).

use std::fs;
use std::path::{Path, PathBuf};

pub fn recent_activity(path: &PathBuf, max_msgs: usize) -> String {
    let Ok(content) = fs::read_to_string(path) else {
        return String::new();
    };
    let mut lines: Vec<String> = Vec::new();
    for raw in content.lines() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let Ok(e) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        let role = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if role != "user" && role != "assistant" {
            continue;
        }
        let content = e.get("message").and_then(|m| m.get("content"));
        let mut text = String::new();
        match content {
            Some(serde_json::Value::String(s)) => text = s.clone(),
            Some(serde_json::Value::Array(blocks)) => {
                let parts: Vec<&str> = blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect();
                text = parts.join(" ");
            }
            _ => {}
        }
        let text = text.trim().replace('\n', " ");
        if !text.is_empty() {
            let snippet: String = text.chars().take(300).collect();
            lines.push(format!("{role}: {snippet}"));
        }
    }
    let start = lines.len().saturating_sub(max_msgs);
    lines[start..].join("\n")
}

/// The project's stated goal: the `## Goal` section of `<root>/CLAUDE.md` (the user's own convention),
/// captured until the next `## ` heading. Best single source of the north-star MAIN goal.
pub fn claude_md_goal(root: &Path) -> Option<String> {
    let content = fs::read_to_string(root.join("CLAUDE.md")).ok()?;
    let mut out = String::new();
    let mut in_goal = false;
    for line in content.lines() {
        let t = line.trim_start();
        if let Some(heading) = t.strip_prefix("## ") {
            if in_goal {
                break;
            } // end of the Goal section
            in_goal = heading.trim().to_lowercase().starts_with("goal");
            continue;
        }
        if in_goal {
            out.push_str(line);
            out.push('\n');
        }
    }
    let out = out.trim();
    if out.is_empty() {
        None
    } else {
        Some(out.chars().take(1500).collect())
    }
}

/// The developer's own prompts (the "sum of what they asked for"), oldest→newest, skipping
/// tool-result echoes and meta lines. This is the throughline the north-star MAIN goal should track.
pub fn user_prompts(path: &Path, max_msgs: usize) -> String {
    let Ok(content) = fs::read_to_string(path) else {
        return String::new();
    };
    let mut prompts: Vec<String> = Vec::new();
    for raw in content.lines() {
        let Ok(e) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
            continue;
        };
        if e.get("type").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let c = e.get("message").and_then(|m| m.get("content"));
        let text = match c {
            Some(serde_json::Value::String(s)) => s.clone(),
            // array form: keep only genuine text blocks (skip tool_result / images)
            Some(serde_json::Value::Array(blocks)) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        };
        let text = text.trim().replace('\n', " ");
        // skip command/hook noise and empty/tool-only turns
        if text.is_empty() || text.starts_with("<") || text.starts_with("Caveat:") {
            continue;
        }
        prompts.push(format!("- {}", text.chars().take(200).collect::<String>()));
    }
    let start = prompts.len().saturating_sub(max_msgs);
    prompts[start..].join("\n")
}

/// Build the carry-forward briefing pasted into a freshly `/clear`ed conversation. Pure: the caller
/// hands in the goals (already in memory) and the recent-prompts block (from `user_prompts`), plus the
/// resolved transcript path and session cwd for the reconstruction scaffold. A missing goal renders as
/// a dash — never a fabricated line. Scaffold wording mirrors `~/.claude/hooks/compact-context/briefing.md`.
pub fn compose_rebrief(
    main: Option<&str>,
    current: &str,
    prompts: &str,
    transcript: &Path,
    cwd: &Path,
) -> String {
    let dash = "—";
    let main = main
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(dash);
    let current = {
        let c = current.trim();
        if c.is_empty() {
            dash
        } else {
            c
        }
    };
    let prompts = {
        let p = prompts.trim();
        if p.is_empty() {
            dash
        } else {
            p
        }
    };
    let tp = transcript.display();
    let cwd = cwd.display();
    format!(
        "<rebrief>\n\
You just ran a TRUE /clear to start a fresh conversation (NOT a compaction). The goals and recent\n\
requests below are carried forward from the previous context. Reconstruct working state from them,\n\
verify against the pre-clear transcript using the searches below, then proceed.\n\
\n\
## Overall goal\n\
{main}\n\
\n\
## Current goal\n\
{current}\n\
\n\
## Recent prompts (oldest first)\n\
{prompts}\n\
\n\
## Pre-clear transcript\n\
{tp}\n\
\n\
## How to search (use sparingly, ≤3 calls)\n\
- grep -a '<keyword>' {tp} | tail -30\n\
- For the active plan: `ls -t ~/.claude/plans/*.md | head -3`, then read the one whose THREAD header matches this cwd ({cwd}).\n\
\n\
## Fill BEFORE your next action\n\
- Last thing I did:\n\
- Plan I'm following (path):\n\
- Known issues:\n\
- What wasted tokens last session (DO NOT repeat):\n\
\n\
## Investigation principle (READ)\n\
If something looks wrong, NEVER default-assume it was a past mistake — grep the transcript above to find WHY before reverting.\n\
</rebrief>"
    )
}

/// The single-line prompt sent after `/clear` that points Claude at the staged briefing file. MUST be
/// one line — `cmux send` turns every newline into Enter, so any newline here would submit early and
/// shred the rest. The full multi-line briefing lives in the file at `path`; Claude reads it.
pub fn rebrief_read_prompt(path: &str) -> String {
    format!(
        "Carry-forward briefing after a TRUE /clear (NOT a compaction). Read {path} in full now and \
         reconstruct working state from it before doing anything else."
    )
}

/// The exact ordered cmux argument vectors for a clear+rebrief on `surface`. Separated from execution
/// so the sequence (TC6) is assertable without mocking cmux: `send '/clear'`, `send-key Enter`,
/// `send <one-line read prompt>`, `send-key Enter` — each its own process; `/clear` and the prompt are
/// never chained.
///
/// `brief_path` is a FILE holding the multi-line briefing. We send a one-line "read this file" prompt
/// rather than the brief itself: `cmux send` (and `paste-buffer`) convert every newline to Enter, so a
/// multi-line payload submits per-line and is shredded in transit (proven against a real surface). A
/// single-line prompt has no newline to break on; Claude reads the staged brief in full.
pub fn clear_rebrief_cmds(surface: &str, brief_path: &str) -> Vec<Vec<String>> {
    let s = surface.to_string();
    let prompt = rebrief_read_prompt(brief_path);
    vec![
        vec![
            "send".into(),
            "--surface".into(),
            s.clone(),
            "/clear".into(),
        ],
        vec![
            "send-key".into(),
            "--surface".into(),
            s.clone(),
            "Enter".into(),
        ],
        vec!["send".into(), "--surface".into(), s.clone(), prompt],
        vec!["send-key".into(), "--surface".into(), s, "Enter".into()],
    ]
}

/// Run the clear+rebrief sequence on `surface` in a detached thread (never blocks the event loop).
/// 1s settle between each step so Claude's REPL processes `/clear` before the read prompt is sent.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    #[test]
    fn recent_activity_extracts_text_blocks() {
        let dir = std::env::temp_dir().join(format!("gw_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        fs::write(
            &p,
            "{\"type\":\"user\",\"message\":{\"content\":\"build the thing\"}}\n\
             {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"on it\"}]}}\n\
             {\"type\":\"system\",\"message\":{\"content\":\"ignore me\"}}\n",
        )
        .unwrap();
        let act = recent_activity(&p, 18);
        assert!(act.contains("user: build the thing"));
        assert!(act.contains("assistant: on it"));
        assert!(!act.contains("ignore me"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compose_rebrief_includes_goals_prompts_and_scaffold() {
        // TC1: build the recent-prompts block from a REAL transcript (also proves "no plus prompt":
        // <…>-prefixed / Caveat / tool-only lines are filtered, only the last 5 survive).
        let dir = std::env::temp_dir().join(format!("goal-widget-rebrief-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tp = dir.join("sess.jsonl");
        let lines = [
            r#"{"type":"user","message":{"content":"<system-reminder>injected</system-reminder>"}}"#,
            r#"{"type":"user","message":{"content":"Caveat: the following is auto"}}"#,
            r#"{"type":"user","message":{"content":"first real prompt"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}"#,
            r#"{"type":"user","message":{"content":"prompt two"}}"#,
            r#"{"type":"user","message":{"content":"prompt three"}}"#,
            r#"{"type":"user","message":{"content":"prompt four"}}"#,
            r#"{"type":"user","message":{"content":"prompt five"}}"#,
            r#"{"type":"user","message":{"content":"prompt six"}}"#,
        ];
        std::fs::write(&tp, lines.join("\n") + "\n").unwrap();
        let prompts = user_prompts(&tp, 5);
        let cwd = PathBuf::from("/Users/tester/Projects/goal-widget");
        let out = compose_rebrief(
            Some("Ship the goal dash"),
            "Add clear+rebrief button",
            &prompts,
            &tp,
            &cwd,
        );

        assert!(out.contains("Ship the goal dash"), "main goal missing");
        assert!(
            out.contains("Add clear+rebrief button"),
            "current goal missing"
        );
        // exactly the last 5 genuine prompts, oldest of the five first; the injected/Caveat/tool lines gone.
        assert!(out.contains("- prompt two") && out.contains("- prompt six"));
        assert!(
            !out.contains("first real prompt"),
            "should keep only last 5"
        );
        assert!(
            !out.contains("injected") && !out.contains("Caveat:"),
            "plus/injected prompts leaked"
        );
        // scaffold lines (literal).
        assert!(out.contains(&format!("grep -a '<keyword>' {} | tail -30", tp.display())));
        assert!(out.contains("ls -t ~/.claude/plans/*.md | head -3"));
        assert!(out.contains("THREAD header matches this cwd (/Users/tester/Projects/goal-widget)"));
        assert!(out.contains("Known issues:"));
        assert!(out.contains("What wasted tokens last session"));
        // it's a TRUE clear, never a compact.
        assert!(out.contains("TRUE /clear") && !out.contains("/compact"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compose_rebrief_missing_pieces_render_as_dash() {
        // TC3: no main, no current, no prompts ⇒ dashes, never fabricated, never a crash.
        let out = compose_rebrief(None, "", "", Path::new("/tmp/t.jsonl"), Path::new("/tmp"));
        assert!(out.contains("## Overall goal\n—"));
        assert!(out.contains("## Current goal\n—"));
        assert!(out.contains("## Recent prompts (oldest first)\n—"));
        // an all-whitespace goal also degrades to a dash.
        let out2 = compose_rebrief(
            Some("   "),
            "  ",
            "  ",
            Path::new("/tmp/t.jsonl"),
            Path::new("/tmp"),
        );
        assert!(out2.contains("## Overall goal\n—") && out2.contains("## Current goal\n—"));
    }

    #[test]
    fn clear_rebrief_cmds_emits_clear_then_brief_unchained() {
        // TC6: exact ordered, separate cmux invocations; /clear and the read-prompt are never chained.
        // The multi-line brief lives in a FILE; the pasted prompt is ONE line pointing at it — `cmux
        // send` turns every newline into Enter, so an inline multi-line brief would submit per-line.
        let path = "/Users/x/.claude/goal/SID.rebrief.md";
        let cmds = clear_rebrief_cmds("surface:42", path);
        assert_eq!(cmds.len(), 4);
        assert_eq!(cmds[0], vec!["send", "--surface", "surface:42", "/clear"]);
        assert_eq!(
            cmds[1],
            vec!["send-key", "--surface", "surface:42", "Enter"]
        );
        assert_eq!(cmds[2][0..3], ["send", "--surface", "surface:42"]);
        assert_eq!(
            cmds[3],
            vec!["send-key", "--surface", "surface:42", "Enter"]
        );
        // /clear is its own payload — the read prompt never rides along with it.
        assert!(!cmds[0].iter().any(|a| a.contains(".rebrief.md")));
        // The read prompt references the staged file and is STRICTLY single-line (no newline to shred).
        let prompt = &cmds[2][3];
        assert!(
            prompt.contains(path),
            "prompt must point at the staged brief file"
        );
        assert!(
            !prompt.contains('\n') && !prompt.contains('\r'),
            "read prompt must be single-line"
        );
    }
}
