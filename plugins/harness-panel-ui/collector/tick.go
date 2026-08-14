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
		// Held: the event loop is already running. Nothing to do.
		return nil
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
	return cmd.Process.Release()
}
