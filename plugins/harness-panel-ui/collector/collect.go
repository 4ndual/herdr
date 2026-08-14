package main

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"log"
	"os"
	"path/filepath"
	"sync"
	"time"
)

const (
	// safetyPollInterval covers state the daemon never announces (see doc.go).
	// Slow on purpose: the fleet list is push-fed, so this only exists to catch
	// out-of-band edits, and a fast timer here would be exactly the polling the
	// architecture is meant to avoid.
	safetyPollInterval = 30 * time.Second
	// controlPollInterval is how often the guest's control file is checked. This
	// is a local stat() of one small file, not a network call: herdr gives a
	// plugin action no way to signal a running sidecar, so a file is the channel
	// and this is the read side of it.
	controlPollInterval = 250 * time.Millisecond
	// wsReadDeadline bounds one frame read so a wedged daemon surfaces as a
	// reconnect instead of a parked goroutine.
	wsReadDeadline = 90 * time.Second
)

// Collector owns the panel's state and every goroutine that mutates it.
type Collector struct {
	client   *Client
	stateDir string

	mu       sync.Mutex
	snap     Snapshot
	lastCtl  Control
	fleetSum string
}

func NewCollector(client *Client, stateDir string) *Collector {
	return &Collector{
		client:   client,
		stateDir: stateDir,
		snap: Snapshot{
			Daemon: client.base,
			View:   ViewFleet,
			Reason: "startup",
		},
	}
}

func (c *Collector) inputPath() string   { return filepath.Join(c.stateDir, "harness_panel_input.json") }
func (c *Collector) controlPath() string { return filepath.Join(c.stateDir, "harness_panel_control.json") }

// Run blocks until ctx is cancelled. Every goroutine it starts is joined before
// it returns, so the process leaves nothing behind — which is what the goleak
// check in the tests asserts.
func (c *Collector) Run(ctx context.Context) error {
	var wg sync.WaitGroup
	wg.Add(3)
	go func() { defer wg.Done(); c.runEvents(ctx) }()
	go func() { defer wg.Done(); c.runControl(ctx) }()
	go func() { defer wg.Done(); c.runSafetyPoll(ctx) }()

	// One eager fetch so the panel has content before the first daemon push.
	c.refreshAll(ctx, "startup")
	wg.Wait()
	return ctx.Err()
}

// runEvents holds the WebSocket and reconnects with backoff. This is the
// primary path: everything the daemon announces arrives here.
func (c *Collector) runEvents(ctx context.Context) {
	backoff := 500 * time.Millisecond
	const maxBackoff = 15 * time.Second
	for ctx.Err() == nil {
		conn, err := dialWS(ctx, c.client.base)
		if err != nil {
			c.setConnected(false)
			if !sleepCtx(ctx, backoff) {
				return
			}
			if backoff < maxBackoff {
				backoff *= 2
			}
			continue
		}
		backoff = 500 * time.Millisecond
		c.setConnected(true)

		// Close on cancellation so a blocked read unblocks immediately rather
		// than waiting out the deadline.
		closed := make(chan struct{})
		var once sync.Once
		stop := func() { once.Do(func() { _ = conn.Close(); close(closed) }) }
		go func() {
			select {
			case <-ctx.Done():
				stop()
			case <-closed:
			}
		}()

		for ctx.Err() == nil {
			payload, err := conn.ReadMessage(wsReadDeadline)
			if err != nil {
				if !errors.Is(err, io.EOF) && ctx.Err() == nil {
					log.Printf("harness-panel: websocket read: %v", err)
				}
				break
			}
			c.handleFrame(ctx, payload)
		}
		stop()
		c.setConnected(false)
		if !sleepCtx(ctx, backoff) {
			return
		}
	}
}

// handleFrame applies one daemon push. A fleet frame is authoritative for the
// list; anything derived from it that the current view needs is refreshed in
// the same beat, so a spawn shows up in the tree without waiting for a timer.
func (c *Collector) handleFrame(ctx context.Context, payload []byte) {
	var frame struct {
		Type string          `json:"type"`
		Data json.RawMessage `json:"data"`
	}
	if err := json.Unmarshal(payload, &frame); err != nil {
		return
	}
	if frame.Type != "fleet" {
		return
	}
	var fleet []Harness
	if err := json.Unmarshal(frame.Data, &fleet); err != nil {
		return
	}
	c.mu.Lock()
	c.snap.Fleet = fleet
	changed := c.fleetSum != fleetSummary(fleet)
	c.fleetSum = fleetSummary(fleet)
	view, selected := c.snap.View, c.snap.Selected
	c.mu.Unlock()

	if changed {
		// A fleet change is the trigger for the derived views — no timer.
		c.refreshDerived(ctx, view, selected)
	}
	c.publish("event")
}

// runControl watches the file the guest's action writes and applies view /
// selection changes, fetching whatever the new view needs.
func (c *Collector) runControl(ctx context.Context) {
	ticker := time.NewTicker(controlPollInterval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			ctl, err := c.readControl()
			if err != nil {
				continue
			}
			c.mu.Lock()
			same := ctl == c.lastCtl
			c.lastCtl = ctl
			c.mu.Unlock()
			if same {
				continue
			}
			c.mu.Lock()
			c.snap.View = ctl.View
			c.snap.Selected = ctl.Selected
			c.snap.DetailTab = ctl.DetailTab
			c.snap.Scroll = ctl.Scroll
			c.mu.Unlock()
			c.refreshDerived(ctx, ctl.View, ctl.Selected)
			c.publish("selection")
		}
	}
}

// runSafetyPoll is the documented exception: state the daemon never pushes.
func (c *Collector) runSafetyPoll(ctx context.Context) {
	ticker := time.NewTicker(safetyPollInterval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			c.refreshAll(ctx, "safety-poll")
		}
	}
}

func (c *Collector) refreshAll(ctx context.Context, reason string) {
	if fleet, err := c.client.Fleet(ctx); err == nil {
		c.mu.Lock()
		c.snap.Fleet = fleet
		c.fleetSum = fleetSummary(fleet)
		c.mu.Unlock()
	}
	c.mu.Lock()
	view, selected := c.snap.View, c.snap.Selected
	c.mu.Unlock()
	c.refreshDerived(ctx, view, selected)
	c.publish(reason)
}

// refreshDerived fetches only what the ACTIVE view needs. Fetching all six
// views on every event would turn one push into six requests; the panel only
// ever shows one.
func (c *Collector) refreshDerived(ctx context.Context, view View, selected string) {
	switch view {
	case ViewOrchestrator:
		if tree, err := c.client.Tree(ctx); err == nil {
			c.mu.Lock()
			c.snap.Tree = tree
			c.mu.Unlock()
		}
		if exp, err := c.client.Exposure(ctx); err == nil {
			c.mu.Lock()
			c.snap.Exposure = exp
			c.mu.Unlock()
		}
	case ViewTemplates:
		if tpl, err := c.client.Templates(ctx); err == nil {
			c.mu.Lock()
			c.snap.Templates = tpl
			c.mu.Unlock()
		}
	case ViewModels:
		if m, err := c.client.Models(ctx); err == nil {
			c.mu.Lock()
			c.snap.Models = m
			c.mu.Unlock()
		}
	case ViewDetail, ViewMemory:
		if d, err := c.client.Detail(ctx, selected); err == nil {
			c.mu.Lock()
			c.snap.Detail = d
			c.mu.Unlock()
		}
	case ViewFleet:
		// Push-fed; nothing derived to fetch.
	}
}

func (c *Collector) setConnected(v bool) {
	c.mu.Lock()
	changed := c.snap.Connected != v
	c.snap.Connected = v
	c.mu.Unlock()
	if changed {
		c.publish("event")
	}
}

// publish writes the input file atomically. The host may read it at any moment,
// so it is never written in place.
func (c *Collector) publish(reason string) {
	c.mu.Lock()
	c.snap.Reason = reason
	c.snap.UpdatedUnix = time.Now().Unix()
	buf, err := json.Marshal(c.snap)
	c.mu.Unlock()
	if err != nil {
		return
	}
	if err := atomicWrite(c.inputPath(), buf); err != nil {
		log.Printf("harness-panel: write input: %v", err)
	}
}

func (c *Collector) readControl() (Control, error) {
	var ctl Control
	raw, err := os.ReadFile(c.controlPath())
	if err != nil {
		return ctl, err
	}
	if err := json.Unmarshal(raw, &ctl); err != nil {
		return ctl, err
	}
	if ctl.View == "" {
		ctl.View = ViewFleet
	}
	return ctl, nil
}

// fleetSummary is a cheap change key: the daemon re-sends the whole fleet on
// every push, and most pushes change nothing the panel derives from.
func fleetSummary(fleet []Harness) string {
	var b []byte
	for _, h := range fleet {
		b = append(b, h.Name...)
		b = append(b, '=')
		b = append(b, h.Status...)
		b = append(b, '/')
		b = append(b, h.SessionID...)
		b = append(b, ';')
	}
	return string(b)
}

func atomicWrite(path string, data []byte) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, 0o644); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

// sleepCtx reports false when the context ended first.
func sleepCtx(ctx context.Context, d time.Duration) bool {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
		return false
	case <-t.C:
		return true
	}
}
