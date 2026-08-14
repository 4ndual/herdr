package meta

import (
	"testing"

	"github.com/herdrdev/herdr/plugins/workspace-ops-ui/internal/herdr"
)

// The field report that produced this test: acting on a pane in w1D, the copied
// metadata said "tab w1C:t3 worker-c / workspace w1C plugfix" — the tab and
// workspace that happened to be SELECTED. Only the pane id and shell folder
// were right.
//
// HERDR_TAB_ID and HERDR_WORKSPACE_ID carry the selection at menu-open time, so
// this pins the case where the selection DISAGREES with the pane. A test where
// they agree passes against the bug.
func TestParentageComesFromThePaneNotTheSelection(t *testing.T) {
	t.Setenv("HERDR_TAB_ID", "w1C:t3")    // selected tab — wrong for this pane
	t.Setenv("HERDR_WORKSPACE_ID", "w1C") // selected workspace — also wrong

	target := Target{PaneID: "w1D:p1"}
	panes := []herdr.Pane{
		{ID: "w1C:p9", TabID: "w1C:t3", WorkspaceID: "w1C"},
		{ID: "w1D:p1", TabID: "w1D:t1", WorkspaceID: "w1D"},
	}
	if err := applyParentage(&target, panes); err != nil {
		t.Fatalf("applyParentage: %v", err)
	}
	if target.TabID != "w1D:t1" {
		t.Errorf("tab: got %q, want w1D:t1 — the selection leaked in", target.TabID)
	}
	if target.WorkspaceID != "w1D" {
		t.Errorf("workspace: got %q, want w1D — the selection leaked in", target.WorkspaceID)
	}
}

// A pane that has gone must be reported, not papered over with the selection —
// otherwise the metadata describes a pane the user is not looking at.
func TestAMissingPaneIsAnError(t *testing.T) {
	target := Target{PaneID: "w9:p9"}
	err := applyParentage(&target, []herdr.Pane{{ID: "w1:p1", TabID: "w1:t1", WorkspaceID: "w1"}})
	if err == nil {
		t.Fatal("a pane that is not in the list must be an error, not a silent fallback")
	}
}
