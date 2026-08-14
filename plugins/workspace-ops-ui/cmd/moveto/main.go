// Command moveto implements the "Move to workspace" context-menu action.
//
// It does not move anything itself: it resolves WHICH tab the user invoked and
// opens the picker overlay, handing the target across in the overlay's
// environment. The picker performs the move once a destination is chosen.
//
// Resolving the target here rather than in the overlay is deliberate: the host
// captures the invocation context before the overlay takes focus, so by the
// time the picker is running, "the focused pane" is the picker itself.
package main

import (
	"context"
	"fmt"
	"os"
	"time"

	"github.com/herdrdev/herdr/plugins/workspace-ops-ui/internal/herdr"
	"github.com/herdrdev/herdr/plugins/workspace-ops-ui/internal/meta"
)

// The host owns overlay geometry: `width`/`height` are accepted only for
// `placement = "popup"` and are rejected outright for an overlay, so the picker
// sizes its own content to whatever rows it is given.
func main() {
	if err := run(); err != nil {
		fmt.Fprintf(os.Stderr, "move-to-workspace: %v\n", err)
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

	pluginID := os.Getenv("HERDR_PLUGIN_ID")
	if pluginID == "" {
		pluginID = "builtin.workspace-ops"
	}
	params := map[string]any{
		"plugin_id":  pluginID,
		"entrypoint": "picker",
		"placement":  "overlay",
		"focus":      true,
		"env": map[string]string{
			"WSOPS_TARGET_PANE":  target.PaneID,
			"WSOPS_TARGET_TAB":   target.TabID,
			"WSOPS_TARGET_WS":    target.WorkspaceID,
			"WSOPS_TARGET_LABEL": target.TabLabel,
			"HERDR_SOCKET_PATH":  client.Socket(),
		},
	}
	// The overlay is spawned by the host with exactly the env we hand it, so a
	// debug switch has to be forwarded explicitly or it silently does nothing.
	if dbg := os.Getenv("WSOPS_DEBUG"); dbg != "" {
		params["env"].(map[string]string)["WSOPS_DEBUG"] = dbg
	}
	// The picker is a separate process in an overlay pane, so this action's only
	// job is to open it — and until now it reported nothing either way. An open
	// that fails left the menu looking dead, which is exactly the complaint.
	if err := client.Call(ctx, "plugin.pane.open", params, nil); err != nil {
		herdr.Report(ctx, client, "move-to-workspace", "move to workspace failed", err.Error())
		return err
	}
	herdr.Report(ctx, client, "move-to-workspace", "pick a workspace",
		"choose a destination for "+target.TabLabel)
	return nil
}
