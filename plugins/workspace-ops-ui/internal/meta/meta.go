// Package meta resolves what a plugin action is acting on: which pane, which
// tab, which harness, and everything the copy-metadata action reports.
//
// Attribution mirrors the harness-indicator / tidus-bar collectors, in the same
// order of evidence, because a weaker claim must never look like a stronger one:
//
//  1. HARNESS_NAME in a pane process's environment — stamped by `harness spawn`;
//  2. CLAUDE_CONFIG_DIR / CODEX_HOME / JCODE_HOME → the config dir the process
//     is really reading → its harness or profile;
//  3. `state/herdr-pane` matching the pane id — tidus's own record, last
//     because only `harness spawn` writes it and it goes stale.
//
// Linux-only by construction: steps 1 and 2 read /proc/<pid>/environ.
package meta

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/herdrdev/herdr/plugins/workspace-ops-ui/internal/fleet"
	"github.com/herdrdev/herdr/plugins/workspace-ops-ui/internal/herdr"
)

// configEnvVars are the per-runtime config-dir variables, in the order we trust
// them.
var configEnvVars = [...]string{"CODEX_HOME", "CLAUDE_CONFIG_DIR", "JCODE_HOME"}

// runtimeNames are process names that ARE an agent runtime.
var runtimeNames = map[string]bool{"claude": true, "codex": true, "pi": true, "jcode": true}

// Target is the pane an action was invoked against, resolved from the host's
// invocation environment.
type Target struct {
	PaneID      string
	TabID       string
	WorkspaceID string
	// TabLabel is what the user sees on the tab; it becomes the new
	// workspace's name on convert.
	TabLabel string
	// ExcludePaneID is a pane that must never be moved even though it sits in
	// the target tab. The overlay picker sets it to its OWN pane: the host
	// attaches an overlay to the focused tab, so a plan built from inside the
	// overlay would otherwise count the picker as cargo and relocate the popup
	// along with the work.
	ExcludePaneID string
}

// ResolveTarget reads the invocation context the host injects.
//
// The host sets HERDR_PANE_ID / HERDR_TAB_ID / HERDR_WORKSPACE_ID for a
// context-menu invocation (`invoke_plugin_action_for_pane` builds the context
// from the clicked pane). When a variable is missing — a keyboard-driven
// invocation, or a menu kind that carries less context — fall back to the
// focused pane, which is what the user was looking at.
func ResolveTarget(ctx context.Context, c *herdr.Client) (Target, error) {
	// Only the PANE comes from the environment. HERDR_TAB_ID and
	// HERDR_WORKSPACE_ID describe the tab and workspace that were SELECTED when
	// the menu opened, which is not the same thing as the parentage of the pane
	// that was clicked — right-click a pane in a workspace that is not the
	// current one and they disagree.
	//
	// Seeding them from the environment and only falling back to the pane made
	// every field except the pane id describe the wrong place: a pane in w1D
	// reported "workspace w1C plugfix" because w1C was on screen. Parentage is
	// therefore taken from the pane record and nowhere else; the environment
	// values are kept only for the case where the pane cannot be found at all.
	t := Target{PaneID: os.Getenv("HERDR_PANE_ID")}
	panes, err := c.Panes(ctx)
	if err != nil {
		return t, err
	}
	if t.PaneID == "" {
		for _, p := range panes {
			if p.Focused {
				t.PaneID = p.ID
				break
			}
		}
	}
	if t.PaneID == "" {
		return t, fmt.Errorf("no pane: neither HERDR_PANE_ID nor a focused pane")
	}
	if err := applyParentage(&t, panes); err != nil {
		return t, err
	}
	tabs, err := c.Tabs(ctx)
	if err != nil {
		return t, err
	}
	for _, tab := range tabs {
		if tab.ID == t.TabID {
			t.TabLabel = tab.Label
			break
		}
	}
	return t, nil
}

// Attribution is what we learned about the harness behind a pane.
type Attribution struct {
	Harness  fleet.Harness
	Found    bool
	Evidence string
	// AgentCWD is the runtime process's own working directory — the folder the
	// agent was launched in, which does not move when a shell cds.
	AgentCWD string
	// Runtime is the agent process name actually running (claude/codex/...),
	// which can disagree with the harness's declared runtime.
	Runtime string
	PID     int
}

// Attribute resolves the harness behind a pane, strongest evidence first.
func Attribute(ctx context.Context, c *herdr.Client, idx *fleet.Index, paneID string) Attribution {
	var out Attribution
	info, err := c.ProcessInfo(ctx, paneID)
	if err == nil {
		// Runtime processes first — their environment is the strongest claim —
		// then the pane's shell, which carries the env of a tab created with
		// `--env` even when the runtime is not in the foreground group.
		type proc struct {
			pid  int
			name string
			cwd  string
		}
		var procs []proc
		for _, p := range info.ForegroundProcesses {
			procs = append(procs, proc{p.PID, p.Name, p.CWD})
		}
		if info.ShellPID != 0 {
			found := false
			for _, p := range procs {
				if p.pid == info.ShellPID {
					found = true
					break
				}
			}
			if !found {
				procs = append(procs, proc{info.ShellPID, "shell", ""})
			}
		}
		// Stable partition: runtimes before everything else.
		ordered := make([]proc, 0, len(procs))
		for _, p := range procs {
			if runtimeNames[p.name] {
				ordered = append(ordered, p)
			}
		}
		for _, p := range procs {
			if !runtimeNames[p.name] {
				ordered = append(ordered, p)
			}
		}

		for _, p := range ordered {
			if runtimeNames[p.name] && out.Runtime == "" {
				out.Runtime = p.name
				out.PID = p.pid
				out.AgentCWD = procCWD(p.pid)
			}
			env := procEnv(p.pid)
			if name := env["HARNESS_NAME"]; name != "" && !out.Found {
				if h, ok := idx.ByName(name); ok {
					out.Harness, out.Found = h, true
					out.Evidence = fmt.Sprintf("HARNESS_NAME of pid %d", p.pid)
				}
			}
			if out.Found {
				continue
			}
			for _, key := range configEnvVars {
				dir := env[key]
				if dir == "" {
					continue
				}
				if h, ok := idx.ByConfigDir(dir); ok {
					out.Harness, out.Found = h, true
					out.Evidence = fmt.Sprintf("%s of pid %d", key, p.pid)
					break
				}
			}
		}
	}
	if !out.Found {
		if h, ok := idx.ByPaneID(paneID); ok {
			out.Harness, out.Found = h, true
			out.Evidence = "state/herdr-pane"
		}
	}
	return out
}

func procEnv(pid int) map[string]string {
	out := make(map[string]string, 32)
	raw, err := os.ReadFile(fmt.Sprintf("/proc/%d/environ", pid))
	if err != nil {
		return out
	}
	for _, entry := range strings.Split(string(raw), "\x00") {
		if entry == "" {
			continue
		}
		if key, value, ok := strings.Cut(entry, "="); ok {
			out[key] = value
		}
	}
	return out
}

func procCWD(pid int) string {
	target, err := os.Readlink(fmt.Sprintf("/proc/%d/cwd", pid))
	if err != nil {
		return ""
	}
	return target
}

// Tilde shortens a $HOME prefix, and only a real one: a sibling directory that
// merely starts with the same bytes must not be rewritten into someone else's
// home.
func Tilde(path string) string {
	home, err := os.UserHomeDir()
	if err != nil || home == "" || path == home {
		if path == home {
			return "~"
		}
		return path
	}
	if strings.HasPrefix(path, home+string(filepath.Separator)) {
		return "~" + path[len(home):]
	}
	return path
}

// applyParentage fills a target's tab and workspace from the PANE, which is the
// only authority on where that pane lives.
//
// Kept separate from the socket calls so the rule itself is testable: the bug it
// fixes is not visible in any state where the selection and the pane agree,
// which is every state a casual test constructs.
func applyParentage(t *Target, panes []herdr.Pane) error {
	for _, p := range panes {
		if p.ID == t.PaneID {
			t.TabID = p.TabID
			t.WorkspaceID = p.WorkspaceID
			return nil
		}
	}
	// The pane vanished between the click and now. Say so rather than silently
	// describing whatever happens to be on screen.
	t.TabID = os.Getenv("HERDR_TAB_ID")
	t.WorkspaceID = os.Getenv("HERDR_WORKSPACE_ID")
	return fmt.Errorf("pane %s is not in the pane list (closed since the menu opened?)", t.PaneID)
}
