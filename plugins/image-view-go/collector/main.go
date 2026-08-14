// Process side of the image-view plugin: pick the newest cached image and
// measure it, writing `image_input.json` into the module's state dir.
//
// WHY this exists as a separate process at all: the wasm guest has no WASI and
// no host imports, so it cannot open a directory, cannot stat a file, and
// cannot read a PNG header. Somebody has to, and it must not be the host —
// putting "find the newest image in the agent cache" into herdr is precisely
// the host-side image feature this whole exercise removes. So it lives here, in
// the plugin, where it can be disabled by disabling the plugin.
//
// What crosses to the guest is a path and two integers. Never bytes.
//
// Go port of `plugins/image-view-ui/collector` (Rust). Behaviour is identical,
// including the exit codes and the stdout line, because the Rust twin stays
// linked and disabled for rollback and the two must be interchangeable.
package main

import (
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"time"
)

// imageInput is what the guest reads: a path and two integers.
type imageInput struct {
	Path   string `json:"path"`
	Width  uint32 `json:"width"`
	Height uint32 `json:"height"`
}

func main() {
	code, err := run(os.Args[1:], os.Stdout)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
	}
	os.Exit(code)
}

// run is separated from main so the whole flow is testable without a subprocess:
// it returns the exit code instead of calling os.Exit.
func run(args []string, stdout io.Writer) (int, error) {
	rootArg := ""
	if len(args) > 0 {
		rootArg = args[0]
	} else {
		rootArg = os.Getenv("IMAGE_VIEW_ROOT")
	}
	if rootArg == "" {
		return 2, errors.New("usage: image-view-collector <image-root-dir> [output.json]\n" +
			"       (or set IMAGE_VIEW_ROOT; output defaults to\n" +
			"        $HERDR_PLUGIN_STATE_DIR/image_input.json)")
	}
	root := expandTilde(rootArg)

	out := "image_input.json"
	switch {
	case len(args) > 1:
		out = args[1]
	case os.Getenv("HERDR_PLUGIN_STATE_DIR") != "":
		out = filepath.Join(os.Getenv("HERDR_PLUGIN_STATE_DIR"), "image_input.json")
	}

	newest, err := newestPNG(root)
	if err != nil {
		return 1, err
	}
	if newest == "" {
		return 1, fmt.Errorf("no png found under %s", root)
	}

	width, height, ok, err := pngDimensions(newest)
	if err != nil {
		return 1, err
	}
	if !ok {
		return 1, fmt.Errorf("not a png: %s", newest)
	}

	// Absolute, because the host requires it and because a relative path could
	// not be checked against graphics_roots in any meaningful way.
	absolute, err := filepath.Abs(newest)
	if err != nil {
		return 1, err
	}
	resolved, err := filepath.EvalSymlinks(absolute)
	if err != nil {
		return 1, err
	}

	input := imageInput{Path: resolved, Width: width, Height: height}
	encoded, err := json.MarshalIndent(input, "", "  ")
	if err != nil {
		return 1, err
	}
	// serde_json::to_vec_pretty ends without a trailing newline; the Rust twin's
	// output is byte-compared against this one in the parity gate.
	if err := writeAtomic(out, encoded); err != nil {
		return 1, err
	}
	// The snapshot is already on disk; a stdout write that fails (closed pipe,
	// full disk) does not make the run unsuccessful, but it is not swallowed
	// either — the host captures stderr into the plugin log.
	if _, err := fmt.Fprintf(stdout, "%s -> %s\n", input.Path, out); err != nil {
		fmt.Fprintf(os.Stderr, "image-view-collector: report line not written: %v\n", err) //nolint:errcheck // last resort
	}
	return 0, nil
}

func expandTilde(raw string) string {
	rest, found := cutPrefix(raw, "~/")
	if !found {
		return raw
	}
	home, ok := os.LookupEnv("HOME")
	if !ok {
		return raw
	}
	return filepath.Join(home, rest)
}

// cutPrefix is strings.CutPrefix, spelled out to keep the module at go1.22
// without pulling the dependency graph forward for one helper.
func cutPrefix(s, prefix string) (string, bool) {
	if len(s) >= len(prefix) && s[:len(prefix)] == prefix {
		return s[len(prefix):], true
	}
	return s, false
}

// newestPNG returns the newest `.png` directly under root or one level below it,
// by mtime, or "" when there is none. One level is enough for the agent caches
// this targets (`<session>/<n>.png`) and bounds the walk so a mistyped root
// cannot turn into a filesystem crawl.
func newestPNG(root string) (string, error) {
	var (
		bestPath string
		bestTime time.Time
	)
	// Newest wins; on an EXACT mtime tie the greater path wins.
	//
	// The tie is not hypothetical: an agent writing a burst of frames lands
	// several files inside one filesystem timestamp granule (measured: 20 files
	// across 2 distinct mtimes). The Rust twin resolves such a tie by whichever
	// entry readdir happened to yield first, which is inode order — so it can
	// pick a different image on two runs over the same unchanged directory, and
	// two machines never agree. An explicit rule costs one comparison and makes
	// the answer reproducible; caches number their frames upward, so the greater
	// name is also the more recent frame.
	consider := func(path string, info fs.FileInfo) {
		if filepath.Ext(path) != ".png" {
			return
		}
		switch {
		case bestPath == "":
		case info.ModTime().After(bestTime):
		case info.ModTime().Equal(bestTime) && path > bestPath:
		default:
			return
		}
		bestPath, bestTime = path, info.ModTime()
	}

	// An absent image root is the NORMAL case, not an error: the agent creates
	// the cache dir lazily, the first time an image is actually pasted. Left
	// propagating, this printed a bare `Error: Os { code: 2 }` — naming neither
	// the path nor the reason — on every single collector run for any user who
	// had not pasted an image yet, which is most of them.
	entries, err := readDirUnsorted(root)
	if errors.Is(err, fs.ErrNotExist) {
		return "", nil
	}
	if err != nil {
		return "", err
	}

	for _, entry := range entries {
		path := filepath.Join(root, entry.Name())
		if entry.IsDir() {
			// A subdirectory that vanishes between listing and descending is a
			// race with the agent writing the cache, not a failure of this run.
			nested, err := readDirUnsorted(path)
			if errors.Is(err, fs.ErrNotExist) {
				continue
			}
			if err != nil {
				return "", err
			}
			for _, child := range nested {
				if child.IsDir() {
					continue
				}
				info, err := child.Info()
				if errors.Is(err, fs.ErrNotExist) {
					continue
				}
				if err != nil {
					return "", err
				}
				consider(filepath.Join(path, child.Name()), info)
			}
			continue
		}
		info, err := entry.Info()
		if errors.Is(err, fs.ErrNotExist) {
			continue
		}
		if err != nil {
			return "", err
		}
		consider(path, info)
	}
	return bestPath, nil
}

// readDirUnsorted lists a directory WITHOUT os.ReadDir sorting pass. The
// tie-break here is explicit (see consider), so ordering the entries first buys
// nothing and this runs on every dash tick.
func readDirUnsorted(dir string) ([]fs.DirEntry, error) {
	file, err := os.Open(dir)
	if err != nil {
		return nil, err
	}
	defer file.Close() //nolint:errcheck // read-only handle
	return file.ReadDir(-1)
}

// pngSignature is the 8-byte magic every PNG starts with.
var pngSignature = [8]byte{0x89, 'P', 'N', 'G', 0x0d, 0x0a, 0x1a, 0x0a}

// pngDimensions reads width/height out of a PNG's IHDR without decoding the
// image.
//
// No image library on purpose: the only question is two big-endian uint32s at a
// fixed offset, and a decoder here would pull a large dependency into a plugin
// whose entire job is to avoid doing image work.
//
// `ok == false` means "this is not a PNG we can measure", which is a normal
// answer, not an error — a file that merely ends in `.png` must be refused
// rather than guessed at, or the host gets a layer whose geometry is fiction.
func pngDimensions(path string) (width, height uint32, ok bool, err error) {
	file, err := os.Open(path)
	if err != nil {
		return 0, 0, false, err
	}
	defer file.Close() //nolint:errcheck // read-only handle; close errors carry no information here

	var header [24]byte
	if _, err := io.ReadFull(file, header[:]); err != nil {
		// Too short to be a PNG with an IHDR — not an error, just not a PNG.
		return 0, 0, false, nil
	}
	if [8]byte(header[:8]) != pngSignature || string(header[12:16]) != "IHDR" {
		return 0, 0, false, nil
	}
	width = binary.BigEndian.Uint32(header[16:20])
	height = binary.BigEndian.Uint32(header[20:24])
	if width == 0 || height == 0 {
		return 0, 0, false, nil
	}
	return width, height, true, nil
}

// writeAtomic writes through a temp file and renames, so the host never reads a
// half-written file on the frame that happens to land mid-write.
func writeAtomic(path string, data []byte) error {
	if parent := filepath.Dir(path); parent != "" {
		if err := os.MkdirAll(parent, 0o755); err != nil {
			return err
		}
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, 0o644); err != nil {
		return err
	}
	if err := os.Rename(tmp, path); err != nil {
		// Leave nothing behind on the failure path; a stray .tmp beside the
		// input file is one more thing for the next reader to wonder about.
		_ = os.Remove(tmp)
		return err
	}
	return nil
}
