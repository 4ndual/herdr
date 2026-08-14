package meta

import (
	"log"
	"os"
)

// Debugf writes to $WSOPS_DEBUG when set.
//
// The overlay owns the terminal and actions are spawned detached, so neither
// has anywhere to print: without a file, "the click did nothing" is
// unfalsifiable. Off unless the variable is set, so it costs nothing normally.
func Debugf(format string, args ...any) {
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
