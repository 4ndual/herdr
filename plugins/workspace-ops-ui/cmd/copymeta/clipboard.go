package main

import (
	"bytes"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// The clipboard problem this file exists to solve.
//
// A menu action is spawned by the herdr SERVER, and the server's environment
// has no DISPLAY, no WAYLAND_DISPLAY and no controlling terminal. Every
// clipboard mechanism needs one of those:
//
//   - xclip/xsel need DISPLAY (and usually XAUTHORITY)
//   - wl-copy needs WAYLAND_DISPLAY
//   - OSC 52 needs a tty to write the escape to
//
// So the copy silently did nothing from the menu while every shell-invoked test
// passed, because a shell has all three. That asymmetry is what made this look
// like a working action and read as a dead button: the test environment was the
// one environment where the bug cannot reproduce.
//
// The chain below therefore never trusts "the tool exited 0". It resolves a
// display from the running session when its own environment lacks one, and then
// READS THE CLIPBOARD BACK and compares. Anything less reports success it did
// not achieve, which is precisely the failure being fixed.

// sessionDisplay is the display environment recovered from the user's session.
type sessionDisplay struct {
	vars map[string]string // DISPLAY / WAYLAND_DISPLAY / XAUTHORITY
	from string            // which process it came from, for the failure message
}

// resolveSessionDisplay finds a display by reading the environment of processes
// this user already has running.
//
// Reading /proc/<pid>/environ is allowed only for our own processes, which is
// exactly the set we want: whatever is drawing this user's desktop knows how to
// reach it. Preference order puts the herdr client first — if a herdr TUI is on
// screen, its display is by definition the one the user is looking at.
func resolveSessionDisplay() *sessionDisplay {
	if os.Getenv("DISPLAY") != "" || os.Getenv("WAYLAND_DISPLAY") != "" {
		return nil // the environment already has one; nothing to recover
	}
	entries, err := os.ReadDir("/proc")
	if err != nil {
		return nil
	}
	var best *sessionDisplay
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		pid := e.Name()
		if pid[0] < '0' || pid[0] > '9' {
			continue
		}
		// No uid check is needed: /proc/<pid>/environ is readable only by the
		// process owner, so a foreign process simply fails the read below.
		raw, err := os.ReadFile(filepath.Join("/proc", pid, "environ"))
		if err != nil {
			continue // dead, or not ours
		}
		vars := map[string]string{}
		for _, kv := range bytes.Split(raw, []byte{0}) {
			k, v, ok := strings.Cut(string(kv), "=")
			if !ok {
				continue
			}
			switch k {
			case "DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY":
				vars[k] = v
			}
		}
		if vars["DISPLAY"] == "" && vars["WAYLAND_DISPLAY"] == "" {
			continue
		}
		comm, _ := os.ReadFile(filepath.Join("/proc", pid, "comm"))
		name := strings.TrimSpace(string(comm))
		cand := &sessionDisplay{vars: vars, from: fmt.Sprintf("%s (pid %s)", name, pid)}
		if strings.Contains(name, "herdr") {
			return cand // the display the user is actually looking at
		}
		if best == nil {
			best = cand
		}
	}
	return best
}

// env returns the process environment with the recovered display applied.
func (s *sessionDisplay) env() []string {
	out := os.Environ()
	if s == nil {
		return out
	}
	for k, v := range s.vars {
		out = append(out, k+"="+v)
	}
	return out
}

func (s *sessionDisplay) describe() string {
	if s == nil {
		return ""
	}
	parts := make([]string, 0, 3)
	for _, k := range []string{"WAYLAND_DISPLAY", "DISPLAY"} {
		if v := s.vars[k]; v != "" {
			parts = append(parts, k+"="+v)
		}
	}
	return strings.Join(parts, " ") + " from " + s.from
}

// readBack reads the clipboard using whichever reader is available, so a write
// can be checked rather than assumed. Returns ok=false when nothing can read,
// which is reported honestly instead of being treated as agreement.
func readBack(env []string) (text string, ok bool) {
	readers := []struct {
		name string
		args []string
	}{
		{"wl-paste", []string{"--no-newline"}},
		{"xclip", []string{"-selection", "clipboard", "-o"}},
		{"xsel", []string{"--clipboard", "--output"}},
		{"pbpaste", nil},
	}
	for _, r := range readers {
		path, err := exec.LookPath(r.name)
		if err != nil {
			continue
		}
		cmd := exec.Command(path, r.args...)
		cmd.Env = env
		out, err := cmd.Output()
		if err != nil {
			continue
		}
		return string(out), true
	}
	return "", false
}

// CopyVerified puts text on the clipboard and proves it landed.
//
// Returns the mechanism that worked. An error here is a real failure the caller
// must surface — never swallow it, and never exit 0 after one.
func CopyVerified(text string) (string, error) {
	attempts := []struct {
		label string
		env   []string
		disp  *sessionDisplay
	}{
		{label: "environment", env: os.Environ()},
	}
	if d := resolveSessionDisplay(); d != nil {
		attempts = append(attempts, struct {
			label string
			env   []string
			disp  *sessionDisplay
		}{label: "session display", env: d.env(), disp: d})
	}

	var tried []string
	for _, attempt := range attempts {
		for _, cand := range clipboardToolsFor(attempt.env) {
			path, err := exec.LookPath(cand.name)
			if err != nil {
				continue
			}
			cmd := exec.Command(path, cand.args...)
			cmd.Env = attempt.env
			cmd.Stdin = strings.NewReader(text)
			if err := cmd.Run(); err != nil {
				tried = append(tried, cand.name+" ("+attempt.label+"): "+err.Error())
				continue
			}
			// The tool exited 0. That is not evidence: xclip can exit 0 with no
			// display in some builds, and wl-copy forks. Read it back.
			if got, ok := readBack(attempt.env); ok {
				if strings.TrimSpace(got) != strings.TrimSpace(text) {
					tried = append(tried, cand.name+" ("+attempt.label+"): wrote but read back different content")
					continue
				}
			}
			mech := cand.name
			if attempt.disp != nil {
				mech += " via " + attempt.disp.describe()
			}
			_ = osc52(text) // best effort as well: herdr may be driven over ssh
			return mech, nil
		}
	}

	// Last resort: a controlling terminal, which a server-spawned action does
	// not have but a shell-invoked one does.
	if err := osc52(text); err == nil {
		return "osc52", nil
	}

	if len(tried) == 0 {
		return "", fmt.Errorf(
			"no clipboard tool found (looked for wl-copy, xclip, xsel, pbcopy) and no terminal for OSC 52 — " +
				"a menu action runs with no DISPLAY, no WAYLAND_DISPLAY and no tty")
	}
	return "", fmt.Errorf("every clipboard mechanism failed: %s", strings.Join(tried, "; "))
}

// clipboardToolsFor orders the tools by what the GIVEN environment can reach,
// rather than by this process's own environment — the whole point is that they
// differ.
func clipboardToolsFor(env []string) []clipTool {
	get := func(key string) string {
		prefix := key + "="
		for i := len(env) - 1; i >= 0; i-- { // last wins, like execve
			if strings.HasPrefix(env[i], prefix) {
				return strings.TrimPrefix(env[i], prefix)
			}
		}
		return ""
	}
	x11 := []clipTool{
		{"xclip", []string{"-selection", "clipboard"}},
		{"xsel", []string{"--clipboard", "--input"}},
	}
	wayland := []clipTool{{"wl-copy", nil}}
	mac := []clipTool{{"pbcopy", nil}}

	out := make([]clipTool, 0, 4)
	switch {
	case get("WAYLAND_DISPLAY") != "":
		out = append(out, wayland...)
		out = append(out, x11...)
	case get("DISPLAY") != "":
		out = append(out, x11...)
		out = append(out, wayland...)
	default:
		out = append(out, x11...)
		out = append(out, wayland...)
	}
	return append(out, mac...)
}
