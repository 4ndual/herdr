package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"log"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"syscall"
	"time"
)

const defaultDaemon = "http://127.0.0.1:7877"

func main() {
	log.SetFlags(0)
	log.SetPrefix("harness-panel: ")

	if len(os.Args) < 2 {
		usage()
		os.Exit(2)
	}
	mode := os.Args[1]
	args := os.Args[2:]

	if err := run(mode, args); err != nil {
		log.Print(err)
		os.Exit(1)
	}
}

func usage() {
	fmt.Fprint(os.Stderr, `usage:
  harness-panel-collector collect [-daemon URL] [-state DIR]
  harness-panel-collector op -action ACTION [-target NAME] [-view VIEW] [-tab TAB] [-scroll N]

  collect  long-lived; holds the daemon WebSocket and writes harness_panel_input.json
  op       one-shot; applies a view/selection change and/or a fleet mutation

Actions understood by op:
  select     set the selected harness (and switch to the detail view)
  view       switch view (fleet|detail|orchestrator|templates|models|memory)
  tab        switch the detail tab
  scroll     set the scroll offset
  spawn      start the target harness      (daemon mutation)
  stop       stop the target harness       (daemon mutation)
  refresh    refresh the target harness    (daemon mutation)
`)
}

func run(mode string, args []string) error {
	switch mode {
	case "collect":
		return runCollect(args)
	case "op":
		return runOp(args)
	case "-h", "--help", "help":
		usage()
		return nil
	default:
		usage()
		return fmt.Errorf("unknown mode %q", mode)
	}
}

func runCollect(args []string) error {
	fs := flag.NewFlagSet("collect", flag.ContinueOnError)
	daemon := fs.String("daemon", envOr("HARNESS_PANEL_DAEMON", defaultDaemon), "fleet daemon base URL")
	state := fs.String("state", stateDir(), "plugin state dir")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if *state == "" {
		return errors.New("no state dir: set HERDR_PLUGIN_STATE_DIR or pass -state")
	}

	// A second collector would fight the first over the input file; the lock is
	// how the startup hook can fire on every herdr start without stacking them.
	release, err := acquireLock(filepath.Join(*state, "collector.lock"))
	if err != nil {
		return err
	}
	defer release()

	ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer cancel()

	c := NewCollector(NewClient(*daemon), *state)
	log.Printf("collecting from %s into %s", *daemon, *state)
	err = c.Run(ctx)
	if errors.Is(err, context.Canceled) {
		return nil
	}
	return err
}

func runOp(args []string) error {
	fs := flag.NewFlagSet("op", flag.ContinueOnError)
	daemon := fs.String("daemon", envOr("HARNESS_PANEL_DAEMON", defaultDaemon), "fleet daemon base URL")
	state := fs.String("state", stateDir(), "plugin state dir")
	action := fs.String("action", "", "action to apply")
	target := fs.String("target", "", "harness name for a mutation or selection")
	view := fs.String("view", "", "view to switch to")
	tab := fs.String("tab", "", "detail tab to switch to")
	scroll := fs.Int("scroll", -1, "scroll offset")
	if err := fs.Parse(args); err != nil {
		return err
	}

	// The guest sends its payload as JSON on stdin when the action carries more
	// than flags can express; flags win when both are present.
	if fromPayload, err := payloadFromEnv(); err == nil && fromPayload != nil {
		if *action == "" {
			*action = fromPayload.Action
		}
		if *target == "" {
			*target = fromPayload.Target
		}
		if *view == "" {
			*view = fromPayload.View
		}
		if *tab == "" {
			*tab = fromPayload.Tab
		}
		if *scroll < 0 && fromPayload.Scroll > 0 {
			*scroll = fromPayload.Scroll
		}
	}
	if *action == "" {
		return errors.New("op needs -action")
	}
	if *state == "" {
		return errors.New("no state dir: set HERDR_PLUGIN_STATE_DIR or pass -state")
	}

	ctl, err := readControlFile(filepath.Join(*state, "harness_panel_control.json"))
	if err != nil {
		ctl = Control{View: ViewFleet}
	}

	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	client := NewClient(*daemon)

	var opResult *OpResult
	switch *action {
	case "select":
		ctl.Selected = *target
		ctl.View = ViewDetail
		ctl.Scroll = 0
	case "view":
		if *view != "" {
			ctl.View = View(*view)
			ctl.Scroll = 0
		}
	case "tab":
		ctl.DetailTab = *tab
		ctl.Scroll = 0
	case "scroll":
		if *scroll >= 0 {
			ctl.Scroll = *scroll
		}
	case "spawn", "stop", "refresh":
		out, err := client.Mutate(ctx, *target, *action, map[string]any{})
		opResult = &OpResult{
			Action: *action, Target: *target, OK: err == nil, AtUnix: time.Now().Unix(),
			Message: firstLine(pick(err, out)),
		}
		if err != nil {
			log.Printf("%s %s failed: %v", *action, *target, err)
		}
	default:
		return fmt.Errorf("unknown action %q", *action)
	}

	ctl.Nonce = time.Now().UnixNano()
	if err := writeControlFile(filepath.Join(*state, "harness_panel_control.json"), ctl); err != nil {
		return err
	}
	// Record the mutation outcome where the guest can render it. The collector
	// owns the snapshot, so this is a sidecar file it folds in rather than a
	// racing write into the snapshot itself.
	if opResult != nil {
		buf, _ := json.Marshal(opResult)
		_ = atomicWrite(filepath.Join(*state, "harness_panel_lastop.json"), buf)
	}
	return nil
}

type opPayload struct {
	Action string `json:"action"`
	Target string `json:"target"`
	View   string `json:"view"`
	Tab    string `json:"tab"`
	Scroll int    `json:"scroll"`
}

// payloadFromEnv reads HERDR_PLUGIN_PAYLOAD_JSON, which is how the host hands a
// guest's action payload to the process it spawns.
func payloadFromEnv() (*opPayload, error) {
	raw := os.Getenv("HERDR_PLUGIN_PAYLOAD_JSON")
	if raw == "" {
		return nil, nil
	}
	var p opPayload
	if err := json.Unmarshal([]byte(raw), &p); err != nil {
		return nil, err
	}
	return &p, nil
}

func readControlFile(path string) (Control, error) {
	var ctl Control
	raw, err := os.ReadFile(path)
	if err != nil {
		return ctl, err
	}
	return ctl, json.Unmarshal(raw, &ctl)
}

func writeControlFile(path string, ctl Control) error {
	buf, err := json.Marshal(ctl)
	if err != nil {
		return err
	}
	return atomicWrite(path, buf)
}

// acquireLock takes an exclusive advisory lock, returning a release func.
func acquireLock(path string) (func(), error) {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return nil, err
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR, 0o644)
	if err != nil {
		return nil, err
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		_ = f.Close()
		return nil, fmt.Errorf("another collector already holds %s", path)
	}
	if _, err := f.WriteString(strconv.Itoa(os.Getpid())); err != nil {
		_ = f.Close()
		return nil, err
	}
	return func() {
		_ = syscall.Flock(int(f.Fd()), syscall.LOCK_UN)
		_ = f.Close()
	}, nil
}

func stateDir() string {
	if d := os.Getenv("HERDR_PLUGIN_STATE_DIR"); d != "" {
		return d
	}
	// Match herdr's own layout so a manually started collector writes where the
	// host reads: $XDG_STATE_HOME/herdr/plugins/<plugin id>.
	base := os.Getenv("XDG_STATE_HOME")
	if base == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return ""
		}
		base = filepath.Join(home, ".local", "state")
	}
	return filepath.Join(base, "herdr", "plugins", "builtin.harness-panel")
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

func pick(err error, out string) string {
	if err != nil {
		return err.Error()
	}
	return out
}

func firstLine(s string) string {
	for i, r := range s {
		if r == '\n' {
			return s[:i]
		}
	}
	if len(s) > 300 {
		return s[:300]
	}
	return s
}
