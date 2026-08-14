package main

import "encoding/json"

// View is which of the website's pages the panel is showing.
type View string

const (
	ViewFleet        View = "fleet"
	ViewDetail       View = "detail"
	ViewOrchestrator View = "orchestrator"
	ViewTemplates    View = "templates"
	ViewModels       View = "models"
	ViewMemory       View = "memory"
)

// AllViews is the tab order, mirroring the site's top nav.
var AllViews = []View{ViewFleet, ViewDetail, ViewOrchestrator, ViewTemplates, ViewModels, ViewMemory}

// Harness is one row of /api/fleet. Only the fields the panel draws are kept —
// the daemon sends more, and decoding into a narrower struct is deliberate so a
// new daemon field cannot change this plugin's behaviour by accident.
type Harness struct {
	Name         string   `json:"name"`
	Template     string   `json:"template"`
	Runtime      string   `json:"runtime"`
	Runtimes     []string `json:"runtimes"`
	Profile      bool     `json:"profile"`
	ConfigDir    string   `json:"configDir"`
	Status       string   `json:"status"`
	SessionID    string   `json:"sessionId"`
	Ctx          *float64 `json:"ctx"`
	Pending      int      `json:"pending"`
	Sessions     int      `json:"sessions"`
	Host         string   `json:"host"`
	PersonaLabel string   `json:"personaLabel"`
}

// TreeNode is one node of /api/orchestrator/livetree.
type TreeNode struct {
	Name    string `json:"name"`
	Status  string `json:"status"`
	State   string `json:"state"`
	Parent  string `json:"parent"`
	Live    bool   `json:"live"`
	Task    string `json:"task"`
	Since   string `json:"since"`
	Session string `json:"session"`
}

// Template is one row of /api/templates.
type Template struct {
	Name        string `json:"name"`
	Version     string `json:"version"`
	Description string `json:"description"`
}

// Memory is one memory file surfaced by the harness detail endpoint.
type Memory struct {
	Rel  string `json:"rel"`
	Name string `json:"name"`
	Body string `json:"body"`
}

// Session is one row of the detail payload's session list.
//
// NOTE the shape clash this exists to survive: /api/fleet reports `sessions` as
// a COUNT, while /api/harness/<name> reports it as an ARRAY of these. Decoding
// the detail into a struct that said `int` failed the whole document, so the
// panel silently had no detail at all. Two fields, two types, on purpose.
type Session struct {
	ID      string  `json:"id"`
	Bytes   int64   `json:"bytes"`
	MTime   float64 `json:"mtime"`
	Current bool    `json:"current"`
}

// Detail is the subset of /api/harness/<name> the panel renders.
type Detail struct {
	Name       string   `json:"name"`
	Status     string   `json:"status"`
	Runtime    string   `json:"runtime"`
	Template   string   `json:"template"`
	ConfigDir  string   `json:"configDir"`
	SessionID  string   `json:"sessionId"`
	Pending    int       `json:"pending"`
	Sessions   []Session `json:"sessions"`
	LiveRules  string    `json:"liveRules"`
	ClaudeMd   string    `json:"claudeMd"`
	MemoryDir  string    `json:"memoryDir"`
	PendingKey []string  `json:"pendingKeys"`
	Memories   []Memory  `json:"memories"`
}

// Snapshot is the whole panel model: exactly what the guest renders, computed
// here so the guest never has to know a daemon route or a JSON shape.
type Snapshot struct {
	// Connected reports the WebSocket, not the daemon: false means the panel is
	// showing the last known good state and says so, rather than blanking.
	Connected bool   `json:"connected"`
	Daemon    string `json:"daemon"`
	// UpdatedUnix is when this snapshot was built, so the guest can age it.
	UpdatedUnix int64 `json:"updated_unix"`
	// Reason is what produced this snapshot: "event", "selection", "view",
	// "safety-poll" or "mutation". Rendered in the footer, and the honest answer
	// to "is this thing live or is it polling".
	Reason string `json:"reason"`

	View     View   `json:"view"`
	Selected string `json:"selected"`
	// Tab within the detail view.
	DetailTab string `json:"detail_tab"`
	Scroll    int    `json:"scroll"`
	// Open drives the panel body: closed renders a single line, open renders the
	// views. The panel never relinquishes its carve — a guest that stops
	// emitting a layer is stranded, because the host then only ever sends it a
	// zero rect and it can never claim geometry again.
	Open bool `json:"open"`

	Fleet     []Harness       `json:"fleet"`
	Tree      []TreeNode      `json:"tree"`
	Templates []Template      `json:"templates"`
	Models    json.RawMessage `json:"models"`
	Exposure  json.RawMessage `json:"exposure"`
	Detail    *Detail         `json:"detail"`

	// LastOp is the outcome of the most recent mutation, so a click that failed
	// says why instead of looking like nothing happened.
	LastOp *OpResult `json:"last_op"`
}

// OpResult records one mutation attempt.
type OpResult struct {
	Action  string `json:"action"`
	Target  string `json:"target"`
	OK      bool   `json:"ok"`
	Message string `json:"message"`
	AtUnix  int64  `json:"at_unix"`
}

// Control is what the guest asks for, written by `op` and watched by `collect`.
// Kept separate from the snapshot so a view change never races a daemon event.
type Control struct {
	View      View   `json:"view"`
	Selected  string `json:"selected"`
	DetailTab string `json:"detail_tab"`
	Scroll    int    `json:"scroll"`
	// Open, not Closed: the zero value must mean the panel is CLOSED, so a fresh
	// install shows only the launcher icon and the panel is something the user
	// opens — the same shape as Plugin Manager's rail icon.
	Open bool `json:"open"`
	// Nonce changes on every write so the collector can detect a repeat request
	// (e.g. refresh pressed twice) that would otherwise look identical.
	Nonce int64 `json:"nonce"`
}
