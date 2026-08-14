// Package herdr is a small client for herdr's newline-delimited JSON socket
// API: one request per connection, one line in, one line out.
//
// Why not shell out to the `herdr` CLI: an action runs on the user's click, so
// its latency is felt. The socket is two syscalls and no process spawn, and it
// removes the risk of talking to a *different* instance than the one that
// invoked us — the CLI resolves its own session from the environment, while
// this client is handed the socket the host itself is listening on.
package herdr

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"time"
)

// DefaultTimeout bounds every call. The server is local and answers in
// microseconds; a short deadline means a wedged server costs a click, not a
// hang the user has to kill.
const DefaultTimeout = 4 * time.Second

// Client talks to one herdr instance.
type Client struct {
	socket string
	// buf is reused across calls in a single action run. Actions are
	// short-lived and single-goroutine, so one buffer is both safe and enough
	// to keep the allocation profile flat.
	buf []byte
}

// Dial resolves the socket of the instance that invoked us.
//
// HERDR_SOCKET_PATH wins when set (the host sets it for plugin commands).
// Otherwise reproduce what session::api_socket_path_for computes, so a fork
// installed under a different app dir does not fight upstream over one path.
// A missing socket is an error rather than a guess: acting on the wrong
// instance would move a pane the user is not looking at.
func Dial() (*Client, error) {
	if explicit := os.Getenv("HERDR_SOCKET_PATH"); explicit != "" {
		return New(explicit)
	}
	appDir := os.Getenv("HERDR_APP_DIR")
	if appDir == "" {
		appDir = "herdr"
	}
	base := os.Getenv("XDG_CONFIG_HOME")
	if base == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return nil, fmt.Errorf("resolve home: %w", err)
		}
		base = filepath.Join(home, ".config")
	}
	dir := filepath.Join(base, appDir)
	if session := os.Getenv("HERDR_SESSION"); session != "" {
		return New(filepath.Join(dir, "sessions", session, "herdr.sock"))
	}
	return New(filepath.Join(dir, "herdr.sock"))
}

// New builds a client for an explicit socket path.
func New(socket string) (*Client, error) {
	if _, err := os.Stat(socket); err != nil {
		return nil, fmt.Errorf("herdr socket %s: %w", socket, err)
	}
	return &Client{socket: socket, buf: make([]byte, 0, 4096)}, nil
}

// Socket is the path this client talks to, for diagnostics.
func (c *Client) Socket() string { return c.socket }

type response struct {
	Result json.RawMessage `json:"result"`
	Error  *struct {
		Code    string `json:"code"`
		Message string `json:"message"`
	} `json:"error"`
}

// Call sends one request and decodes `result` into out (which may be nil).
func (c *Client) Call(ctx context.Context, method string, params any, out any) error {
	deadline, ok := ctx.Deadline()
	if !ok {
		deadline = time.Now().Add(DefaultTimeout)
	}
	var dialer net.Dialer
	conn, err := dialer.DialContext(ctx, "unix", c.socket)
	if err != nil {
		return fmt.Errorf("connect %s: %w", c.socket, err)
	}
	defer conn.Close()
	if err := conn.SetDeadline(deadline); err != nil {
		return err
	}

	if params == nil {
		params = struct{}{}
	}
	req := struct {
		ID     string `json:"id"`
		Method string `json:"method"`
		Params any    `json:"params"`
	}{ID: "workspace-ops:" + method, Method: method, Params: params}

	c.buf = c.buf[:0]
	line, err := json.Marshal(req)
	if err != nil {
		return fmt.Errorf("encode %s: %w", method, err)
	}
	c.buf = append(append(c.buf, line...), '\n')
	if _, err := conn.Write(c.buf); err != nil {
		return fmt.Errorf("write %s: %w", method, err)
	}

	reader := bufio.NewReader(conn)
	raw, err := reader.ReadBytes('\n')
	if err != nil && len(raw) == 0 {
		return fmt.Errorf("read %s: %w", method, err)
	}
	var resp response
	if err := json.Unmarshal(raw, &resp); err != nil {
		return fmt.Errorf("decode %s: %w", method, err)
	}
	if resp.Error != nil {
		return fmt.Errorf("%s: %s (%s)", method, resp.Error.Message, resp.Error.Code)
	}
	if out == nil || len(resp.Result) == 0 {
		return nil
	}
	if err := json.Unmarshal(resp.Result, out); err != nil {
		return fmt.Errorf("decode %s result: %w", method, err)
	}
	return nil
}

// ── typed helpers for the handful of methods this plugin uses ──────────────

// Workspace is one entry of workspace.list. Fields verified against a live
// server, not assumed: the human name is `label`, and there is no `name`.
type Workspace struct {
	ID      string `json:"workspace_id"`
	Label   string `json:"label"`
	Number  int    `json:"number"`
	Tabs    int    `json:"tab_count"`
	Panes   int    `json:"pane_count"`
	Focused bool   `json:"focused"`
}

// Display is what to show the user for a workspace.
func (w Workspace) Display() string {
	if w.Label != "" {
		return w.Label
	}
	return w.ID
}

// Tab is one entry of tab.list.
type Tab struct {
	ID          string `json:"tab_id"`
	WorkspaceID string `json:"workspace_id"`
	Number      int    `json:"number"`
	Label       string `json:"label"`
	Focused     bool   `json:"focused"`
	PaneCount   int    `json:"pane_count"`
	Hidden      bool   `json:"hidden"`
}

// Pane is one entry of pane.list.
type Pane struct {
	ID            string `json:"pane_id"`
	TabID         string `json:"tab_id"`
	WorkspaceID   string `json:"workspace_id"`
	CWD           string `json:"cwd"`
	ForegroundCWD string `json:"foreground_cwd"`
	Focused       bool   `json:"focused"`
	TerminalTitle string `json:"terminal_title_stripped"`
}

// Agent is one entry of agent.list.
type Agent struct {
	Name        string `json:"name"`
	Agent       string `json:"agent"`
	Status      string `json:"agent_status"`
	PaneID      string `json:"pane_id"`
	TabID       string `json:"tab_id"`
	WorkspaceID string `json:"workspace_id"`
	Session     *struct {
		Source string `json:"source"`
		Agent  string `json:"agent"`
		Kind   string `json:"kind"`
		Value  string `json:"value"`
	} `json:"agent_session"`
}

// SessionValue is the runtime session id, or "" when herdr has not learned it.
func (a Agent) SessionValue() string {
	if a.Session == nil {
		return ""
	}
	return a.Session.Value
}

func (c *Client) Workspaces(ctx context.Context) ([]Workspace, error) {
	var out struct {
		Workspaces []Workspace `json:"workspaces"`
	}
	if err := c.Call(ctx, "workspace.list", nil, &out); err != nil {
		return nil, err
	}
	return out.Workspaces, nil
}

func (c *Client) Tabs(ctx context.Context) ([]Tab, error) {
	var out struct {
		Tabs []Tab `json:"tabs"`
	}
	if err := c.Call(ctx, "tab.list", nil, &out); err != nil {
		return nil, err
	}
	return out.Tabs, nil
}

func (c *Client) Panes(ctx context.Context) ([]Pane, error) {
	var out struct {
		Panes []Pane `json:"panes"`
	}
	if err := c.Call(ctx, "pane.list", nil, &out); err != nil {
		return nil, err
	}
	return out.Panes, nil
}

func (c *Client) Agents(ctx context.Context) ([]Agent, error) {
	var out struct {
		Agents []Agent `json:"agents"`
	}
	if err := c.Call(ctx, "agent.list", nil, &out); err != nil {
		return nil, err
	}
	return out.Agents, nil
}

// ProcessInfo is the foreground process group of a pane plus its shell.
type ProcessInfo struct {
	ShellPID            int `json:"shell_pid"`
	ForegroundProcesses []struct {
		PID  int    `json:"pid"`
		Name string `json:"name"`
		CWD  string `json:"cwd"`
	} `json:"foreground_processes"`
}

func (c *Client) ProcessInfo(ctx context.Context, paneID string) (ProcessInfo, error) {
	var out struct {
		Info ProcessInfo `json:"process_info"`
	}
	err := c.Call(ctx, "pane.process_info", map[string]string{"pane_id": paneID}, &out)
	return out.Info, err
}

// MoveResult is what `pane.move` reports back.
//
// Reading it is not optional: the server answers OK for a move it REFUSED,
// putting the refusal in `changed:false` + `reason`. A client that only checks
// the transport error reports success while nothing moved — which is exactly
// what happened here with a zoomed tab.
type MoveResult struct {
	Changed bool   `json:"changed"`
	Reason  string `json:"reason"`
}

// ReasonZoomedTab is the refusal we can actually fix: the host will not move a
// pane out of a zoomed tab, so we un-zoom and retry once.
const ReasonZoomedTab = "zoomed_tab"

// Zoom sets or clears zoom on a pane's tab ("on" | "off" | "toggle").
func (c *Client) Zoom(ctx context.Context, paneID, mode string) error {
	return c.Call(ctx, "pane.zoom", map[string]string{
		"pane_id": paneID,
		"mode":    mode,
	}, nil)
}

// move issues one pane.move and turns a refusal into an error, un-zooming and
// retrying once when the tab was zoomed.
func (c *Client) move(ctx context.Context, paneID string, dest map[string]any, focus bool) error {
	params := map[string]any{"pane_id": paneID, "destination": dest, "focus": focus}
	var out struct {
		Result MoveResult `json:"move_result"`
	}
	if err := c.Call(ctx, "pane.move", params, &out); err != nil {
		return err
	}
	if out.Result.Changed {
		return nil
	}
	if out.Result.Reason == ReasonZoomedTab {
		if err := c.Zoom(ctx, paneID, "off"); err != nil {
			return fmt.Errorf("pane %s is in a zoomed tab and un-zooming failed: %w", paneID, err)
		}
		if err := c.Call(ctx, "pane.move", params, &out); err != nil {
			return err
		}
		if out.Result.Changed {
			return nil
		}
	}
	reason := out.Result.Reason
	if reason == "" {
		reason = "no reason given"
	}
	return fmt.Errorf("host refused to move %s: %s", paneID, reason)
}

// MoveToNewWorkspace relocates a pane into a brand-new workspace, creating the
// workspace and the tab in one host operation.
//
// This is the whole of "convert to workspace": doing it as create + move would
// leave a half-built workspace behind if the second call failed, and `tab.move`
// cannot cross workspaces at all (it takes an insert_index, not a target).
func (c *Client) MoveToNewWorkspace(ctx context.Context, paneID, label, tabLabel string, focus bool) error {
	dest := map[string]any{"type": "new_workspace"}
	if label != "" {
		dest["label"] = label
	}
	if tabLabel != "" {
		dest["tab_label"] = tabLabel
	}
	return c.move(ctx, paneID, dest, focus)
}

// MoveToWorkspace relocates a pane into a new tab of an EXISTING workspace.
func (c *Client) MoveToWorkspace(ctx context.Context, paneID, workspaceID, label string, focus bool) error {
	dest := map[string]any{"type": "new_tab", "workspace_id": workspaceID}
	if label != "" {
		dest["label"] = label
	}
	return c.move(ctx, paneID, dest, focus)
}

// RenameWorkspace sets a workspace's display label.
func (c *Client) RenameWorkspace(ctx context.Context, workspaceID, label string) error {
	return c.Call(ctx, "workspace.rename", map[string]string{
		"workspace_id": workspaceID,
		"label":        label,
	}, nil)
}

// FocusWorkspace brings a workspace to the front.
func (c *Client) FocusWorkspace(ctx context.Context, workspaceID string) error {
	return c.Call(ctx, "workspace.focus", map[string]string{"workspace_id": workspaceID}, nil)
}

// Notify shows a herdr notification.
//
// There is no `ui.toast` method — an action's only channel back to the user is
// `notification.show`, since the host toasts a plugin command only when the
// DISPATCH fails, never when the work itself does. Without this an action that
// half-succeeded would be silent.
func (c *Client) Notify(ctx context.Context, title, body string) error {
	params := map[string]any{"title": title}
	if body != "" {
		params["body"] = body
	}
	return c.Call(ctx, "notification.show", params, nil)
}
