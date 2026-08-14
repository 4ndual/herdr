// Event-handling core, shared by both entrypoints.
//
// ARCHITECTURE (operator mandate, 2026-08-14): no polling. This collector is
// written as a long-lived event loop — a context, a cancellable watcher, and a
// handler that runs once per event — even though the host currently drives
// collectors by re-exec'ing them on a ~1 Hz tick.
//
// The tick entrypoint (`main.go`) is therefore a THIN SHIM: it constructs the
// same `Core` and calls `HandleEvent` exactly once, then exits. When the host's
// event-driven collector mode lands, `RunLoop` below becomes the entrypoint and
// nothing in the resolution logic changes. That is the one-line migration the
// mandate asks for, and it is why the work lives here rather than in `main`.
//
// The loop watches the image cache with fsnotify rather than re-listing it:
// re-listing on an interval is polling wearing a different hat.
package main

import (
	"context"
	"fmt"
	"io"
	"time"
)

// Core is everything a run needs, resolved once. It holds no mutable state
// between events on purpose: an event handler that accumulates is how a
// long-lived process turns a bounded job into a leak.
type Core struct {
	Root   string
	Out    string
	Stdout io.Writer
}

// Event is what wakes the core. The tick shim synthesises one; the event loop
// will translate a filesystem notification or a host event into the same shape,
// so the handler cannot tell them apart and neither can its tests.
type Event struct {
	// Reason is for the log line only. It never changes what the handler does —
	// a handler that behaves differently per trigger is two handlers.
	Reason string
	At     time.Time
}

// HandleEvent resolves the newest image and publishes the snapshot. It is the
// ONLY place that does work, so both entrypoints are provably equivalent.
func (c *Core) HandleEvent(ctx context.Context, event Event) error {
	// Honour cancellation before doing filesystem work: on the event path this
	// is what makes shutdown immediate rather than one-event-late.
	if err := ctx.Err(); err != nil {
		return err
	}

	newest, err := newestPNG(c.Root)
	if err != nil {
		return err
	}
	if newest == "" {
		return fmt.Errorf("no png found under %s", c.Root)
	}

	width, height, ok, err := pngDimensions(newest)
	if err != nil {
		return err
	}
	if !ok {
		return fmt.Errorf("not a png: %s", newest)
	}

	resolved, err := absoluteResolved(newest)
	if err != nil {
		return err
	}

	input := imageInput{Path: resolved, Width: width, Height: height}
	encoded, err := marshalSnapshot(input)
	if err != nil {
		return err
	}
	if err := writeAtomic(c.Out, encoded); err != nil {
		return err
	}
	if _, err := fmt.Fprintf(c.Stdout, "%s -> %s\n", input.Path, c.Out); err != nil {
		return fmt.Errorf("report line not written: %w", err)
	}
	return nil
}

// RunLoop is the event-driven entrypoint, for the host mode landing tonight.
//
// It is deliberately written now, against the same core the tick shim uses, so
// the migration is a call-site change rather than a rewrite. `events` is the
// caller's channel — a host event subscription, an fsnotify watcher, or a test
// — and the loop owns nothing it did not create.
//
// Every exit path is a cancellation or a closed channel: there is no timer, no
// sleep and no re-list, so an idle collector costs nothing and a stopped one
// leaves no goroutine behind (asserted by goleak in the tests).
func (c *Core) RunLoop(ctx context.Context, events <-chan Event, onError func(error)) error {
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case event, open := <-events:
			if !open {
				return nil
			}
			if err := c.HandleEvent(ctx, event); err != nil {
				// A single bad event must not kill a long-lived collector: the
				// cache being momentarily empty is normal, and exiting would
				// mean the snapshot stops updating until someone restarts it.
				if onError != nil {
					onError(err)
				}
			}
		}
	}
}
