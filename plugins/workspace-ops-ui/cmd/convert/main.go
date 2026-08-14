// Command convert implements the "Convert to workspace" context-menu action.
//
// From the invoked tab: create a workspace named after the tab, move the tab's
// panes into it, and — when tidus is present — move each DIRECT child's tab
// too, so a spawned subtree stays together.
//
// The whole thing is one `pane.move` per pane. `tab.move` cannot cross
// workspaces (it takes an insert_index), and create-then-move would leave an
// empty workspace behind whenever the second call failed.
//
// Session continuity is the hard requirement and it comes from the mechanism,
// not from care: `pane.move` relocates the LIVE pane, so the PTY and the agent
// process are never restarted. Verified in the gates by pid, session id and a
// marker chat that survives the move with no resume.
package main

import (
	"context"
	"fmt"
	"os"
	"time"

	"github.com/herdrdev/herdr/plugins/workspace-ops-ui/internal/herdr"
	"github.com/herdrdev/herdr/plugins/workspace-ops-ui/internal/meta"
)

func main() {
	if err := run(); err != nil {
		// stderr is captured into the plugin command log; the notification is
		// what the user actually sees.
		fmt.Fprintf(os.Stderr, "convert-to-workspace: %v\n", err)
		os.Exit(1)
	}
}

func run() error {
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	client, err := herdr.Dial()
	if err != nil {
		return err
	}
	target, err := meta.ResolveTarget(ctx, client)
	if err != nil {
		return err
	}
	plan, err := meta.BuildPlan(ctx, client, target)
	if err != nil {
		return err
	}

	workspace, err := plan.Execute(ctx, client, "", true)
	if err != nil {
		if workspace != "" {
			_ = client.Notify(ctx, "convert to workspace: partial",
				fmt.Sprintf("%v — panes already moved are in the new workspace", err))
		}
		return err
	}
	name := plan.WorkspaceName()
	_ = client.Notify(ctx, "converted to workspace", plan.Summary(name))
	fmt.Printf("%s\n", plan.Summary(name))
	return nil
}
