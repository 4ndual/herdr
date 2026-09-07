//! Per-pane memory pressure policy.
//!
//! Every pane command is wrapped in its own transient systemd scope
//! (`systemd-run --user --scope`) carrying a soft `MemoryHigh` ceiling, and
//! JS runtimes get heap-sizing env so they garbage-collect as if the machine
//! were smaller. `--scope` execs the command in place: the pane keeps its pty,
//! cwd, env and pid exactly as before; only its cgroup changes.
//!
//! Panes that stay idle (no pty output, negligible CPU) for a configured
//! period get their resident memory proactively pushed to swap/zram through
//! cgroup v2 `memory.reclaim`, so cold agent heaps leave RAM without waiting
//! for global pressure.
//!
//! The policy is process-global (like `kitty_graphics`) because it is set from
//! config once at startup / reload and consumed at the single pty spawn site.
//! It is probed once against the live systemd instance: if `systemd-run` is
//! missing, the user manager is unreachable, or the memory controller is not
//! delegated, the scope wrapper is disabled and panes spawn exactly as before.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use portable_pty::CommandBuilder;

use crate::config::{parse_memory_size, PaneMemoryConfig};

const SCOPE_UNIT_PREFIX: &str = "herdr-pane-";
const IDLE_RECLAIM_TICK: Duration = Duration::from_secs(60);
/// CPU time per tick above which a pane counts as busy even with no output.
const IDLE_RECLAIM_CPU_BUDGET: Duration = Duration::from_millis(500);

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Policy {
    /// `MemoryHigh=` value for the pane scope, already validated. `None` = no scope.
    pane_high: Option<String>,
    /// `BUN_JSC_forceRAMSize` in bytes.
    bun_force_ram_size: Option<u64>,
    /// `--max-old-space-size` in MiB.
    node_max_old_space_mib: Option<u64>,
    /// Idle time before a pane scope is reclaimed. `None` = never.
    idle_reclaim_after: Option<Duration>,
}

static POLICY: RwLock<Policy> = RwLock::new(Policy {
    pane_high: None,
    bun_force_ram_size: None,
    node_max_old_space_mib: None,
    idle_reclaim_after: None,
});
static SCOPE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Install the policy from config. Invalid sizes are reported as diagnostics
/// and that knob is left off; the scope wrapper is probed live and disabled
/// (with one log line) when the host cannot honor it.
pub(crate) fn configure(config: &PaneMemoryConfig) -> Vec<String> {
    let mut diagnostics = Vec::new();
    let mut size = |key: &str, text: &str| match parse_memory_size(text) {
        Ok(size) => size,
        Err(err) => {
            diagnostics.push(format!("terminal.memory.{key}: {err}"));
            None
        }
    };
    let pane_high = size("pane_high", &config.pane_high);
    let bun_force_ram_size = size("bun_force_ram_size", &config.bun_force_ram_size);
    let node_max_old_space_mib = size("node_max_old_space_size", &config.node_max_old_space_size)
        .map(|bytes| (bytes >> 20).max(1));

    let pane_high = pane_high.map(|_| config.pane_high.trim().to_string());
    let pane_high = pane_high.filter(|high| {
        let previous = POLICY.read().map(|p| p.pane_high.clone()).unwrap_or_default();
        previous.as_deref() == Some(high.as_str()) || scope_wrapper_available(high)
    });
    let idle_reclaim_after = (config.idle_reclaim_minutes > 0 && pane_high.is_some())
        .then(|| Duration::from_secs(config.idle_reclaim_minutes * 60));

    let policy = Policy {
        pane_high,
        bun_force_ram_size,
        node_max_old_space_mib,
        idle_reclaim_after,
    };
    if let Ok(mut current) = POLICY.write() {
        if *current != policy {
            tracing::info!(
                event = "pane.memory_policy",
                subsystem = "pane",
                pane_high = policy.pane_high.as_deref().unwrap_or("off"),
                bun_force_ram_size = policy.bun_force_ram_size.unwrap_or(0),
                node_max_old_space_mib = policy.node_max_old_space_mib.unwrap_or(0),
                idle_reclaim_minutes = policy
                    .idle_reclaim_after
                    .map_or(0, |after| after.as_secs() / 60),
                "pane memory policy applied"
            );
        }
        *current = policy;
    }
    diagnostics
}

/// Apply the policy to a pane command right before it is spawned: inject the
/// runtime heap env and, when available, prefix argv with the scope wrapper.
pub(crate) fn apply(cmd: &mut CommandBuilder) {
    let Ok(policy) = POLICY.read() else {
        return;
    };
    if let Some(bytes) = policy.bun_force_ram_size {
        cmd.env("BUN_JSC_forceRAMSize", bytes.to_string());
    }
    if let Some(mib) = policy.node_max_old_space_mib {
        let flag = format!("--max-old-space-size={mib}");
        let value = match std::env::var("NODE_OPTIONS") {
            Ok(existing) if !existing.trim().is_empty() => format!("{existing} {flag}"),
            _ => flag,
        };
        cmd.env("NODE_OPTIONS", value);
    }
    #[cfg(target_os = "linux")]
    if let Some(high) = policy.pane_high.as_deref() {
        wrap_in_scope(cmd, high);
    }
}

/// Watch a spawned pane and reclaim its scope's memory once it has been idle
/// for the configured period. Idle means the pty produced no output and the
/// scope used negligible CPU across the whole period. Reclaims once per idle
/// stretch; a pane that wakes up and goes quiet again is reclaimed again.
/// No-op unless the pane was spawned inside a herdr-owned scope.
pub(crate) fn spawn_idle_reclaim(
    pane_id: u32,
    child_pid: Arc<std::sync::atomic::AtomicU32>,
    child_exited: Arc<AtomicBool>,
    content_seq: Arc<AtomicU64>,
) {
    let Some(idle_after) = POLICY.read().ok().and_then(|p| p.idle_reclaim_after) else {
        return;
    };
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(IDLE_RECLAIM_TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        let mut cgroup: Option<std::path::PathBuf> = None;
        let mut last_seq = content_seq.load(Ordering::Acquire);
        let mut last_cpu_usec = 0;
        let mut idle_ticks = 0u32;
        let mut reclaimed_seq = None;
        let idle_ticks_needed = idle_after.as_secs().div_ceil(IDLE_RECLAIM_TICK.as_secs()) as u32;
        loop {
            interval.tick().await;
            if child_exited.load(Ordering::Acquire) {
                return;
            }
            let dir = match &cgroup {
                Some(dir) => dir,
                None => match pane_scope_cgroup(child_pid.load(Ordering::Acquire)) {
                    Some(dir) => cgroup.insert(dir),
                    None => return,
                },
            };
            let seq = content_seq.load(Ordering::Acquire);
            let cpu_usec = scope_cpu_usec(dir).unwrap_or(last_cpu_usec);
            let busy = seq != last_seq
                || cpu_usec.saturating_sub(last_cpu_usec)
                    > IDLE_RECLAIM_CPU_BUDGET.as_micros() as u64;
            last_seq = seq;
            last_cpu_usec = cpu_usec;
            idle_ticks = if busy { 0 } else { idle_ticks + 1 };
            if idle_ticks < idle_ticks_needed || reclaimed_seq == Some(seq) {
                continue;
            }
            reclaimed_seq = Some(seq);
            match reclaim_scope(dir) {
                Ok((before, after)) => crate::logging::pane_memory_reclaimed(pane_id, before, after),
                Err(err) => {
                    tracing::warn!(
                        event = "pane.memory_reclaim_failed",
                        subsystem = "pane",
                        pane_id,
                        err = %err,
                        "memory.reclaim write failed"
                    );
                    return;
                }
            }
        }
    });
}

#[cfg(target_os = "linux")]
fn scope_args(high: &str) -> [String; 9] {
    let seq = SCOPE_SEQ.fetch_add(1, Ordering::Relaxed);
    [
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        // Scopes whose process died by signal otherwise linger as "failed" units.
        "--collect".into(),
        format!("--unit={SCOPE_UNIT_PREFIX}{}-{seq}", std::process::id()),
        "--description=herdr pane".into(),
        format!("--property=MemoryHigh={high}"),
        "--property=MemorySwapMax=infinity".into(),
        "--".into(),
    ]
}

#[cfg(target_os = "linux")]
fn wrap_in_scope(cmd: &mut CommandBuilder, high: &str) {
    let argv = cmd.get_argv_mut();
    // `new_default_prog()` has empty argv and lets portable-pty pick the login
    // shell at exec time; resolve it here since systemd-run needs a program.
    let target: Vec<std::ffi::OsString> = if argv.is_empty() {
        vec![cmd.get_shell().into(), "-l".into()]
    } else {
        std::mem::take(argv)
    };
    let argv = cmd.get_argv_mut();
    argv.push("systemd-run".into());
    argv.extend(scope_args(high).into_iter().map(Into::into));
    argv.extend(target);
}

#[cfg(target_os = "linux")]
fn scope_wrapper_available(high: &str) -> bool {
    let output = std::process::Command::new("systemd-run")
        .args(scope_args(high))
        .arg("true")
        .stdin(std::process::Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            tracing::warn!(
                event = "pane.memory_scope_unavailable",
                subsystem = "pane",
                outcome = "fallback",
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "systemd-run cannot create a pane scope with MemoryHigh; panes spawn without a memory ceiling"
            );
            false
        }
        Err(err) => {
            tracing::info!(
                event = "pane.memory_scope_unavailable",
                subsystem = "pane",
                outcome = "fallback",
                err = %err,
                "systemd-run is not available; panes spawn without a memory ceiling"
            );
            false
        }
    }
}

/// The cgroup v2 directory of the pane's scope, only if herdr created it.
#[cfg(target_os = "linux")]
fn pane_scope_cgroup(pid: u32) -> Option<std::path::PathBuf> {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim();
    let unit = path.rsplit('/').next()?;
    unit.starts_with(SCOPE_UNIT_PREFIX)
        .then(|| std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

#[cfg(target_os = "linux")]
fn scope_cpu_usec(dir: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(dir.join("cpu.stat"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("usage_usec "))?
        .trim()
        .parse()
        .ok()
}

/// Push the scope's resident memory to swap. Returns `memory.current`
/// before and after. The kernel answers EAGAIN when it could not reclaim the
/// full amount (it still reclaimed what it could), which is not an error here.
#[cfg(target_os = "linux")]
fn reclaim_scope(dir: &std::path::Path) -> std::io::Result<(u64, u64)> {
    let read_current = || -> std::io::Result<u64> {
        std::fs::read_to_string(dir.join("memory.current"))?
            .trim()
            .parse()
            .map_err(|_| std::io::Error::other("unparsable memory.current"))
    };
    let before = read_current()?;
    if before == 0 {
        return Ok((0, 0));
    }
    let reclaim = dir.join("memory.reclaim");
    // `swappiness=` (6.11+) prefers anon over file pages, which is exactly the
    // JS heap we want out of RAM; older kernels reject the argument.
    let mut result = std::fs::write(&reclaim, format!("{before} swappiness=200"));
    if matches!(&result, Err(err) if err.kind() == std::io::ErrorKind::InvalidInput) {
        result = std::fs::write(&reclaim, before.to_string());
    }
    match result {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(err) => return Err(err),
    }
    Ok((before, read_current()?))
}

#[cfg(not(target_os = "linux"))]
fn scope_wrapper_available(_high: &str) -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
fn pane_scope_cgroup(_pid: u32) -> Option<std::path::PathBuf> {
    None
}

#[cfg(not(target_os = "linux"))]
fn scope_cpu_usec(_dir: &std::path::Path) -> Option<u64> {
    None
}

#[cfg(not(target_os = "linux"))]
fn reclaim_scope(_dir: &std::path::Path) -> std::io::Result<(u64, u64)> {
    Ok((0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn policy() -> Policy {
        POLICY.read().expect("policy lock").clone()
    }

    #[test]
    fn disabled_config_clears_policy_and_leaves_command_untouched() {
        let _guard = test_lock();
        let diagnostics = configure(&PaneMemoryConfig::disabled());
        assert!(diagnostics.is_empty());
        assert_eq!(policy(), Policy::default());
        let mut cmd = CommandBuilder::new("/bin/cat");
        apply(&mut cmd);
        assert_eq!(cmd.get_argv(), &["/bin/cat"]);
        assert!(cmd.get_env("BUN_JSC_forceRAMSize").is_none());
    }

    #[test]
    fn invalid_sizes_are_reported_and_ignored() {
        let _guard = test_lock();
        let diagnostics = configure(&PaneMemoryConfig {
            pane_high: String::new(),
            bun_force_ram_size: "lots".into(),
            node_max_old_space_size: "0".into(),
            idle_reclaim_minutes: 10,
        });
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert!(diagnostics[0].starts_with("terminal.memory.bun_force_ram_size"));
        assert!(diagnostics[1].starts_with("terminal.memory.node_max_old_space_size"));
        // Idle reclaim needs a scope; without pane_high it stays off.
        assert_eq!(policy(), Policy::default());
        configure(&PaneMemoryConfig::disabled());
    }

    #[test]
    fn parse_memory_size_handles_suffixes() {
        assert_eq!(parse_memory_size("768M"), Ok(Some(768 << 20)));
        assert_eq!(parse_memory_size(" 1g "), Ok(Some(1 << 30)));
        assert_eq!(parse_memory_size("4096"), Ok(Some(4096)));
        assert_eq!(parse_memory_size(""), Ok(None));
        assert!(parse_memory_size("1.5G").is_err());
        assert!(parse_memory_size("M").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scope_wrapper_prefixes_argv_and_resolves_default_prog() {
        let mut cmd = CommandBuilder::new("/bin/cat");
        cmd.arg("-n");
        wrap_in_scope(&mut cmd, "768M");
        let argv: Vec<&str> = cmd.get_argv().iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(argv[0], "systemd-run");
        assert!(argv.contains(&"--property=MemoryHigh=768M"));
        assert!(argv.iter().any(|a| a.starts_with("--unit=herdr-pane-")));
        assert_eq!(&argv[argv.len() - 3..], ["--", "/bin/cat", "-n"]);

        let mut cmd = CommandBuilder::new_default_prog();
        cmd.env("SHELL", "/bin/sh");
        wrap_in_scope(&mut cmd, "768M");
        let argv: Vec<&str> = cmd.get_argv().iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(&argv[argv.len() - 3..], ["--", "/bin/sh", "-l"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn foreign_cgroups_are_never_reclaimed() {
        assert_eq!(pane_scope_cgroup(std::process::id()), None);
    }
}
