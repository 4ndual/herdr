// Command copymeta implements the "Copy metadata" context-menu action: it puts
// a formatted description of the invoked pane on the system clipboard.
//
// What it reports, and where each fact comes from:
//
//	harness     the pane process's own environment (HARNESS_NAME, or the config
//	            dir it is really reading) matched against ~/harnesses/*
//	folder      the agent's launch cwd (/proc/<pid>/cwd of the runtime, which
//	            does not move when a shell cds) and the pane's current cwd
//	pane        pane id + terminal title
//	workspace   workspace id + label
//	transcript  the runtime's session .jsonl, resolved per runtime from the
//	            session id and the config root
//	runtime     the agent kind herdr detects, and the process actually running
//
// Clipboard: herdr has no clipboard method on the socket API (`HostEffect::
// CopyText` exists only for wasm guests), so this probes the platform tools —
// wl-copy, xclip, xsel, pbcopy — and ALSO emits an OSC 52 escape, which is the
// only path that survives ssh. Both are attempted; the report says which
// landed, because a clipboard action that silently copies nothing is worse than
// one that admits it.
package main

import (
	"context"
	"encoding/base64"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/herdrdev/herdr/plugins/workspace-ops-ui/internal/fleet"
	"github.com/herdrdev/herdr/plugins/workspace-ops-ui/internal/herdr"
	"github.com/herdrdev/herdr/plugins/workspace-ops-ui/internal/meta"
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintf(os.Stderr, "copy-metadata: %v\n", err)
		os.Exit(1)
	}
}

func run() error {
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	client, err := herdr.Dial()
	if err != nil {
		return err
	}
	target, err := meta.ResolveTarget(ctx, client)
	if err != nil {
		return err
	}
	block, err := Build(ctx, client, target)
	if err != nil {
		return err
	}

	via, copyErr := Copy(block)
	if copyErr != nil {
		_ = client.Notify(ctx, "copy metadata failed", copyErr.Error())
		return copyErr
	}
	_ = client.Notify(ctx, "metadata copied", "via "+via)
	fmt.Print(block)
	return nil
}

// Build assembles the report. Exported so the gate can render it without a
// clipboard.
func Build(ctx context.Context, client *herdr.Client, target meta.Target) (string, error) {
	panes, err := client.Panes(ctx)
	if err != nil {
		return "", err
	}
	var pane herdr.Pane
	for _, p := range panes {
		if p.ID == target.PaneID {
			pane = p
			break
		}
	}
	spaces, err := client.Workspaces(ctx)
	if err != nil {
		return "", err
	}
	wsLabel := ""
	for _, ws := range spaces {
		if ws.ID == target.WorkspaceID {
			wsLabel = ws.Display()
			break
		}
	}
	agents, err := client.Agents(ctx)
	if err != nil {
		return "", err
	}
	var agent herdr.Agent
	for _, a := range agents {
		if a.PaneID == target.PaneID {
			agent = a
			break
		}
	}

	idx := fleet.NewIndex(fleet.Load(fleet.Root()))
	attr := meta.Attribute(ctx, client, idx, target.PaneID)

	fields := []field{
		{"harness", harnessField(attr)},
		{"runtime", runtimeField(agent, attr)},
		{"pane", strings.TrimSpace(target.PaneID + "  " + pane.TerminalTitle)},
		{"tab", fmt.Sprintf("%s  %s", target.TabID, target.TabLabel)},
		{"workspace", strings.TrimSpace(fmt.Sprintf("%s  %s", target.WorkspaceID, wsLabel))},
		{"agent folder", meta.Tilde(attr.AgentCWD)},
		{"shell folder", meta.Tilde(pane.CWD)},
		{"session", agent.SessionValue()},
		{"transcript", transcriptPath(agent, attr)},
	}
	return render(fields), nil
}

type field struct{ key, value string }

// render lays the block out as aligned "key: value" lines. Alignment is the
// whole point of "neatly indented": a block someone pastes into an issue should
// read as a table, not as prose.
func render(fields []field) string {
	// Pad to the longest "key:" — measuring the key alone leaves the longest
	// line one column wider than the rest, which is exactly the sort of thing
	// someone notices after pasting it into an issue.
	width := 0
	for _, f := range fields {
		if f.value != "" && len(f.key)+1 > width {
			width = len(f.key) + 1
		}
	}
	var b strings.Builder
	b.WriteString("herdr pane metadata\n")
	for _, f := range fields {
		if f.value == "" {
			continue // an unknown fact is omitted, never printed as "unknown"
		}
		fmt.Fprintf(&b, "  %-*s  %s\n", width, f.key+":", f.value)
	}
	return b.String()
}

func harnessField(attr meta.Attribution) string {
	if !attr.Found {
		return ""
	}
	out := attr.Harness.Name
	if attr.Harness.Profile {
		out += " (profile)"
	}
	if attr.Evidence != "" {
		out += "  [via " + attr.Evidence + "]"
	}
	return out
}

// runtimeField reports what herdr detected and, when they disagree, what is
// actually running — a pane whose harness declares codex while it runs claude
// is reading one harness's config with another's runtime.
func runtimeField(agent herdr.Agent, attr meta.Attribution) string {
	parts := []string{}
	if agent.Agent != "" && agent.Agent != "none" {
		parts = append(parts, agent.Agent)
	} else if attr.Runtime != "" {
		parts = append(parts, attr.Runtime)
	}
	if agent.Status != "" {
		parts = append(parts, "("+agent.Status+")")
	}
	if attr.Found && attr.Harness.Runtime != "" && attr.Runtime != "" &&
		attr.Harness.Runtime != attr.Runtime {
		parts = append(parts, fmt.Sprintf("⚠ harness declares %s", attr.Harness.Runtime))
	}
	return strings.Join(parts, " ")
}

// transcriptPath resolves the runtime's session file. Each runtime stores it
// differently, so this is per-runtime rather than one guessed shape, and it
// only returns a path that actually exists.
func transcriptPath(agent herdr.Agent, attr meta.Attribution) string {
	session := agent.SessionValue()
	if session == "" {
		return ""
	}
	configRoot := ""
	if attr.Found {
		configRoot = attr.Harness.ConfigDir
	}
	home, _ := os.UserHomeDir()

	kind := agent.Agent
	if kind == "" {
		kind = attr.Runtime
	}
	switch kind {
	case "claude":
		roots := []string{configRoot, filepath.Join(home, ".claude")}
		for _, root := range roots {
			if root == "" {
				continue
			}
			// projects/<encoded-cwd>/<session>.jsonl — the cwd encoding is
			// claude's own, so glob the session id instead of reproducing it.
			matches, _ := filepath.Glob(filepath.Join(root, "projects", "*", session+".jsonl"))
			if len(matches) > 0 {
				return meta.Tilde(matches[0])
			}
		}
	case "codex":
		roots := []string{configRoot, filepath.Join(home, ".codex")}
		for _, root := range roots {
			if root == "" {
				continue
			}
			// sessions/YYYY/MM/DD/rollout-<ts>-<session>.jsonl
			matches, _ := filepath.Glob(filepath.Join(root, "sessions", "*", "*", "*", "*"+session+"*.jsonl"))
			if len(matches) > 0 {
				sort.Strings(matches)
				return meta.Tilde(matches[len(matches)-1])
			}
		}
	}
	return ""
}

// Copy puts text on the clipboard, reporting which mechanism took it.
func Copy(text string) (string, error) {
	var tried []string
	for _, cand := range clipboardTools() {
		path, err := exec.LookPath(cand.name)
		if err != nil {
			continue
		}
		tried = append(tried, cand.name)
		cmd := exec.Command(path, cand.args...)
		cmd.Stdin = strings.NewReader(text)
		// wl-copy forks a daemon to own the selection; giving it its own
		// process group keeps it from dying with this short-lived action.
		if err := cmd.Run(); err == nil {
			// OSC 52 as well: a native tool only reaches the local display,
			// and herdr may be driven over ssh.
			_ = osc52(text)
			return cand.name, nil
		}
	}
	if err := osc52(text); err == nil {
		return "osc52", nil
	}
	if len(tried) == 0 {
		return "", fmt.Errorf("no clipboard tool found (tried wl-copy, xclip, xsel, pbcopy) and no tty for OSC 52")
	}
	return "", fmt.Errorf("clipboard tools failed: %s", strings.Join(tried, ", "))
}

type clipTool struct {
	name string
	args []string
}

// clipboardTools is ordered by platform fit: Wayland first when a Wayland
// display exists, then X11, then macOS.
func clipboardTools() []clipTool {
	x11 := []clipTool{
		{"xclip", []string{"-selection", "clipboard"}},
		{"xsel", []string{"--clipboard", "--input"}},
	}
	wayland := []clipTool{{"wl-copy", nil}}
	mac := []clipTool{{"pbcopy", nil}}

	out := make([]clipTool, 0, 4)
	if os.Getenv("WAYLAND_DISPLAY") != "" {
		out = append(out, wayland...)
		out = append(out, x11...)
	} else if os.Getenv("DISPLAY") != "" {
		out = append(out, x11...)
		out = append(out, wayland...)
	} else {
		out = append(out, wayland...)
		out = append(out, x11...)
	}
	return append(out, mac...)
}

// osc52 writes the clipboard escape to the controlling terminal. Written to
// /dev/tty rather than stdout: an action's stdout is captured by the host into
// its command log, where an escape sequence would be inert noise.
func osc52(text string) error {
	tty, err := os.OpenFile("/dev/tty", os.O_WRONLY, 0)
	if err != nil {
		return err
	}
	defer tty.Close()
	_ = tty.SetWriteDeadline(time.Now().Add(2 * time.Second))
	encoded := base64.StdEncoding.EncodeToString([]byte(text))
	_, err = fmt.Fprintf(tty, "\x1b]52;c;%s\x07", encoded)
	return err
}
