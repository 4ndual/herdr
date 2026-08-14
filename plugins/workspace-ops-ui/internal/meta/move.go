package meta

import (
	"context"
	"fmt"
	"strings"

	"github.com/herdrdev/herdr/plugins/workspace-ops-ui/internal/fleet"
	"github.com/herdrdev/herdr/plugins/workspace-ops-ui/internal/herdr"
)

// Moved is one pane relocated by a move operation.
type Moved struct {
	Harness string // "" when the pane is not a fleet agent
	PaneID  string
	TabLbl  string
}

// Plan is what a convert/move will do, resolved before anything is touched so
// the whole operation can be reported (and refused) as a unit.
type Plan struct {
	Target Target
	// Panes are every pane to relocate, in order: the invoked tab's panes
	// first, then one pane per direct tidus child.
	Panes []Moved
	// Children names, for the report.
	Children []string
	// Attribution of the invoked pane, for the report.
	Attribution Attribution
}

// BuildPlan resolves the invoked tab, its panes, and — when tidus is present —
// the direct children of the harness behind it.
//
// With no fleet on disk this returns exactly the invoked tab's panes, which is
// the "works with zero tidus installed" contract: the same code path, with an
// empty children list, not a separate mode.
func BuildPlan(ctx context.Context, c *herdr.Client, target Target) (Plan, error) {
	plan := Plan{Target: target}

	panes, err := c.Panes(ctx)
	if err != nil {
		return plan, err
	}
	tabs, err := c.Tabs(ctx)
	if err != nil {
		return plan, err
	}
	labelOf := make(map[string]string, len(tabs))
	for _, t := range tabs {
		labelOf[t.ID] = t.Label
	}

	// Every pane of the invoked tab travels together: a split is one workspace
	// unit, and leaving half of it behind would be a data-losing surprise.
	for _, p := range panes {
		if p.TabID == target.TabID && p.ID != target.ExcludePaneID {
			plan.Panes = append(plan.Panes, Moved{PaneID: p.ID, TabLbl: labelOf[p.TabID]})
		}
	}
	if len(plan.Panes) == 0 {
		return plan, fmt.Errorf("tab %s has no panes", target.TabID)
	}

	idx := fleet.NewIndex(fleet.Load(fleet.Root()))
	if idx.Empty() {
		return plan, nil // no tidus: the single tab is the whole job
	}
	plan.Attribution = Attribute(ctx, c, idx, target.PaneID)
	if !plan.Attribution.Found {
		return plan, nil // a pane that is not a fleet agent has no children
	}

	// Direct children only, located by asking each pane which harness it is
	// RUNNING, rather than asking each harness which tab it remembers.
	//
	// The remembered ids are not trustworthy: `state/herdr-tab` is rewritten
	// only when the fleet's SessionStart hook runs, so it goes stale the moment
	// a tab is replaced — including by this very plugin, which closes the old
	// tab when it moves a pane. herdr's agent registry is fresher but only
	// names agents someone started through it, so `harness spawn` children are
	// missing from it. A first run trusting those two sources moved one child
	// out of three while the other two sat on screen.
	//
	// The pane's own process environment is neither stale nor partial, and this
	// runs once per explicit click, not on a tick.
	liveTabs := make(map[string]bool, len(tabs))
	for _, t := range tabs {
		liveTabs[t.ID] = true
	}
	wanted := make(map[string]bool, 8)
	for _, child := range idx.Children(plan.Attribution.Harness.Name) {
		wanted[child.Name] = true
	}
	if len(wanted) == 0 {
		return plan, nil
	}

	// harness name → the tabs its panes live in, in pane order.
	tabsOf := make(map[string][]string, len(wanted))
	seenTab := make(map[string]bool, len(panes))
	for _, p := range panes {
		if p.TabID == target.TabID || !liveTabs[p.TabID] {
			continue
		}
		if p.ID == target.ExcludePaneID {
			continue
		}
		attr := Attribute(ctx, c, idx, p.ID)
		if !attr.Found || !wanted[attr.Harness.Name] {
			continue
		}
		key := attr.Harness.Name + "\x00" + p.TabID
		if seenTab[key] {
			continue
		}
		seenTab[key] = true
		tabsOf[attr.Harness.Name] = append(tabsOf[attr.Harness.Name], p.TabID)
	}

	for _, child := range idx.Children(plan.Attribution.Harness.Name) {
		childTabs := tabsOf[child.Name]
		if len(childTabs) == 0 {
			// Fall back to what the fleet remembers, if it still exists — a
			// child whose runtime has exited has no process to ask.
			if child.TabID != "" && liveTabs[child.TabID] && child.TabID != target.TabID {
				childTabs = []string{child.TabID}
			} else {
				continue
			}
		}
		plan.Children = append(plan.Children, child.Name)
		for _, tabID := range childTabs {
			for _, p := range panes {
				if p.TabID == tabID {
					plan.Panes = append(plan.Panes, Moved{
						Harness: child.Name,
						PaneID:  p.ID,
						TabLbl:  labelOf[tabID],
					})
				}
			}
		}
	}
	return plan, nil
}

// WorkspaceName is the name a converted workspace gets: the invoked tab's
// label, falling back to the harness, then to something honest rather than
// blank.
func (p Plan) WorkspaceName() string {
	if label := strings.TrimSpace(p.Target.TabLabel); label != "" {
		return label
	}
	if p.Attribution.Found {
		return p.Attribution.Harness.Name
	}
	return "workspace"
}

// Execute relocates every planned pane.
//
// The first pane creates the destination (a new workspace for convert, or the
// first new tab for move); the rest follow it. Order matters: creating the
// workspace with the first pane is a single host operation, so a failure
// halfway cannot leave an empty workspace behind.
//
// `focus` applies only to the LAST pane moved, so the user lands on the tab
// they invoked rather than watching focus bounce through every child.
func (p Plan) Execute(ctx context.Context, c *herdr.Client, destWorkspaceID string, focus bool) (string, error) {
	if len(p.Panes) == 0 {
		return "", fmt.Errorf("nothing to move")
	}
	created := destWorkspaceID

	for i, pane := range p.Panes {
		last := i == len(p.Panes)-1
		wantFocus := focus && last
		label := pane.TabLbl
		if label == "" {
			label = pane.Harness
		}

		Debugf("execute pane=%s harness=%s label=%q dest=%q focus=%v", pane.PaneID, pane.Harness, label, created, wantFocus)
		if created == "" {
			// Convert: one call creates the workspace, names it, creates the
			// tab and moves the live pane into it. `tab.move` cannot do this —
			// it takes an insert_index and never crosses workspaces.
			if err := c.MoveToNewWorkspace(ctx, pane.PaneID, p.WorkspaceName(), label, wantFocus); err != nil {
				return "", fmt.Errorf("create workspace from %s: %w", pane.PaneID, err)
			}
			id, err := findWorkspaceByLabel(ctx, c, p.WorkspaceName())
			if err != nil {
				return "", err
			}
			created = id
			continue
		}
		err := c.MoveToWorkspace(ctx, pane.PaneID, created, label, wantFocus)
		Debugf("  -> move %s to %s err=%v", pane.PaneID, created, err)
		if err != nil {
			// Partial moves are reported rather than rolled back: the panes
			// that did move are alive and reachable in the new workspace, and
			// moving them back would be a second chance to fail.
			return created, fmt.Errorf("move %s (%s): %w", pane.PaneID, pane.Harness, err)
		}
	}
	return created, nil
}

func findWorkspaceByLabel(ctx context.Context, c *herdr.Client, label string) (string, error) {
	spaces, err := c.Workspaces(ctx)
	if err != nil {
		return "", err
	}
	// Newest last: the workspace we just made is the highest-numbered match.
	best := ""
	bestNum := -1
	for _, ws := range spaces {
		if ws.Label == label && ws.Number > bestNum {
			best, bestNum = ws.ID, ws.Number
		}
	}
	if best == "" {
		return "", fmt.Errorf("workspace %q not found after creating it", label)
	}
	return best, nil
}

// Summary is the one-line report an action shows the user.
func (p Plan) Summary(workspace string) string {
	panes := len(p.Panes)
	switch {
	case len(p.Children) == 0 && panes == 1:
		return fmt.Sprintf("moved %s to %s", p.Target.TabLabel, workspace)
	case len(p.Children) == 0:
		return fmt.Sprintf("moved %s (%d panes) to %s", p.Target.TabLabel, panes, workspace)
	default:
		return fmt.Sprintf("moved %s + %d tidus child%s (%s) to %s",
			p.Target.TabLabel, len(p.Children), plural(len(p.Children)),
			strings.Join(p.Children, ", "), workspace)
	}
}

func plural(n int) string {
	if n == 1 {
		return ""
	}
	return "ren"
}
