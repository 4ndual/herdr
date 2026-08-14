package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"testing"
	"time"

	"go.uber.org/goleak"
)

// The collector is a one-shot process, so a goroutine outliving a run is always
// a bug — it would mean work still in flight when the host reads the snapshot.
func TestMain(m *testing.M) {
	goleak.VerifyTestMain(m)
}

func tempDir(t *testing.T, name string) string {
	t.Helper()
	dir := filepath.Join(t.TempDir(), name)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatalf("mkdir %s: %v", dir, err)
	}
	return dir
}

// writePNG writes a minimal but real PNG header: signature, length, `IHDR`,
// then the two dimensions the collector reads.
func writePNG(t *testing.T, path string, width, height uint32) {
	t.Helper()
	var bytesOut []byte
	bytesOut = append(bytesOut, 0x89, 'P', 'N', 'G', 0x0d, 0x0a, 0x1a, 0x0a)
	bytesOut = binary.BigEndian.AppendUint32(bytesOut, 13)
	bytesOut = append(bytesOut, []byte("IHDR")...)
	bytesOut = binary.BigEndian.AppendUint32(bytesOut, width)
	bytesOut = binary.BigEndian.AppendUint32(bytesOut, height)
	if err := os.WriteFile(path, bytesOut, 0o644); err != nil {
		t.Fatalf("write png %s: %v", path, err)
	}
}

func TestDimensionsComeFromTheIHDRHeader(t *testing.T) {
	dir := tempDir(t, "ihdr")
	png := filepath.Join(dir, "a.png")
	writePNG(t, png, 1280, 720)

	width, height, ok, err := pngDimensions(png)
	if err != nil {
		t.Fatalf("pngDimensions: %v", err)
	}
	if !ok || width != 1280 || height != 720 {
		t.Fatalf("got (%d, %d, %v), want (1280, 720, true)", width, height, ok)
	}
}

// A file that merely ends in `.png` is not a PNG. Reporting dimensions for it
// would hand the host a layer whose geometry is fiction.
func TestANonPNGIsRefusedRatherThanGuessed(t *testing.T) {
	dir := tempDir(t, "notpng")

	fake := filepath.Join(dir, "a.png")
	if err := os.WriteFile(fake, []byte("this is not a png at all, not even close"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, _, ok, err := pngDimensions(fake); err != nil || ok {
		t.Fatalf("fake png: got ok=%v err=%v, want ok=false err=nil", ok, err)
	}

	truncated := filepath.Join(dir, "b.png")
	if err := os.WriteFile(truncated, []byte("\x89PNG"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, _, ok, err := pngDimensions(truncated); err != nil || ok {
		t.Fatalf("truncated png: got ok=%v err=%v, want ok=false err=nil", ok, err)
	}
}

// A zero dimension is refused for the same reason a non-PNG is: a 0-wide layer
// is not a smaller picture, it is a broken one.
func TestZeroDimensionsAreRefused(t *testing.T) {
	dir := tempDir(t, "zero")
	png := filepath.Join(dir, "a.png")
	writePNG(t, png, 0, 480)
	if _, _, ok, err := pngDimensions(png); err != nil || ok {
		t.Fatalf("got ok=%v err=%v, want ok=false err=nil", ok, err)
	}
}

func TestTheNewestPNGWinsIncludingOneLevelDown(t *testing.T) {
	dir := tempDir(t, "newest")
	nested := filepath.Join(dir, "session")
	if err := os.MkdirAll(nested, 0o755); err != nil {
		t.Fatal(err)
	}

	old := filepath.Join(dir, "old.png")
	writePNG(t, old, 1, 1)
	newer := filepath.Join(nested, "new.png")
	writePNG(t, newer, 2, 2)

	// Make the ordering explicit rather than relying on write order.
	longAgo := time.Now().Add(-time.Hour)
	if err := os.Chtimes(old, longAgo, longAgo); err != nil {
		t.Fatal(err)
	}

	got, err := newestPNG(dir)
	if err != nil {
		t.Fatalf("newestPNG: %v", err)
	}
	if got != newer {
		t.Fatalf("got %q, want %q", got, newer)
	}
}

// The state this plugin is in for most users most of the time: the agent has not
// pasted an image yet, so it has never created the cache dir. It must be
// indistinguishable from "the dir is there and empty" — both are "nothing to
// show", not a failure. This previously escaped as a bare `Error: Os { code: 2 }`
// naming neither the path nor the reason.
func TestAnAbsentImageRootIsNothingToShowNotAnError(t *testing.T) {
	missing := filepath.Join(tempDir(t, "absent"), "never-created")
	if _, err := os.Stat(missing); !os.IsNotExist(err) {
		t.Fatalf("fixture is not absent: %v", err)
	}
	fromMissing, err := newestPNG(missing)
	if err != nil {
		t.Fatalf("absent root returned an error: %v", err)
	}
	if fromMissing != "" {
		t.Fatalf("got %q, want empty", fromMissing)
	}

	// Same answer as the present-but-empty directory, deliberately.
	fromEmpty, err := newestPNG(tempDir(t, "absent-empty"))
	if err != nil {
		t.Fatalf("empty root returned an error: %v", err)
	}
	if fromEmpty != fromMissing {
		t.Fatalf("absent gave %q but empty gave %q; they must agree", fromMissing, fromEmpty)
	}
}

func TestNonPNGFilesAreIgnoredEntirely(t *testing.T) {
	dir := tempDir(t, "ignore")
	if err := os.WriteFile(filepath.Join(dir, "notes.txt"), []byte("hello"), 0o644); err != nil {
		t.Fatal(err)
	}
	got, err := newestPNG(dir)
	if err != nil {
		t.Fatalf("newestPNG: %v", err)
	}
	if got != "" {
		t.Fatalf("got %q, want empty", got)
	}
}

// Beyond the ported Rust tests: the end-to-end flow, including the exit codes
// the host distinguishes and the JSON the guest parses.
func TestRunWritesTheSnapshotAndReportsIt(t *testing.T) {
	dir := tempDir(t, "run")
	png := filepath.Join(dir, "shot.png")
	writePNG(t, png, 640, 480)
	out := filepath.Join(t.TempDir(), "state", "image_input.json")

	var stdout bytes.Buffer
	code, err := run(context.Background(), []string{dir, out}, &stdout)
	if err != nil || code != 0 {
		t.Fatalf("run: code=%d err=%v", code, err)
	}

	written, err := os.ReadFile(out)
	if err != nil {
		t.Fatalf("read snapshot: %v", err)
	}
	want := "{\n  \"path\": " + quote(mustEval(t, png)) + ",\n  \"width\": 640,\n  \"height\": 480\n}"
	if string(written) != want {
		t.Fatalf("snapshot bytes differ:\n got: %s\nwant: %s", written, want)
	}
	if !bytes.Contains(stdout.Bytes(), []byte(" -> "+out+"\n")) {
		t.Fatalf("stdout %q does not report the destination", stdout.String())
	}
	// No .tmp left behind for the next reader to wonder about.
	if _, err := os.Stat(out + ".tmp"); !os.IsNotExist(err) {
		t.Fatalf("temp file survived: %v", err)
	}
}

func TestRunWithoutARootIsAUsageError(t *testing.T) {
	t.Setenv("IMAGE_VIEW_ROOT", "")
	code, err := run(context.Background(), nil, &bytes.Buffer{})
	if code != 2 || err == nil {
		t.Fatalf("got code=%d err=%v, want code=2 and a usage error", code, err)
	}
}

// An empty root exits 1 with a message naming the path — the host logs it, and
// "no png found under <path>" is the whole diagnosis.
func TestRunWithNoImagesExitsOneNamingThePath(t *testing.T) {
	dir := tempDir(t, "empty-run")
	code, err := run(context.Background(), []string{dir, filepath.Join(t.TempDir(), "out.json")}, &bytes.Buffer{})
	if code != 1 || err == nil {
		t.Fatalf("got code=%d err=%v, want code=1 with an error", code, err)
	}
	if !bytes.Contains([]byte(err.Error()), []byte(dir)) {
		t.Fatalf("error %q does not name the root", err)
	}
}

func TestExpandTildeOnlyAtAPathBoundary(t *testing.T) {
	t.Setenv("HOME", "/home/x")
	if got := expandTilde("~/p"); got != "/home/x/p" {
		t.Fatalf("got %q, want /home/x/p", got)
	}
	// `~foo` is a username reference, not this user's home.
	if got := expandTilde("~foo/p"); got != "~foo/p" {
		t.Fatalf("got %q, want it unchanged", got)
	}
	if got := expandTilde("/abs"); got != "/abs" {
		t.Fatalf("got %q, want /abs", got)
	}
}

func quote(s string) string {
	encoded, err := json.Marshal(s)
	if err != nil {
		panic(err)
	}
	return string(encoded)
}

func mustEval(t *testing.T, path string) string {
	t.Helper()
	absolute, err := filepath.Abs(path)
	if err != nil {
		t.Fatal(err)
	}
	resolved, err := filepath.EvalSymlinks(absolute)
	if err != nil {
		t.Fatal(err)
	}
	return resolved
}

// An mtime tie must resolve the same way every run. The Rust twin leaves this
// to readdir order, so it can answer differently on two runs over the same
// unchanged directory; this pins the Go rule instead of inheriting that.
func TestAnMtimeTieResolvesDeterministically(t *testing.T) {
	dir := tempDir(t, "tie")
	same := time.Now().Add(-time.Minute)
	for _, name := range []string{"b1.png", "b20.png", "b3.png"} {
		path := filepath.Join(dir, name)
		writePNG(t, path, 10, 10)
		if err := os.Chtimes(path, same, same); err != nil {
			t.Fatal(err)
		}
	}

	first, err := newestPNG(dir)
	if err != nil {
		t.Fatal(err)
	}
	if want := filepath.Join(dir, "b3.png"); first != want {
		t.Fatalf("got %q, want %q (greater path wins a tie)", first, want)
	}
	// Same directory, same answer — twice, because "stable" is the claim.
	for n := 0; n < 5; n++ {
		again, err := newestPNG(dir)
		if err != nil {
			t.Fatal(err)
		}
		if again != first {
			t.Fatalf("run %d gave %q, first run gave %q", n, again, first)
		}
	}

	// A strictly newer file still beats the tie group, whatever its name.
	newer := filepath.Join(dir, "a0.png")
	writePNG(t, newer, 10, 10)
	if err := os.Chtimes(newer, time.Now(), time.Now()); err != nil {
		t.Fatal(err)
	}
	got, err := newestPNG(dir)
	if err != nil {
		t.Fatal(err)
	}
	if got != newer {
		t.Fatalf("got %q, want the strictly newer %q", got, newer)
	}
}

// ── event-loop core ────────────────────────────────────────────────────────

// The whole point of the shim architecture: the tick path and the event path
// must produce the same bytes, or the "one-line migration" is a rewrite in
// disguise and the gates only ever tested one of them.
func TestTickShimAndEventLoopProduceIdenticalSnapshots(t *testing.T) {
	dir := tempDir(t, "equiv")
	writePNG(t, filepath.Join(dir, "a.png"), 320, 240)

	tickOut := filepath.Join(t.TempDir(), "tick.json")
	var tickStdout bytes.Buffer
	code, err := run(context.Background(), []string{dir, tickOut}, &tickStdout)
	if err != nil || code != 0 {
		t.Fatalf("tick shim: code=%d err=%v", code, err)
	}

	loopOut := filepath.Join(t.TempDir(), "loop.json")
	var loopStdout bytes.Buffer
	core := &Core{Root: dir, Out: loopOut, Stdout: &loopStdout}
	events := make(chan Event, 1)
	events <- Event{Reason: "test", At: time.Now()}
	close(events)
	if err := core.RunLoop(context.Background(), events, nil); err != nil {
		t.Fatalf("event loop: %v", err)
	}

	tickBytes, err := os.ReadFile(tickOut)
	if err != nil {
		t.Fatal(err)
	}
	loopBytes, err := os.ReadFile(loopOut)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(tickBytes, loopBytes) {
		t.Fatalf("paths diverge:\n tick: %s\n loop: %s", tickBytes, loopBytes)
	}
	if tickStdout.String() == "" || loopStdout.String() == "" {
		t.Fatal("both paths must report on stdout")
	}
}

// A cancelled context must stop the loop immediately, not after one more event.
// This is what makes shutdown clean in a long-lived process, and goleak in
// TestMain is what proves nothing survives it.
func TestRunLoopStopsOnCancellation(t *testing.T) {
	dir := tempDir(t, "cancel")
	writePNG(t, filepath.Join(dir, "a.png"), 10, 10)
	core := &Core{Root: dir, Out: filepath.Join(t.TempDir(), "o.json"), Stdout: &bytes.Buffer{}}

	ctx, cancel := context.WithCancel(context.Background())
	events := make(chan Event) // unbuffered and never fed: only cancel can end this
	done := make(chan error, 1)
	go func() { done <- core.RunLoop(ctx, events, nil) }()

	cancel()
	select {
	case err := <-done:
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("got %v, want context.Canceled", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("RunLoop did not return within 2s of cancellation")
	}
}

// A bad event must not kill a long-lived collector: an empty cache is normal,
// and exiting would freeze the snapshot until someone restarted the process.
func TestRunLoopSurvivesABadEvent(t *testing.T) {
	empty := tempDir(t, "survive-empty")
	core := &Core{Root: empty, Out: filepath.Join(t.TempDir(), "o.json"), Stdout: &bytes.Buffer{}}

	var seen []error
	events := make(chan Event, 2)
	events <- Event{Reason: "empty-cache"}
	events <- Event{Reason: "still-empty"}
	close(events)

	if err := core.RunLoop(context.Background(), events, func(err error) { seen = append(seen, err) }); err != nil {
		t.Fatalf("loop returned %v; a bad event must not end it", err)
	}
	if len(seen) != 2 {
		t.Fatalf("got %d errors, want 2 — every bad event must be reported, not swallowed", len(seen))
	}
}

// The handler must hold no state between events: a long-lived process that
// accumulates is how a bounded job becomes a leak. Same core, many events,
// byte-identical output every time.
func TestHandlerIsStatelessAcrossManyEvents(t *testing.T) {
	dir := tempDir(t, "stateless")
	writePNG(t, filepath.Join(dir, "a.png"), 64, 48)
	out := filepath.Join(t.TempDir(), "o.json")
	core := &Core{Root: dir, Out: out, Stdout: io.Discard}

	var first []byte
	for n := 0; n < 500; n++ {
		if err := core.HandleEvent(context.Background(), Event{Reason: "loop"}); err != nil {
			t.Fatalf("event %d: %v", n, err)
		}
		current, err := os.ReadFile(out)
		if err != nil {
			t.Fatal(err)
		}
		if n == 0 {
			first = current
			continue
		}
		if !bytes.Equal(first, current) {
			t.Fatalf("event %d produced different bytes than event 0", n)
		}
	}
}
