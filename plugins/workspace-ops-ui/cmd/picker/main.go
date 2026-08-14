// Command picker is the "Move to workspace" overlay: a workspace list the user
// drives with the keyboard or the mouse.
//
// It follows the overlay canon (plugin-ux-standard.md §3, §6):
//
//		╭─ move to workspace ──────────────────── [x] ─╮
//		│  tab      tb-kid1                            │
//		│                                              │
//		│  ▸ fleet                    19 tabs · current│
//		│    qa                        3 tabs          │
//		│                                              │
//		│  run: pane.move w1:p3 → w2                   │
//		│  ↑↓ move · ↵ move here · esc close           │
//		│           [ ↵ move ]  [ esc cancel ]         │
//		╰──────────────────────────────────────────────╯
//
//	  - border with the title ON the border, left-aligned dim labels;
//	  - a live run-preview line: dim "run:" prefix, the command in green;
//	  - a footer hint row plus explicit commit/cancel buttons;
//	  - Esc always closes, and a CLICKABLE [x] in the top-right, because herdr is
//	    a mouse-first TUI and "press escape" is not an affordance you can see.
//
// The overlay owns its own keyboard and mouse; herdr paints only the chrome.
package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"github.com/herdrdev/herdr/plugins/workspace-ops-ui/internal/herdr"
	"github.com/herdrdev/herdr/plugins/workspace-ops-ui/internal/meta"
)

// Palette roles, mapped to the canon's meanings (plugin-ux-standard.md §4).
var (
	styleTitle  = lipgloss.NewStyle().Bold(true)
	styleLabel  = lipgloss.NewStyle().Foreground(lipgloss.Color("245")) // dim field labels
	styleMuted  = lipgloss.NewStyle().Foreground(lipgloss.Color("240")) // inert: hints, units
	styleRun    = lipgloss.NewStyle().Foreground(lipgloss.Color("42"))  // green: the command
	styleSelect = lipgloss.NewStyle().Foreground(lipgloss.Color("39")).Bold(true)
	styleClose  = lipgloss.NewStyle().Foreground(lipgloss.Color("203")) // the [x] affordance
	styleErr    = lipgloss.NewStyle().Foreground(lipgloss.Color("203"))
	styleOK     = lipgloss.NewStyle().Foreground(lipgloss.Color("42"))
	styleBorder = lipgloss.NewStyle().Border(lipgloss.RoundedBorder()).Padding(0, 1)
)

type model struct {
	client *herdr.Client
	target meta.Target
	plan   meta.Plan

	spaces []herdr.Workspace
	cursor int

	// closeHit is the screen cell span of the [x] button, recomputed on every
	// render so a resize cannot leave a stale hit-rect behind — the exact class
	// of bug the real-click gate exists to catch.
	closeX, closeY, closeW int
	// listY is the first row of the workspace list, for click-to-select.
	listY int

	status string
	failed bool
	done   bool
	width  int
	height int
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintf(os.Stderr, "workspace picker: %v\n", err)
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
	target := meta.Target{
		PaneID:      os.Getenv("WSOPS_TARGET_PANE"),
		TabID:       os.Getenv("WSOPS_TARGET_TAB"),
		WorkspaceID: os.Getenv("WSOPS_TARGET_WS"),
		TabLabel:    os.Getenv("WSOPS_TARGET_LABEL"),
		// The host attaches this overlay to the focused tab and sets
		// HERDR_PANE_ID to the pane it created for us, so without this the
		// picker plans to move itself.
		ExcludePaneID: os.Getenv("HERDR_PANE_ID"),
	}
	if target.PaneID == "" {
		// Opened directly rather than through the action: fall back to whatever
		// the host says is focused, which is at least honest.
		if t, err := meta.ResolveTarget(ctx, client); err == nil {
			target = t
		}
	}
	plan, err := meta.BuildPlan(ctx, client, target)
	if err != nil {
		return err
	}
	spaces, err := client.Workspaces(ctx)
	if err != nil {
		return err
	}
	// The workspace the tab already lives in is not a destination.
	filtered := spaces[:0:0]
	for _, ws := range spaces {
		if ws.ID != target.WorkspaceID {
			filtered = append(filtered, ws)
		}
	}

	debugf("target pane=%s tab=%s ws=%s label=%q exclude=%s panes=%d children=%v spaces=%d/%d",
		target.PaneID, target.TabID, target.WorkspaceID, target.TabLabel,
		target.ExcludePaneID, len(plan.Panes), plan.Children, len(filtered), len(spaces))
	m := model{client: client, target: target, plan: plan, spaces: filtered}
	prog := tea.NewProgram(&m, tea.WithAltScreen(), tea.WithMouseCellMotion())
	_, err = prog.Run()
	return err
}

// debugf writes to $WSOPS_DEBUG when set. The overlay owns the terminal, so
// printing anywhere else is invisible; without this, "the click did nothing"
// is unfalsifiable.
func debugf(format string, args ...any) {
	path := os.Getenv("WSOPS_DEBUG")
	if path == "" {
		return
	}
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o600)
	if err != nil {
		return
	}
	defer f.Close()
	log.New(f, "", log.Ltime).Printf(format, args...)
}

func (m *model) Init() tea.Cmd { return nil }

func (m *model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.width, m.height = msg.Width, msg.Height

	case tea.KeyMsg:
		switch msg.String() {
		case "esc", "ctrl+c", "q":
			return m, tea.Quit
		case "up", "k":
			m.move(-1)
		case "down", "j":
			m.move(1)
		case "enter":
			return m, m.commit()
		}

	case tea.MouseMsg:
		debugf("mouse action=%v button=%v x=%d y=%d listY=%d closeX=%d closeY=%d rows=%d",
			msg.Action, msg.Button, msg.X, msg.Y, m.listY, m.closeX, m.closeY, len(m.spaces))
		if msg.Action != tea.MouseActionPress || msg.Button != tea.MouseButtonLeft {
			break
		}
		// The [x] button: a real click must close, not just Esc.
		if msg.Y == m.closeY && msg.X >= m.closeX && msg.X < m.closeX+m.closeW {
			return m, tea.Quit
		}
		if idx := msg.Y - m.listY; idx >= 0 && idx < len(m.spaces) {
			m.cursor = idx
			return m, m.commit()
		}
	}
	if m.done {
		return m, tea.Quit
	}
	return m, nil
}

func (m *model) move(delta int) {
	if len(m.spaces) == 0 {
		return
	}
	m.cursor = (m.cursor + delta + len(m.spaces)) % len(m.spaces)
}

// commit performs the move and quits. The overlay does the work itself because
// it is the only process that knows what the user picked; handing the choice
// back to the action would need a channel that does not exist.
func (m *model) commit() tea.Cmd {
	if len(m.spaces) == 0 {
		return tea.Quit
	}
	dest := m.spaces[m.cursor]
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	debugf("commit dest=%s(%s) panes=%d", dest.ID, dest.Display(), len(m.plan.Panes))
	if _, err := m.plan.Execute(ctx, m.client, dest.ID, true); err != nil {
		debugf("commit failed: %v", err)
		m.status, m.failed = err.Error(), true
		return nil
	}
	_ = m.client.Notify(ctx, "moved to workspace", m.plan.Summary(dest.Display()))
	m.status, m.done = m.plan.Summary(dest.Display()), true
	return tea.Quit
}

// runPreview is the live command line: what Enter will actually do.
func (m *model) runPreview() string {
	if len(m.spaces) == 0 {
		return "no other workspace to move to"
	}
	dest := m.spaces[m.cursor]
	panes := len(m.plan.Panes)
	if panes <= 1 {
		return fmt.Sprintf("pane.move %s → %s", m.target.PaneID, dest.Display())
	}
	return fmt.Sprintf("pane.move ×%d → %s", panes, dest.Display())
}

func (m *model) View() string {
	const inner = 58 // content width inside the border

	title := " move to workspace "
	closeBtn := "[x]"
	// Title sits ON the top border; the [x] sits at its right end.
	dashes := inner - lipgloss.Width(title) - lipgloss.Width(closeBtn) - 2
	if dashes < 1 {
		dashes = 1
	}
	top := styleMuted.Render("╭─") + styleTitle.Render(title) +
		styleMuted.Render(strings.Repeat("─", dashes)) + styleClose.Render(closeBtn) + styleMuted.Render("─╮")

	var b strings.Builder
	b.WriteString(top + "\n")

	// The [x] hit-rect, in screen cells. lipgloss renders the border at column
	// 0 of this overlay pane, so the button's X is its offset in `top`.
	m.closeY = 0
	m.closeX = 2 + lipgloss.Width(title) + dashes
	m.closeW = lipgloss.Width(closeBtn)

	row := func(s string) { b.WriteString(styleMuted.Render("│ ") + s + "\n") }

	row(styleLabel.Render("tab   ") + "  " + m.target.TabLabel)
	if len(m.plan.Children) > 0 {
		row(styleLabel.Render("moves ") + "  " +
			fmt.Sprintf("%d panes · children: %s", len(m.plan.Panes), strings.Join(m.plan.Children, ", ")))
	}
	row("")

	// Row 0 is the border, row 1 the "tab" label, row 2 the children line when
	// there is one, then a blank — so the first workspace sits at 3, or 4 when
	// children are listed. Counting this wrong sends every click one row off,
	// which reads as "the overlay ignores the mouse".
	m.listY = 3
	if len(m.plan.Children) > 0 {
		m.listY = 4
	}
	if len(m.spaces) == 0 {
		row(styleMuted.Render("  no other workspace — use “convert to workspace”"))
	}
	for i, ws := range m.spaces {
		marker := "  "
		line := fmt.Sprintf("%-24s %2d tabs", ws.Display(), ws.Tabs)
		if ws.Focused {
			line += styleMuted.Render(" · current")
		}
		if i == m.cursor {
			marker = styleSelect.Render("▸ ")
			line = styleSelect.Render(line)
		}
		row(marker + line)
	}

	row("")
	row(styleLabel.Render("run:") + " " + styleRun.Render(m.runPreview()))
	if m.status != "" {
		if m.failed {
			row(styleErr.Render("! " + m.status))
		} else {
			row(styleOK.Render("✓ " + m.status))
		}
	}
	row(styleMuted.Render("↑↓ move · ↵ move here · esc close"))
	row(styleSelect.Render("[ ↵ move ]") + "  " + styleMuted.Render("[ esc cancel ]"))

	b.WriteString(styleMuted.Render("╰" + strings.Repeat("─", inner) + "╯"))
	return b.String()
}

var _ = styleBorder // reserved: the manual border above keeps the title/[x] on it
