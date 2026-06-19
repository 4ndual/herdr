//! Portable goal store + resolver + transcript discovery, ported VERBATIM from goal-widget's
//! `main.rs` (~lines 64–166, 256–303, 607–813, 1086–1144). `GoalStore` owns all goal I/O
//! (load/save/history/prompt-log), `GoalStore::resolve` implements the
//! link → adopt-by-cwd → migrate-legacy → project-root → global resolution chain, and the
//! transcript helpers (`session_jsonl`, `session_cwd`, `most_recent_session`) resolve a session's
//! files under `~/.claude/projects` and `~/.pi/agent/sessions`. The cmux focus/bridge layer is NOT
//! ported (herdr supplies focus via `FocusInput`).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::model::{GoalDoc, GoalState, Links, PromptEntry, Tree, GOAL_FILE};
use crate::timegate::{
    cwd_from_jsonl, global_goal_path, home, project_goal_file, project_root, state_dir,
};

// ----------------------------- legacy split-file readers -----------------------------

/// Per-session legacy state file: ~/.claude/goal/<sid>.json
fn state_path_for(sid: &str) -> PathBuf {
    state_dir().join(format!("{sid}.json"))
}
fn read_state_for(sid: &str) -> Option<GoalState> {
    let s = fs::read_to_string(state_path_for(sid)).ok()?;
    serde_json::from_str(&s).ok()
}
fn tree_path_for(sid: &str) -> PathBuf {
    state_dir().join(format!("{sid}.tree.json"))
}
fn read_tree(sid: &str) -> Tree {
    fs::read_to_string(tree_path_for(sid))
        .ok()
        .and_then(|s| serde_json::from_str::<Tree>(&s).ok())
        .map(|mut t| {
            if t.next_id == 0 {
                t.next_id = 1;
            }
            t
        })
        .unwrap_or_default()
}

pub fn read_goal_file(path: &Path) -> Option<GoalDoc> {
    serde_json::from_str::<GoalDoc>(&fs::read_to_string(path).ok()?).ok()
}

// ----------------------------- link index -----------------------------

fn links_path() -> PathBuf {
    state_dir().join("links.json")
}
pub fn read_links() -> Links {
    fs::read_to_string(links_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
fn write_links(l: &Links) {
    if fs::create_dir_all(state_dir()).is_err() {
        return;
    }
    if let Ok(s) = serde_json::to_string_pretty(l) {
        let tmp = state_dir().join("links.json.tmp");
        if fs::write(&tmp, s).is_ok() {
            let _ = fs::rename(&tmp, links_path());
        }
    }
}

// ----------------------------- GoalStore -----------------------------

/// Handle to one resolved goal: its stable id + the on-disk `.goal.json` path. Owns all goal I/O.
#[derive(Clone)]
pub struct GoalStore {
    pub id: String,
    pub path: PathBuf,
}
impl Default for GoalStore {
    fn default() -> Self {
        GoalStore { id: String::new(), path: global_goal_path("none") }
    }
}
impl GoalStore {
    /// The goal's project root = the goal file's parent dir.
    pub fn root(&self) -> PathBuf {
        self.path.parent().map(|p| p.to_path_buf()).unwrap_or_else(state_dir)
    }
    /// History dir is a sibling of the goal file: `<stem>.history/`.
    pub fn history_dir(&self) -> PathBuf {
        let name = self.path.file_name().and_then(|n| n.to_str()).unwrap_or(GOAL_FILE);
        let stem = name.strip_suffix(".json").unwrap_or(name);
        let parent = self.path.parent().map(|p| p.to_path_buf()).unwrap_or_else(state_dir);
        parent.join(format!("{stem}.history"))
    }
    pub fn history_file(&self, entity: &str) -> PathBuf {
        self.history_dir().join(format!("{entity}.jsonl"))
    }

    /// Prompt-log is a sibling of the goal file: `<stem>.prompts.jsonl`.
    pub fn prompt_log_path(&self) -> PathBuf {
        let name = self.path.file_name().and_then(|n| n.to_str()).unwrap_or(GOAL_FILE);
        let stem = name.strip_suffix(".json").unwrap_or(name);
        let parent = self.path.parent().map(|p| p.to_path_buf()).unwrap_or_else(state_dir);
        parent.join(format!("{stem}.prompts.jsonl"))
    }
    pub fn read_prompt_log(&self) -> Vec<PromptEntry> {
        let Ok(s) = fs::read_to_string(self.prompt_log_path()) else {
            return Vec::new();
        };
        s.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<PromptEntry>(l).ok())
            .collect()
    }
    pub fn append_prompt_log(&self, e: &PromptEntry) {
        if let Some(parent) = self.prompt_log_path().parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(line) = serde_json::to_string(e) {
            use std::io::Write;
            if let Ok(mut f) =
                fs::OpenOptions::new().create(true).append(true).open(self.prompt_log_path())
            {
                let _ = writeln!(f, "{line}");
            }
        }
    }

    pub fn load(&self) -> GoalDoc {
        let mut doc = read_goal_file(&self.path).unwrap_or_default();
        if doc.goal_id.is_empty() {
            doc.goal_id = self.id.clone();
        }
        if doc.next_id == 0 {
            doc.next_id = 1;
        }
        doc
    }
    pub fn save(&self, doc: &GoalDoc) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let serialized = serde_json::to_string_pretty(doc)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        fs::write(&tmp, serialized)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
    pub fn mtime(&self) -> Option<SystemTime> {
        fs::metadata(&self.path).ok()?.modified().ok()
    }

    /// One entity's change log, oldest first. Entity = "main" | "current" | "sub-<id>".
    pub fn read_history(&self, entity: &str) -> Vec<GoalState> {
        let Ok(s) = fs::read_to_string(self.history_file(entity)) else {
            return Vec::new();
        };
        s.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<GoalState>(l).ok())
            .collect()
    }
    /// Append a change, skipping no-op repeats (identical text+source+approved+state).
    pub fn append_history(&self, entity: &str, st: &GoalState) {
        if st.text.is_empty() {
            return;
        }
        if let Some(last) = self.read_history(entity).last() {
            if last.text == st.text
                && last.source == st.source
                && last.approved == st.approved
                && last.state == st.state
            {
                return;
            }
        }
        if fs::create_dir_all(self.history_dir()).is_err() {
            return;
        }
        if let Ok(line) = serde_json::to_string(st) {
            use std::io::Write;
            if let Ok(mut f) =
                fs::OpenOptions::new().create(true).append(true).open(self.history_file(entity))
            {
                let _ = writeln!(f, "{line}");
            }
        }
    }

    /// Resolve the goal for a session GLOBALLY: link → adopt-by-cwd → migrate-legacy → new.
    pub fn resolve(sid: &str) -> GoalStore {
        let mut links = read_links();
        // 1. existing link
        if let Some(gid) = links.sessions.get(sid).cloned() {
            let path =
                links.goals.get(&gid).map(PathBuf::from).unwrap_or_else(|| global_goal_path(&gid));
            if path.exists() {
                return GoalStore { id: gid, path };
            }
        }
        // 2. adopt a goal file in the session's exact cwd
        if let Some(cwd) = session_cwd(sid) {
            let pf = project_goal_file(&cwd);
            if let Some(doc) = read_goal_file(&pf) {
                let gid = if doc.goal_id.is_empty() { sid.to_string() } else { doc.goal_id };
                links.sessions.insert(sid.to_string(), gid.clone());
                links.goals.insert(gid.clone(), pf.to_string_lossy().into_owned());
                write_links(&links);
                return GoalStore { id: gid, path: pf };
            }
        }
        // 3. migrate legacy split files (<sid>.json / <sid>.tree.json), non-destructive
        if state_path_for(sid).exists() || tree_path_for(sid).exists() {
            let store = GoalStore { id: sid.to_string(), path: global_goal_path(sid) };
            if !store.path.exists() {
                migrate_legacy(sid, &store);
            }
            links.sessions.insert(sid.to_string(), sid.to_string());
            write_links(&links);
            return store;
        }
        // 4. new goal — prefer the PROJECT ROOT; fall back to the global store.
        if let Some(cwd) = session_cwd(sid) {
            if let Some(root) = project_root(&cwd) {
                let pf = project_goal_file(&root);
                if let Some(doc) = read_goal_file(&pf) {
                    let gid = if doc.goal_id.is_empty() { sid.to_string() } else { doc.goal_id };
                    links.sessions.insert(sid.to_string(), gid.clone());
                    links.goals.insert(gid.clone(), pf.to_string_lossy().into_owned());
                    write_links(&links);
                    return GoalStore { id: gid, path: pf };
                }
                let store = GoalStore { id: sid.to_string(), path: pf };
                links.sessions.insert(sid.to_string(), sid.to_string());
                links.goals.insert(sid.to_string(), store.path.to_string_lossy().into_owned());
                write_links(&links);
                return store;
            }
        }
        // 4b. fall back to a new global goal (cwd has no project root)
        let store = GoalStore { id: sid.to_string(), path: global_goal_path(sid) };
        links.sessions.insert(sid.to_string(), sid.to_string());
        write_links(&links);
        store
    }
}

/// Build the consolidated goal file (+ history dir) from legacy split files. Copies (never moves).
fn migrate_legacy(sid: &str, store: &GoalStore) {
    let main = read_state_for(sid).unwrap_or_default();
    let tree = read_tree(sid);
    let doc = GoalDoc {
        goal_id: sid.to_string(),
        schema: 1,
        main,
        current: tree.current,
        subs: tree.subs,
        next_id: tree.next_id,
        ..GoalDoc::default()
    };
    let _ = store.save(&doc);
    let hd = store.history_dir();
    let _ = fs::create_dir_all(&hd);
    copy_file(&state_dir().join(format!("{sid}.history.jsonl")), &hd.join("main.jsonl"));
    copy_file(&state_dir().join(format!("{sid}.current.history.jsonl")), &hd.join("current.jsonl"));
    if let Ok(rd) = fs::read_dir(state_dir()) {
        let prefix = format!("{sid}.sub-");
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(rest) = name.strip_prefix(&prefix) {
                if let Some(num) = rest.strip_suffix(".history.jsonl") {
                    copy_file(&e.path(), &hd.join(format!("sub-{num}.jsonl")));
                }
            }
        }
    }
}
fn copy_file(from: &Path, to: &Path) {
    if let Ok(c) = fs::read_to_string(from) {
        let _ = fs::write(to, c);
    }
}

/// Move a goal store's files into `dest_root` and repoint the global link. Returns the new store.
pub fn move_goal_store(old: &GoalStore, doc: &GoalDoc, dest_root: &Path) -> std::io::Result<GoalStore> {
    let dest = project_goal_file(dest_root);
    let new = GoalStore { id: old.id.clone(), path: dest.clone() };
    if dest == old.path {
        return Ok(new);
    }
    new.save(doc)?;
    if let Ok(entries) = fs::read_dir(old.history_dir()) {
        let _ = fs::create_dir_all(new.history_dir());
        for ent in entries.flatten() {
            let from = ent.path();
            if let Some(name) = from.file_name() {
                let to = new.history_dir().join(name);
                if fs::rename(&from, &to).is_err() {
                    copy_file(&from, &to);
                    let _ = fs::remove_file(&from);
                }
            }
        }
        let _ = fs::remove_dir(old.history_dir());
    }
    if old.prompt_log_path().exists() {
        let to = new.prompt_log_path();
        if fs::rename(old.prompt_log_path(), &to).is_err() {
            copy_file(&old.prompt_log_path(), &to);
            let _ = fs::remove_file(old.prompt_log_path());
        }
    }
    let _ = fs::remove_file(&old.path);
    let mut links = read_links();
    links.goals.insert(old.id.clone(), dest.to_string_lossy().into_owned());
    write_links(&links);
    Ok(new)
}

// ----------------------------- transcript discovery -----------------------------

fn projects_dir() -> PathBuf {
    home().join(".claude/projects")
}
fn pi_sessions_dir() -> PathBuf {
    home().join(".pi/agent/sessions")
}
fn transcript_roots() -> Vec<PathBuf> {
    vec![projects_dir(), pi_sessions_dir()]
}

fn for_each_jsonl_in(roots: &[PathBuf], mut f: impl FnMut(&PathBuf, &fs::DirEntry)) {
    let mut stack = roots.to_vec();
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(_) if path.extension().and_then(|e| e.to_str()) == Some("jsonl") => {
                    f(&path, &entry)
                }
                _ => {}
            }
        }
    }
}

/// Newest *.jsonl under known transcript roots — the user's most recently active session.
pub fn most_recent_session() -> Option<PathBuf> {
    most_recent_session_in(&transcript_roots())
}

pub fn most_recent_session_in(roots: &[PathBuf]) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for_each_jsonl_in(roots, |path, entry| {
        if let Ok(m) = entry.metadata().and_then(|md| md.modified()) {
            if best.as_ref().map_or(true, |(bt, _)| m > *bt) {
                best = Some((m, path.clone()));
            }
        }
    });
    best.map(|(_, p)| p)
}

/// The transcript file for a specific session id (<sid>.jsonl anywhere under transcript roots).
pub fn session_jsonl(sid: &str) -> Option<PathBuf> {
    session_jsonl_in(&transcript_roots(), sid)
}

pub fn session_jsonl_in(roots: &[PathBuf], sid: &str) -> Option<PathBuf> {
    let want = format!("{sid}.jsonl");
    let mut found = None;
    for_each_jsonl_in(roots, |path, _| {
        if found.is_none() && path.file_name().and_then(|n| n.to_str()) == Some(want.as_str()) {
            found = Some(path.clone());
        }
    });
    found
}

/// The session's working directory = the latest `cwd` recorded in its transcript jsonl.
pub fn session_cwd(sid: &str) -> Option<PathBuf> {
    cwd_from_jsonl(&fs::read_to_string(session_jsonl(sid)?).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::norm;
    use crate::model::Sub;
    use std::time::Duration;

    fn tmp_store(tag: &str) -> GoalStore {
        let dir = std::env::temp_dir().join(format!("gw_store_{}_{}", std::process::id(), tag));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        GoalStore { id: "abc-123".into(), path: dir.join(GOAL_FILE) }
    }

    fn set_mtime(p: &Path, t: SystemTime) {
        let f = fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(t).unwrap();
    }

    #[test]
    fn goal_doc_round_trip_through_store() {
        let store = tmp_store("rt");
        let doc = GoalDoc {
            goal_id: "abc-123".into(),
            schema: 1,
            main: GoalState {
                text: "ship it".into(),
                source: "user".into(),
                approved: true,
                updated_at: "t".into(),
                ..Default::default()
            },
            current: GoalState {
                text: "wire store".into(),
                source: "agent".into(),
                updated_at: "t".into(),
                ..Default::default()
            },
            subs: vec![Sub {
                id: 2,
                text: "tests".into(),
                source: "user".into(),
                approved: true,
                state: "active".into(),
                parent: "current".into(),
                created_at: "x".into(),
                updated_at: "y".into(),
                pinned: false,
            }],
            next_id: 3,
            ..GoalDoc::default()
        };
        store.save(&doc).unwrap();
        let back = store.load();
        assert_eq!(back.goal_id, "abc-123");
        assert_eq!(back.main.text, "ship it");
        assert!(back.main.approved);
        assert_eq!(back.current.text, "wire store");
        assert_eq!(back.subs.len(), 1);
        assert_eq!(back.subs[0].text, "tests");
        assert_eq!(back.next_id, 3);
        fs::remove_dir_all(store.path.parent().unwrap()).ok();
    }

    #[test]
    fn store_load_defaults_id_and_next_id() {
        let store = tmp_store("def");
        let doc = store.load();
        assert_eq!(doc.goal_id, "abc-123");
        assert_eq!(doc.next_id, 1);
        fs::remove_dir_all(store.path.parent().unwrap()).ok();
    }

    #[test]
    fn store_history_is_separate_and_dedups() {
        let store = tmp_store("hist");
        assert_eq!(store.history_dir(), store.path.parent().unwrap().join(".goal.history"));
        let a = GoalState {
            text: "v1".into(),
            source: "user".into(),
            approved: true,
            updated_at: "t1".into(),
            ..Default::default()
        };
        let b = GoalState {
            text: "v2".into(),
            source: "user".into(),
            approved: true,
            updated_at: "t2".into(),
            ..Default::default()
        };
        store.append_history("main", &a);
        store.append_history("main", &a);
        store.append_history("main", &b);
        let log = store.read_history("main");
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].text, "v1");
        assert_eq!(log[1].text, "v2");
        store.save(&store.load()).unwrap();
        let raw = fs::read_to_string(&store.path).unwrap();
        assert!(!raw.contains("v1"), "history must not be embedded in the goal file");
        fs::remove_dir_all(store.path.parent().unwrap()).ok();
    }

    #[test]
    fn project_key_round_trips_and_legacy_loads_clean() {
        let store = tmp_store("pk_rt");
        let mut doc = GoalDoc { goal_id: "g".into(), ..GoalDoc::default() };
        doc.project_key = "laser-tag".into();
        store.save(&doc).unwrap();
        let raw = fs::read_to_string(&store.path).unwrap();
        assert!(raw.contains("\"projectKey\""), "serialized when set");
        assert_eq!(store.load().project_key, "laser-tag");
        fs::write(&store.path, "{\"goalId\":\"g\",\"schema\":1}").unwrap();
        assert_eq!(store.load().project_key, "");
        fs::remove_dir_all(store.path.parent().unwrap()).ok();
    }

    #[test]
    fn constraints_round_trip_in_goal_file() {
        use crate::model::GoalConstraint;
        let store = tmp_store("cons_rt");
        let mut doc = GoalDoc { goal_id: "g".into(), ..GoalDoc::default() };
        doc.constraints.push(GoalConstraint { entity: "main".into(), text: "no new deps".into(), source: "user".into(), ts: "t".into() });
        doc.constraints.push(GoalConstraint { entity: "current".into(), text: "merge-commit only".into(), source: "user".into(), ts: "t".into() });
        store.save(&doc).unwrap();
        let back = store.load();
        assert_eq!(back.constraints.len(), 2);
        assert_eq!(back.constraints[0].entity, "main");
        assert_eq!(back.constraints[1].text, "merge-commit only");
        fs::remove_dir_all(store.path.parent().unwrap()).ok();
    }

    #[test]
    fn doc_carries_observer_fields_round_trip() {
        use crate::model::Proposal;
        let store = tmp_store("obs");
        let mut doc = GoalDoc { goal_id: "g".into(), ..GoalDoc::default() };
        doc.proposals.push(Proposal { kind: "main".into(), text: "new north star".into(), reason: "scope shift".into(), ts: "t".into() });
        doc.dismissed.push(norm("an old idea"));
        doc.paraphrase = "do the thing".into();
        doc.last_turn = 7;
        store.save(&doc).unwrap();
        let back = store.load();
        assert_eq!(back.proposals.len(), 1);
        assert_eq!(back.proposals[0].kind, "main");
        assert_eq!(back.dismissed, vec![norm("an old idea")]);
        assert_eq!(back.paraphrase, "do the thing");
        assert_eq!(back.last_turn, 7);
        fs::remove_dir_all(store.path.parent().unwrap()).ok();
    }

    #[test]
    fn prompt_log_round_trips_beside_goal_file() {
        let store = tmp_store("plog");
        assert!(store.prompt_log_path().to_string_lossy().ends_with(".goal.prompts.jsonl"));
        store.append_prompt_log(&PromptEntry { ts: "t1".into(), paraphrase: "first".into(), action: "noop".into(), reason: String::new() });
        store.append_prompt_log(&PromptEntry { ts: "t2".into(), paraphrase: "second".into(), action: "current".into(), reason: "shift".into() });
        let log = store.read_prompt_log();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].paraphrase, "first");
        assert_eq!(log[1].action, "current");
        fs::remove_dir_all(store.path.parent().unwrap()).ok();
    }

    #[test]
    fn session_jsonl_in_finds_pi_transcript_root() {
        let dir = std::env::temp_dir().join(format!("gw_transcripts_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let claude = dir.join("claude/projects/a");
        let pi = dir.join("pi/agent/sessions/b");
        fs::create_dir_all(&claude).unwrap();
        fs::create_dir_all(&pi).unwrap();
        let claude_sid = "019ed000-0000-7000-8000-000000000010";
        let pi_sid = "019ed000-0000-7000-8000-000000000011";
        fs::write(claude.join(format!("{claude_sid}.jsonl")), "{}\n").unwrap();
        let pi_path = pi.join(format!("{pi_sid}.jsonl"));
        fs::write(&pi_path, "{}\n").unwrap();
        let roots = vec![dir.join("claude/projects"), dir.join("pi/agent/sessions")];
        assert_eq!(session_jsonl_in(&roots, pi_sid).as_deref(), Some(pi_path.as_path()));
        assert_eq!(
            session_jsonl_in(&roots, claude_sid).as_deref(),
            Some(claude.join(format!("{claude_sid}.jsonl")).as_path())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn most_recent_session_in_prefers_newest_pi_transcript_across_roots() {
        let dir = std::env::temp_dir().join(format!("gw_recent_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let claude = dir.join("claude/projects/a");
        let pi = dir.join("pi/agent/sessions/b");
        fs::create_dir_all(&claude).unwrap();
        fs::create_dir_all(&pi).unwrap();
        let claude_path = claude.join("019ed000-0000-7000-8000-000000000020.jsonl");
        let pi_path = pi.join("019ed000-0000-7000-8000-000000000021.jsonl");
        fs::write(&claude_path, "{}\n").unwrap();
        fs::write(&pi_path, "{}\n").unwrap();
        let now = SystemTime::now();
        set_mtime(&claude_path, now - Duration::from_secs(60));
        set_mtime(&pi_path, now);
        let roots = vec![dir.join("claude/projects"), dir.join("pi/agent/sessions")];
        assert_eq!(most_recent_session_in(&roots).as_deref(), Some(pi_path.as_path()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_goal_store_relocates_docs_repoints_link_and_keeps_key() {
        static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("gw_home_{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home);

        let src_dir = home.join(".claude/goal");
        fs::create_dir_all(&src_dir).unwrap();
        let old = GoalStore { id: "gid-1".into(), path: src_dir.join("gid-1.goal.json") };
        let mut doc = GoalDoc { goal_id: "gid-1".into(), schema: 1, ..GoalDoc::default() };
        doc.main = GoalState { text: "ship sorter".into(), ..Default::default() };
        doc.project_key = "goal-widget".into();
        old.save(&doc).unwrap();
        old.append_prompt_log(&PromptEntry { ts: "t".into(), paraphrase: "p".into(), action: "keep".into(), reason: String::new() });
        old.append_history("main", &doc.main);
        assert!(old.path.exists() && old.prompt_log_path().exists() && old.history_file("main").exists());

        let dest = home.join("Projects/goal-widget");
        fs::create_dir_all(&dest).unwrap();
        let new = move_goal_store(&old, &doc, &dest).unwrap();

        assert_eq!(new.path, dest.join(GOAL_FILE));
        assert!(new.path.exists(), "new goal file written");
        assert!(new.prompt_log_path().exists(), "prompt-log moved");
        assert!(new.history_file("main").exists(), "history moved");
        assert!(!old.path.exists(), "old goal file removed (true move)");
        assert!(!old.prompt_log_path().exists(), "old prompt-log gone");
        let links = read_links();
        assert_eq!(links.goals.get("gid-1").map(String::as_str), Some(new.path.to_string_lossy().as_ref()));
        assert_eq!(new.load().project_key, "goal-widget", "projectKey survives the move");

        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let _ = fs::remove_dir_all(&home);
    }
}
