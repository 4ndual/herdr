package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
)

// runTick is the HOST-facing collector entry point.
//
// The host runs a module's declared `collector` once per dash tick, as a fresh
// process with a short budget. That model is polling by construction, which is
// exactly what this plugin's architecture avoids (see doc.go): a new process
// per tick means a new WebSocket handshake per tick, and the daemon's fleet
// pushes would be pointless.
//
// So the tick does not collect. It only ensures the long-lived event-loop
// collector is alive, then exits immediately. The event loop keeps the one
// WebSocket and owns every write to the input file; the tick is a supervisor,
// not a second writer. Declaring `collector` this way is also what wires the
// host's `ui_input_file` transport, which is why it is declared at all.
func runTick(_ []string) error {
	dir := stateDir()

	// The event loop holds this lock for its whole life. Taking it means no
	// collector is running; releasing it immediately and spawning one is safe
	// because the child re-takes it, and a second tick that loses the race
	// simply finds the lock held and does nothing.
	lock, err := os.OpenFile(filepath.Join(dir, "collector.lock"), os.O_CREATE|os.O_RDWR, 0o644)
	if err != nil {
		return err
	}
	defer func() { _ = lock.Close() }()

	if err := syscall.Flock(int(lock.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		// Held: the event loop is already running. Just relay its output.
		return emitSnapshot(dir)
	}
	_ = syscall.Flock(int(lock.Fd()), syscall.LOCK_UN)

	self, err := os.Executable()
	if err != nil {
		return err
	}
	cmd := exec.Command(self, "collect")
	cmd.Env = os.Environ()
	// Detach: the tick process is reaped by the host in seconds, and the event
	// loop must outlive it.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	cmd.Stdin, cmd.Stdout, cmd.Stderr = nil, nil, nil
	if err := cmd.Start(); err != nil {
		return err
	}
	if err := cmd.Process.Release(); err != nil {
		return err
	}
	return emitSnapshot(dir)
}

// emitSnapshot relays the event loop's latest snapshot on stdout, which is how
// the host actually transports a collector's output: it runs the binary each
// tick and parses stdout as
//
//	{"outcome":"replace","snapshot":{…}} | {"outcome":"keep"} | {"outcome":"clear"}
//
// (src/plugin_collector.rs). The event loop stays the source of truth; this only
// forwards what it last published. When the loop has not published yet, `keep`
// is the honest answer — it tells the host to hold whatever it already has
// instead of replacing live data with an empty snapshot.
func emitSnapshot(dir string) error {
	raw, err := os.ReadFile(filepath.Join(dir, "harness_panel_input.json"))
	if err != nil || len(raw) == 0 {
		_, err := os.Stdout.WriteString(`{"outcome":"keep"}`)
		return err
	}
	if _, err := os.Stdout.WriteString(`{"outcome":"replace","snapshot":`); err != nil {
		return err
	}
	if _, err := os.Stdout.Write(raw); err != nil {
		return err
	}
	_, err = os.Stdout.WriteString(`}`)
	return err
}
