// Package fleet reads tidus/harness-fleet parentage straight off disk.
//
// The edges are the ones DONE-K established and gated (task #43): a child hangs
// off a parent by a live dispatch, by a declared team, or by the parent of its
// most recent finished job. Nothing here writes — `dispatch.json` is
// O_EXCL-claimed by the fleet and the tidus brief is explicit that a UI must
// never write it.
//
// Everything degrades to empty when the fleet is absent: with no
// `$HARNESS_ROOT` (or no harnesses in it) every lookup returns no children, so
// a herdr install with zero tidus behaves exactly like one with a fleet that
// simply has no children — which is what "MUST work with zero tidus installed"
// means in practice.
package fleet

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// cliParent is the sentinel route.go writes when a dispatch came from the CLI
// rather than another harness. It is not a harness.
const cliParent = "(cli)"

// maxAncestry bounds the upward walk. Deep trees are normal fleet operation, so
// this is a cycle backstop rather than a policy.
const maxAncestry = 8

// Harness is one fleet member as it exists on disk.
type Harness struct {
	Name    string
	Runtime string
	Team    string
	Profile bool
	// ConfigDir is where the runtime reads its config, canonicalised. It is the
	// key that maps a running process back to its harness.
	ConfigDir string
	// DispatchParent is set while a routed task is in flight.
	DispatchParent string
	// HistoryParent is the parent of the most recent finished job.
	HistoryParent string
	TabID         string
	PaneID        string
	SessionID     string
}

// Parent is the edge used to walk UP the tree: a live dispatch is the truest
// "who is running me", team is the durable fallback, history is last.
func (h Harness) Parent() string {
	switch {
	case h.DispatchParent != "":
		return h.DispatchParent
	case h.Team != "":
		return h.Team
	default:
		return h.HistoryParent
	}
}

// ChildOf reports whether h hangs off parent by any edge.
func (h Harness) ChildOf(parent string) bool {
	return parent != "" && (h.DispatchParent == parent || h.Team == parent || h.HistoryParent == parent)
}

// Root is $HARNESS_ROOT, or ~/harnesses.
func Root() string {
	if root := os.Getenv("HARNESS_ROOT"); root != "" {
		return root
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return filepath.Join(home, "harnesses")
}

// Load reads every harness under root. A missing or unreadable root is not an
// error — it is a herdr without tidus, which is a supported configuration.
func Load(root string) []Harness {
	if root == "" {
		return nil
	}
	entries, err := os.ReadDir(root)
	if err != nil {
		return nil
	}
	out := make([]Harness, 0, len(entries))
	for _, entry := range entries {
		name := entry.Name()
		if !entry.IsDir() || strings.HasPrefix(name, ".") {
			continue
		}
		dir := filepath.Join(root, name)
		raw, err := os.ReadFile(filepath.Join(dir, "harness.json"))
		if err != nil {
			continue // a directory without harness.json is not a harness
		}
		var meta struct {
			Runtime   string `json:"runtime"`
			Team      string `json:"team"`
			Profile   bool   `json:"profile"`
			ConfigDir string `json:"configDir"`
		}
		if json.Unmarshal(raw, &meta) != nil {
			continue
		}
		configDir := meta.ConfigDir
		if configDir == "" {
			configDir = filepath.Join(dir, "config")
		}
		configDir = expandTilde(configDir)
		if resolved, err := filepath.EvalSymlinks(configDir); err == nil {
			configDir = resolved
		}
		state := filepath.Join(dir, "state")
		h := Harness{
			Name:      name,
			Runtime:   orDefault(meta.Runtime, "claude"),
			Team:      meta.Team,
			Profile:   meta.Profile,
			ConfigDir: configDir,
			TabID:     readTrim(filepath.Join(state, "herdr-tab")),
			PaneID:    readTrim(filepath.Join(state, "herdr-pane")),
			SessionID: readTrim(filepath.Join(state, "session-id")),
		}
		h.DispatchParent = dispatchParent(state)
		h.HistoryParent = historyParent(state)
		out = append(out, h)
	}
	return out
}

func dispatchParent(state string) string {
	raw, err := os.ReadFile(filepath.Join(state, "dispatch.json"))
	if err != nil {
		return ""
	}
	var rec struct {
		ID     string `json:"id"`
		Parent string `json:"parent"`
	}
	// claimDispatch creates the file O_EXCL and only then writes it; a record
	// with no id is a half-written file, not a job.
	if json.Unmarshal(raw, &rec) != nil || rec.ID == "" || rec.Parent == cliParent {
		return ""
	}
	return rec.Parent
}

func historyParent(state string) string {
	raw, err := os.ReadFile(filepath.Join(state, "last-result.json"))
	if err != nil {
		return ""
	}
	var ev struct {
		Parent string `json:"parent"`
	}
	if json.Unmarshal(raw, &ev) != nil || ev.Parent == cliParent {
		return ""
	}
	return ev.Parent
}

// Index is a name-keyed view of a loaded fleet.
type Index struct {
	byName map[string]Harness
	all    []Harness
}

// NewIndex builds the lookup once, so callers do not rescan per question.
func NewIndex(all []Harness) *Index {
	byName := make(map[string]Harness, len(all))
	for _, h := range all {
		byName[h.Name] = h
	}
	return &Index{byName: byName, all: all}
}

// Empty reports a herdr with no fleet at all.
func (i *Index) Empty() bool { return i == nil || len(i.all) == 0 }

// ByName returns a harness and whether it exists.
func (i *Index) ByName(name string) (Harness, bool) {
	if i == nil {
		return Harness{}, false
	}
	h, ok := i.byName[name]
	return h, ok
}

// ByConfigDir attributes a config dir (read from a running process) to its
// harness. Profiles are included: the human's own session is usually a profile
// and is usually the orchestrator.
func (i *Index) ByConfigDir(dir string) (Harness, bool) {
	if i == nil || dir == "" {
		return Harness{}, false
	}
	if resolved, err := filepath.EvalSymlinks(dir); err == nil {
		dir = resolved
	}
	for _, h := range i.all {
		if h.ConfigDir == dir {
			return h, true
		}
	}
	return Harness{}, false
}

// ByPaneID finds the harness that recorded this pane. Weakest attribution: only
// `harness spawn` writes `state/herdr-pane`, and it goes stale when a pane is
// replaced.
func (i *Index) ByPaneID(paneID string) (Harness, bool) {
	if i == nil || paneID == "" {
		return Harness{}, false
	}
	for _, h := range i.all {
		if h.PaneID == paneID {
			return h, true
		}
	}
	return Harness{}, false
}

// Children returns the DIRECT children of parent, sorted by name so a move is
// deterministic and reviewable.
//
// Direct only, never grandchildren: moving a subtree wholesale would drag panes
// the user never pointed at into a new workspace.
func (i *Index) Children(parent string) []Harness {
	if i == nil || parent == "" {
		return nil
	}
	var out []Harness
	for _, h := range i.all {
		if h.Name == parent || h.Profile {
			continue // a profile has no managed process; nobody spawns it
		}
		if h.ChildOf(parent) {
			out = append(out, h)
		}
	}
	sort.Slice(out, func(a, b int) bool { return out[a].Name < out[b].Name })
	return out
}

// Ancestors walks up from name, root-most first, cycle-guarded.
func (i *Index) Ancestors(name string) []Harness {
	if i == nil {
		return nil
	}
	seen := map[string]bool{name: true}
	var chain []Harness
	cursor, ok := i.ByName(name)
	for ok && len(chain) < maxAncestry {
		parent := cursor.Parent()
		if parent == "" || seen[parent] {
			break
		}
		next, found := i.ByName(parent)
		if !found {
			break
		}
		seen[parent] = true
		chain = append(chain, next)
		cursor = next
	}
	for l, r := 0, len(chain)-1; l < r; l, r = l+1, r-1 {
		chain[l], chain[r] = chain[r], chain[l]
	}
	return chain
}

func readTrim(path string) string {
	raw, err := os.ReadFile(path)
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(raw))
}

func expandTilde(path string) string {
	if !strings.HasPrefix(path, "~") {
		return path
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return path
	}
	if path == "~" {
		return home
	}
	if strings.HasPrefix(path, "~/") {
		return filepath.Join(home, path[2:])
	}
	return path
}

func orDefault(value, fallback string) string {
	if value == "" {
		return fallback
	}
	return value
}
