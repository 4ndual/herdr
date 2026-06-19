//! Time-gate + plans + path helpers ported VERBATIM from goal-widget's `main.rs`
//! (~lines 61–66, 280–533, 581–601): home/state-dir/plans-dir paths, the project-root
//! walk, `~/.claude/plans` scoping (`norm_proj`, `parse_thread_project`, `parse_timegate`,
//! `timegate_estimate`, `list_project_plans`, `plan_inventory_block`, watermark), the
//! est/deadline math (`add_minutes`, `minutes_until`, `left_label`, `parse_duration_min`,
//! `fill_estimate`, `est_label`), and `cwd_from_jsonl`. No behavior changed.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::model::{GoalState, GOAL_FILE};

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME not set"))
}
pub fn state_dir() -> PathBuf {
    home().join(".claude/goal")
}

pub fn project_goal_file(dir: &Path) -> PathBuf {
    dir.join(GOAL_FILE)
}

/// The project root for a working dir: nearest ancestor holding `.git`, `CLAUDE.md`, or an existing
/// `.goal.json`. `$HOME` and `~/.claude` are never project roots.
pub fn project_root(dir: &Path) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    let skip: Vec<PathBuf> = home.iter().flat_map(|h| [h.clone(), h.join(".claude")]).collect();
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if !skip.iter().any(|s| s == d)
            && (d.join(".git").exists() || d.join("CLAUDE.md").exists() || d.join(GOAL_FILE).exists())
        {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}
pub fn global_goal_path(goal_id: &str) -> PathBuf {
    state_dir().join(format!("{goal_id}.goal.json"))
}

pub fn plans_dir() -> PathBuf {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
    home.join(".claude").join("plans")
}

/// `~/Projects` — the root holding per-project folders, the candidate set for the sorter.
pub fn projects_home() -> PathBuf {
    home().join("Projects")
}

/// Normalize a project label for matching ("GOAL-WIDGET", "Goal Widget", "goal_widget" → "goal-widget").
pub fn norm_proj(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c == ' ' || c == '_' { '-' } else { c })
        .collect()
}

/// Parse the PROJECT token out of a plan's `<!-- THREAD: PROJECT — thread | type -->` header.
pub fn parse_thread_project(content: &str) -> Option<String> {
    for line in content.lines() {
        if let Some(i) = line.find("THREAD:") {
            let rest = &line[i + "THREAD:".len()..];
            let end = rest
                .find('—')
                .or_else(|| rest.find(" - "))
                .or_else(|| rest.find('|'))
                .or_else(|| rest.find("-->"))
                .unwrap_or(rest.len());
            let proj = rest[..end].trim();
            if !proj.is_empty() {
                return Some(proj.to_string());
            }
        }
    }
    None
}

/// Parse `p50=` and `p90=` (minutes) out of a plan's `<!-- TIME-GATE: ... -->` trailer.
pub fn parse_timegate(content: &str) -> Option<(u32, u32)> {
    for line in content.lines() {
        if !line.contains("TIME-GATE:") {
            continue;
        }
        let grab = |key: &str| -> Option<u32> {
            let i = line.find(key)?;
            let rest = &line[i + key.len()..];
            let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            num.parse().ok()
        };
        if let (Some(p50), Some(p90)) = (grab("p50="), grab("p90=")) {
            return Some((p50, p90));
        }
    }
    None
}

/// p50/p90 from the most-recent time-gate plan whose THREAD project matches this root's dir name.
pub fn timegate_estimate(root: &Path) -> Option<(u32, u32)> {
    let target = norm_proj(root.file_name()?.to_str()?);
    let mut best: Option<(SystemTime, (u32, u32))> = None;
    for entry in std::fs::read_dir(plans_dir()).ok()?.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if parse_thread_project(&content).map(|x| norm_proj(&x)) != Some(target.clone()) {
            continue;
        }
        let est = match parse_timegate(&content) {
            Some(e) => e,
            None => continue,
        };
        let mtime = match entry.metadata().and_then(|m| m.modified()).ok() {
            Some(t) => t,
            None => continue,
        };
        if best.as_ref().map_or(true, |(bt, _)| mtime > *bt) {
            best = Some((mtime, est));
        }
    }
    best.map(|(_, e)| e)
}

/// One project-scoped plan, as listed in the goal pane's plans section.
#[derive(Clone)]
pub struct PlanMeta {
    pub path: PathBuf,
    pub basename: String,
    pub title: String,
    pub p50: Option<u32>,
    pub mtime_ms: i64,
}

/// A plan's display title = its first `# ` H1 line; falls back to the filename.
pub fn plan_title(content: &str, basename: &str) -> String {
    for line in content.lines() {
        if let Some(t) = line.strip_prefix("# ") {
            let t = t.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    basename.to_string()
}

/// The normalized key a project's plans are matched against: explicit `project_key` else folder name.
pub fn project_match_key(root: &Path, project_key: &str) -> Option<String> {
    let k = project_key.trim();
    if !k.is_empty() {
        return Some(norm_proj(k));
    }
    let name = root.file_name()?.to_str()?;
    let n = norm_proj(name);
    if n.is_empty() {
        None
    } else {
        Some(n)
    }
}

/// Decide which key (if any) to scope the plans list by. Explicit key always wins; a global goal with
/// no key has no project (None); a real project root with no key falls back to its folder name.
pub fn plans_scope_key(root: &Path, project_key: &str) -> Option<String> {
    if !project_key.trim().is_empty() {
        return project_match_key(root, project_key);
    }
    if root.starts_with(state_dir()) {
        return None;
    }
    project_match_key(root, project_key)
}

/// A change-watermark for `~/.claude/plans`: max of dir mtime AND newest plan file's mtime.
pub fn plans_watermark() -> Option<SystemTime> {
    plans_watermark_in(&plans_dir())
}

pub fn plans_watermark_in(dir: &Path) -> Option<SystemTime> {
    let mut latest = std::fs::metadata(dir).and_then(|m| m.modified()).ok()?;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            if entry.path().extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Ok(m) = entry.metadata().and_then(|m| m.modified()) {
                if m > latest {
                    latest = m;
                }
            }
        }
    }
    Some(latest)
}

/// All time-gate plans whose THREAD project norm-matches `key`, newest change first.
pub fn list_project_plans(key: &str) -> Vec<PlanMeta> {
    list_project_plans_in(&plans_dir(), key)
}

pub fn list_project_plans_in(dir: &Path, key: &str) -> Vec<PlanMeta> {
    let mut out: Vec<PlanMeta> = Vec::new();
    if key.is_empty() {
        return out;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if parse_thread_project(&content).map(|x| norm_proj(&x)).as_deref() != Some(key) {
            continue;
        }
        let basename = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let title = plan_title(&content, &basename);
        let p50 = parse_timegate(&content).map(|(p50, _)| p50);
        let mtime_ms = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        out.push(PlanMeta { path: p, basename, title, p50, mtime_ms });
    }
    out.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms));
    out
}

/// Existing project folders a session's goal docs could be sorted INTO.
pub fn candidate_projects(session_cwd: Option<&Path>) -> Vec<PathBuf> {
    candidate_projects_in(&projects_home(), session_cwd.and_then(project_root))
}

pub fn candidate_projects_in(projects_root: &Path, extra: Option<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(projects_root) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                out.push(p);
            }
        }
    }
    if let Some(e) = extra {
        if e.is_dir() {
            out.push(e);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A one-line-per-plan inventory of `~/.claude/plans` for the sorter's context.
pub fn plan_inventory_block_in(dir: &Path, cap: usize) -> String {
    let mut rows: Vec<String> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(content) = fs::read_to_string(&p) else {
                continue;
            };
            let base = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            let proj = parse_thread_project(&content).unwrap_or_else(|| "—".into());
            let title = plan_title(&content, &base);
            rows.push(format!("- {base} · {proj} · {title}"));
        }
    }
    rows.sort();
    rows.truncate(cap);
    if rows.is_empty() {
        "—".into()
    } else {
        rows.join("\n")
    }
}

pub fn plan_inventory_block() -> String {
    plan_inventory_block_in(&plans_dir(), 80)
}

/// Format a plan's mtime: `HH:MM` if today, else `mm-dd HH:MM` (local time).
pub fn fmt_updated(mtime_ms: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(mtime_ms / 1000, 0).single() {
        Some(dt) => {
            if dt.date_naive() == chrono::Local::now().date_naive() {
                dt.format("%H:%M").to_string()
            } else {
                dt.format("%m-%d %H:%M").to_string()
            }
        }
        None => "—".to_string(),
    }
}

/// `iso` plus `mins` minutes, re-serialized in the same format. `None` on overflow (never panics).
pub fn add_minutes(iso: &str, mins: u32) -> Option<String> {
    let dt = chrono::DateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S%z").ok()?;
    let out = dt.checked_add_signed(chrono::Duration::minutes(mins as i64))?;
    Some(out.format("%Y-%m-%dT%H:%M:%S%z").to_string())
}

/// Minutes from now until `deadline_iso` (negative when overdue). `None` if unparseable/empty.
pub fn minutes_until(deadline_iso: &str) -> Option<i64> {
    if deadline_iso.is_empty() {
        return None;
    }
    let dl = chrono::DateTime::parse_from_str(deadline_iso, "%Y-%m-%dT%H:%M:%S%z").ok()?;
    Some((dl.with_timezone(&chrono::Local) - chrono::Local::now()).num_minutes())
}

/// Human "time left": `3h left`, `45m left`, or `-2h` / `-15m` when overdue. Returns (text, overdue).
pub fn left_label(mins: i64) -> (String, bool) {
    let overdue = mins < 0;
    let a = mins.abs();
    let body = if a >= 60 {
        format!("{}h", a / 60)
    } else {
        format!("{}m", a)
    };
    if overdue {
        (format!("-{body}"), true)
    } else {
        (format!("{body} left"), false)
    }
}

/// Parse a user-typed duration into minutes: `2h`, `90m`, `1h30m`, or a bare number (minutes).
pub fn parse_duration_min(s: &str) -> Option<u32> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<u32>() {
        return Some(n);
    }
    let mut total = 0u32;
    let mut num = String::new();
    let mut saw = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else if ch == 'h' {
            total += num.parse::<u32>().ok()? * 60;
            num.clear();
            saw = true;
        } else if ch == 'm' {
            total += num.parse::<u32>().ok()?;
            num.clear();
            saw = true;
        } else if ch == ' ' {
            continue;
        } else {
            return None;
        }
    }
    if !num.is_empty() {
        return None;
    }
    if saw {
        Some(total)
    } else {
        None
    }
}

/// Fill a goal's est/deadline from the project's time-gate plan, unless pinned. Never fabricates.
pub fn fill_estimate(st: &mut GoalState, root: &Path) {
    if st.time_pinned || st.set_at.is_empty() {
        return;
    }
    if let Some((p50, _p90)) = timegate_estimate(root) {
        st.est_minutes = Some(p50);
        st.deadline = add_minutes(&st.set_at, p50).unwrap_or_default();
    }
}

/// Estimate minutes rendered compactly: `8h`, `90m`, or `—` when absent.
pub fn est_label(est: Option<u32>) -> String {
    match est {
        Some(m) if m >= 60 && m % 60 == 0 => format!("{}h", m / 60),
        Some(m) if m >= 60 => format!("{}h{}m", m / 60, m % 60),
        Some(m) => format!("{m}m"),
        None => "—".to_string(),
    }
}

/// The latest non-empty `cwd` recorded across the lines of a transcript jsonl.
pub fn cwd_from_jsonl(content: &str) -> Option<PathBuf> {
    let mut cwd: Option<PathBuf> = None;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                if !c.is_empty() {
                    cwd = Some(PathBuf::from(c));
                }
            }
        }
    }
    cwd
}

/// Expand a leading `~`/`~/` to `$HOME`; otherwise verbatim.
pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        home().join(rest)
    } else if s == "~" {
        home()
    } else {
        PathBuf::from(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::now_stamp;
    use std::time::Duration;

    fn tmp_plans_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gw_plans_{}_{}", std::process::id(), tag));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_plan(dir: &Path, name: &str, project: &str, title: &str, gate: Option<(u32, u32)>) -> PathBuf {
        let mut s = format!("<!-- THREAD: {project} — t | feature -->\n\n# {title}\n\nbody\n");
        if let Some((p50, p90)) = gate {
            s.push_str(&format!("<!-- TIME-GATE: v=1 round=1 status=calibrated p50={p50} p90={p90} -->\n"));
        }
        let path = dir.join(name);
        fs::write(&path, s).unwrap();
        path
    }

    fn set_mtime(p: &Path, t: SystemTime) {
        let f = fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(t).unwrap();
    }

    #[test]
    fn plans_watermark_advances_on_in_place_edit_not_just_add_remove() {
        let dir = tmp_plans_dir("watermark");
        let a = write_plan(&dir, "a.md", "GOAL-WIDGET", "v1", Some((33, 200)));
        let dir_mtime = fs::metadata(&dir).unwrap().modified().unwrap();
        let wm0 = plans_watermark_in(&dir).expect("dir exists");
        let later = dir_mtime + Duration::from_secs(120);
        set_mtime(&a, later);
        let wm1 = plans_watermark_in(&dir).expect("dir exists");
        assert!(wm1 > wm0, "watermark must advance on in-place edit (wm0={wm0:?} wm1={wm1:?})");
        assert_eq!(wm1, later, "watermark tracks the newest plan file's mtime");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn project_match_key_prefers_explicit_then_folder() {
        let root = Path::new("/x/Some_Project");
        assert_eq!(project_match_key(root, "Laser Tag").as_deref(), Some("laser-tag"));
        assert_eq!(project_match_key(root, "").as_deref(), Some("some-project"));
    }

    #[test]
    fn plans_scope_key_explicit_key_wins_even_for_global_or_worktree() {
        let global = state_dir();
        assert_eq!(plans_scope_key(&global, ""), None);
        assert_eq!(plans_scope_key(&global, "CleoBot").as_deref(), Some("cleobot"));
        let wt = Path::new("/private/tmp/CleoBot-code-1a2b");
        assert_eq!(plans_scope_key(wt, "cleobot").as_deref(), Some("cleobot"));
        let proj = Path::new("/Users/x/Projects/goal-widget");
        assert_eq!(plans_scope_key(proj, "").as_deref(), Some("goal-widget"));
    }

    #[test]
    fn list_project_plans_filters_by_key_and_sorts_newest_first() {
        let dir = tmp_plans_dir("filter");
        let a = write_plan(&dir, "a.md", "GOAL-WIDGET", "older", Some((33, 200)));
        let b = write_plan(&dir, "b.md", "GOAL-WIDGET", "newer", Some((10, 40)));
        write_plan(&dir, "c.md", "OTHER-PROJECT", "excluded", Some((5, 9)));
        let now = SystemTime::now();
        set_mtime(&a, now - Duration::from_secs(600));
        set_mtime(&b, now);
        let plans = list_project_plans_in(&dir, "goal-widget");
        assert_eq!(plans.len(), 2, "OTHER-PROJECT excluded");
        assert_eq!(plans[0].title, "newer", "newest first");
        assert_eq!(plans[1].title, "older");
        assert_eq!(plans[0].p50, Some(10));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_project_plans_tolerates_missing_timegate_and_titles_fall_back() {
        let dir = tmp_plans_dir("notrailer");
        let p = dir.join("no-title.md");
        fs::write(&p, "<!-- THREAD: GOAL-WIDGET — t | feature -->\n\njust body, no H1\n").unwrap();
        let plans = list_project_plans_in(&dir, "goal-widget");
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].p50, None);
        assert_eq!(plans[0].title, "no-title.md");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plan_title_reads_first_h1() {
        assert_eq!(plan_title("# Hello world\n## sub\n", "x.md"), "Hello world");
        assert_eq!(plan_title("no heading here\n", "x.md"), "x.md");
    }

    #[test]
    fn empty_key_lists_nothing() {
        let dir = tmp_plans_dir("emptykey");
        write_plan(&dir, "a.md", "GOAL-WIDGET", "t", Some((1, 2)));
        assert!(list_project_plans_in(&dir, "").is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn candidate_projects_lists_dirs_plus_session_root() {
        let base = std::env::temp_dir().join(format!("gw_projroot_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let proj = base.join("Projects");
        fs::create_dir_all(proj.join("alpha")).unwrap();
        fs::create_dir_all(proj.join("beta")).unwrap();
        fs::write(proj.join("notadir.txt"), "x").unwrap();
        let extra = base.join("gamma");
        fs::create_dir_all(&extra).unwrap();
        let got = candidate_projects_in(&proj, Some(extra.clone()));
        assert!(got.contains(&proj.join("alpha")));
        assert!(got.contains(&proj.join("beta")));
        assert!(got.contains(&extra));
        assert!(!got.iter().any(|p| p.file_name().and_then(|n| n.to_str()) == Some("notadir.txt")));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn plan_inventory_lists_basename_project_title_and_caps() {
        let dir = tmp_plans_dir("inv");
        write_plan(&dir, "one.md", "GOAL-WIDGET", "Sort button", Some((1, 2)));
        write_plan(&dir, "two.md", "LASER-TAG", "Gate E", None);
        let block = plan_inventory_block_in(&dir, 80);
        assert!(block.contains("one.md · GOAL-WIDGET · Sort button"), "got: {block}");
        assert!(block.contains("two.md · LASER-TAG · Gate E"), "got: {block}");
        assert_eq!(plan_inventory_block_in(&dir, 1).lines().count(), 1, "cap respected");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cwd_from_jsonl_takes_latest_nonempty() {
        let content = "{\"cwd\":\"/first\",\"type\":\"user\"}\n\
                       {\"type\":\"assistant\"}\n\
                       {\"cwd\":\"\",\"type\":\"user\"}\n\
                       {\"cwd\":\"/second/proj\",\"type\":\"user\"}\n";
        assert_eq!(cwd_from_jsonl(content), Some(PathBuf::from("/second/proj")));
        assert_eq!(cwd_from_jsonl("{\"type\":\"user\"}\n"), None);
        assert_eq!(cwd_from_jsonl(""), None);
    }

    #[test]
    fn global_goal_path_names_file_by_id() {
        let p = global_goal_path("abc-123");
        assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("abc-123.goal.json"));
    }

    #[test]
    fn project_goal_file_is_dotfile_in_dir() {
        let p = project_goal_file(Path::new("/work/proj"));
        assert_eq!(p, PathBuf::from("/work/proj/.goal.json"));
    }

    #[test]
    fn fill_estimate_never_fabricates_and_respects_pin() {
        let root = std::env::temp_dir().join(format!("gw_noplan_{}", std::process::id()));
        let mut st = GoalState { set_at: now_stamp(), ..Default::default() };
        fill_estimate(&mut st, &root);
        assert_eq!(st.est_minutes, None);
        assert!(st.deadline.is_empty());
        let mut pinned = GoalState { set_at: now_stamp(), time_pinned: true, est_minutes: Some(5), ..Default::default() };
        fill_estimate(&mut pinned, &root);
        assert_eq!(pinned.est_minutes, Some(5));
    }
}
