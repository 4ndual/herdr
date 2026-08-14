package main

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"runtime"
	"testing"
	"time"

	"go.uber.org/goleak"
)

// TestMain is the goroutine-leak gate for the whole package: any goroutine this
// package starts and fails to join shows up here as a failure, which is the
// only automatic check that a long-lived collector actually shuts down.
func TestMain(m *testing.M) {
	goleak.VerifyTestMain(m)
}

// fakeDaemon serves the routes the collector uses, so the tests exercise the
// real HTTP client and the real JSON shapes rather than mocks of them.
func fakeDaemon(t *testing.T) (*httptest.Server, *int) {
	t.Helper()
	detailHits := 0
	mux := http.NewServeMux()
	mux.HandleFunc("/api/fleet", func(w http.ResponseWriter, _ *http.Request) {
		_ = json.NewEncoder(w).Encode([]Harness{
			{Name: "alpha", Status: "running", Runtime: "claude", Sessions: 2},
			{Name: "beta", Status: "stopped", Runtime: "codex"},
		})
	})
	mux.HandleFunc("/api/orchestrator/livetree", func(w http.ResponseWriter, _ *http.Request) {
		_ = json.NewEncoder(w).Encode(map[string]any{
			"nodes": []TreeNode{{Name: "alpha", Status: "running", Live: true}},
		})
	})
	mux.HandleFunc("/api/orchestrator/exposure", func(w http.ResponseWriter, _ *http.Request) {
		_, _ = w.Write([]byte(`{"alpha":["beta"]}`))
	})
	mux.HandleFunc("/api/templates", func(w http.ResponseWriter, _ *http.Request) {
		_ = json.NewEncoder(w).Encode([]Template{{Name: "default", Version: "4"}})
	})
	mux.HandleFunc("/api/models", func(w http.ResponseWriter, _ *http.Request) {
		_, _ = w.Write([]byte(`{"claude":["opus"]}`))
	})
	mux.HandleFunc("/api/harness/alpha", func(w http.ResponseWriter, _ *http.Request) {
		detailHits++
		_ = json.NewEncoder(w).Encode(Detail{Name: "alpha", Status: "running", LiveRules: "be brief"})
	})
	mux.HandleFunc("/api/harness/alpha/spawn", func(w http.ResponseWriter, _ *http.Request) {
		_, _ = w.Write([]byte(`{"ok":true,"out":"spawned"}`))
	})
	srv := httptest.NewServer(mux)
	t.Cleanup(srv.Close)
	return srv, &detailHits
}

func newTestCollector(t *testing.T) (*Collector, *httptest.Server, *int) {
	t.Helper()
	srv, hits := fakeDaemon(t)
	dir := t.TempDir()
	return NewCollector(NewClient(srv.URL), dir), srv, hits
}

func TestRefreshAllPopulatesTheFleetAndPublishes(t *testing.T) {
	c, _, _ := newTestCollector(t)
	c.refreshAll(context.Background(), "test")

	raw, err := os.ReadFile(c.inputPath())
	if err != nil {
		t.Fatalf("input file not written: %v", err)
	}
	var snap Snapshot
	if err := json.Unmarshal(raw, &snap); err != nil {
		t.Fatalf("input file is not a snapshot: %v", err)
	}
	if len(snap.Fleet) != 2 || snap.Fleet[0].Name != "alpha" {
		t.Fatalf("fleet not populated: %+v", snap.Fleet)
	}
	if snap.Reason != "test" || snap.UpdatedUnix == 0 {
		t.Fatalf("snapshot provenance missing: reason=%q updated=%d", snap.Reason, snap.UpdatedUnix)
	}
}

// A fleet frame is the primary path; it must land in the snapshot without any
// timer having fired.
func TestFleetFrameUpdatesWithoutPolling(t *testing.T) {
	c, _, _ := newTestCollector(t)
	frame := []byte(`{"type":"fleet","data":[{"name":"gamma","status":"running"}]}`)
	c.handleFrame(context.Background(), frame)

	c.mu.Lock()
	defer c.mu.Unlock()
	if len(c.snap.Fleet) != 1 || c.snap.Fleet[0].Name != "gamma" {
		t.Fatalf("frame not applied: %+v", c.snap.Fleet)
	}
	if c.snap.Reason != "event" {
		t.Fatalf("reason should record the push, got %q", c.snap.Reason)
	}
}

func TestNonFleetFramesAreIgnored(t *testing.T) {
	c, _, _ := newTestCollector(t)
	c.handleFrame(context.Background(), []byte(`{"type":"noise","data":{}}`))
	c.handleFrame(context.Background(), []byte(`not json`))
	c.mu.Lock()
	defer c.mu.Unlock()
	if len(c.snap.Fleet) != 0 {
		t.Fatalf("unexpected fleet mutation: %+v", c.snap.Fleet)
	}
}

// Only the active view's data is fetched: showing the fleet must not pull the
// harness detail, or one push becomes six requests.
func TestOnlyTheActiveViewIsFetched(t *testing.T) {
	c, _, detailHits := newTestCollector(t)
	ctx := context.Background()

	c.refreshDerived(ctx, ViewFleet, "alpha")
	if *detailHits != 0 {
		t.Fatalf("fleet view fetched the detail endpoint %d times", *detailHits)
	}
	c.refreshDerived(ctx, ViewDetail, "alpha")
	if *detailHits != 1 {
		t.Fatalf("detail view should fetch exactly once, got %d", *detailHits)
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.snap.Detail == nil || c.snap.Detail.LiveRules != "be brief" {
		t.Fatalf("detail not stored: %+v", c.snap.Detail)
	}
}

func TestOrchestratorViewFetchesTreeAndExposure(t *testing.T) {
	c, _, _ := newTestCollector(t)
	c.refreshDerived(context.Background(), ViewOrchestrator, "")
	c.mu.Lock()
	defer c.mu.Unlock()
	if len(c.snap.Tree) != 1 || c.snap.Tree[0].Name != "alpha" {
		t.Fatalf("tree not fetched: %+v", c.snap.Tree)
	}
	if len(c.snap.Exposure) == 0 {
		t.Fatal("exposure not fetched")
	}
}

func TestFleetSummaryDetectsOnlyMeaningfulChange(t *testing.T) {
	a := []Harness{{Name: "x", Status: "running", SessionID: "s1"}}
	b := []Harness{{Name: "x", Status: "running", SessionID: "s1", Ctx: floatPtr(0.5)}}
	cc := []Harness{{Name: "x", Status: "stopped", SessionID: "s1"}}
	if fleetSummary(a) != fleetSummary(b) {
		t.Fatal("a ctx-only change must not count as a fleet change")
	}
	if fleetSummary(a) == fleetSummary(cc) {
		t.Fatal("a status change must count as a fleet change")
	}
}

func TestRunStopsCleanlyOnCancel(t *testing.T) {
	c, _, _ := newTestCollector(t)
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() { defer close(done); _ = c.Run(ctx) }()
	// Let the loops start and the eager fetch land.
	time.Sleep(300 * time.Millisecond)
	cancel()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("Run did not return within 5s of cancellation")
	}
	// goleak (TestMain) asserts nothing was left running.
}

func TestControlFileRoundTrip(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "ctl.json")
	want := Control{View: ViewOrchestrator, Selected: "alpha", DetailTab: "rules", Scroll: 3, Nonce: 42}
	if err := writeControlFile(path, want); err != nil {
		t.Fatal(err)
	}
	got, err := readControlFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got != want {
		t.Fatalf("round trip changed the control: %+v vs %+v", got, want)
	}
}

func TestAtomicWriteLeavesNoPartialFile(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "sub", "out.json")
	if err := atomicWrite(path, []byte(`{"a":1}`)); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(path + ".tmp"); !os.IsNotExist(err) {
		t.Fatal("temp file survived the rename")
	}
	raw, err := os.ReadFile(path)
	if err != nil || string(raw) != `{"a":1}` {
		t.Fatalf("bad content: %q err=%v", raw, err)
	}
}

// Heap-flat soak: the collector runs for thousands of events and must not grow
// its live heap. A snapshot rebuilt per event is exactly the shape that leaks if
// anything accumulates, so this is the check that it does not.
func TestHeapStaysFlatUnderSustainedEvents(t *testing.T) {
	if testing.Short() {
		t.Skip("soak skipped under -short")
	}
	c, _, _ := newTestCollector(t)
	ctx := context.Background()
	frame := []byte(`{"type":"fleet","data":[{"name":"alpha","status":"running","sessionId":"s1"},{"name":"beta","status":"stopped"}]}`)

	// Warm up so first-touch allocations are not counted as growth.
	for i := 0; i < 500; i++ {
		c.handleFrame(ctx, frame)
	}
	runtime.GC()
	var before runtime.MemStats
	runtime.ReadMemStats(&before)

	const iterations = 20_000
	for i := 0; i < iterations; i++ {
		c.handleFrame(ctx, frame)
	}
	runtime.GC()
	var after runtime.MemStats
	runtime.ReadMemStats(&after)

	growth := int64(after.HeapAlloc) - int64(before.HeapAlloc)
	// Allow a small absolute slack for allocator noise; the point is that growth
	// is not proportional to the number of events.
	const slack = 512 << 10
	if growth > slack {
		t.Fatalf("heap grew %d bytes over %d events (slack %d) — something accumulates per event",
			growth, iterations, slack)
	}
}

func floatPtr(f float64) *float64 { return &f }
