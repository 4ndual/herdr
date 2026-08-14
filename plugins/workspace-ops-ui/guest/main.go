// The workspace-ops guest: it draws nothing and exists only to register this
// plugin's context-menu items.
//
// Context-menu entries can ONLY come from a wasm guest publishing a
// `context_menu` surface (`ui_plugins.rs::context_menu_items_from_update`);
// `[[actions]]` alone never reach a menu. The items are constant, so they ship
// in the STATIC update — the guest never parses an event, never allocates, and
// therefore cannot leak or trap.
//
// Build (both rules are load-bearing, proven by worker-m's pipeline):
//
//		tinygo build -target=wasm-unknown -gc=conservative -opt=z \
//		  -o target/workspace_ops_ui.wasm ./guest
//
//	  - `//export`, never `//go:wasmexport`: Go 1.24's directive wraps exports in
//	    a runtime-init guard, and since the host never calls `_initialize`, every
//	    export traps immediately.
//	  - `-gc=conservative`, never the default: `wasm-unknown` ships gc=leaking,
//	    which dies after ~15 frames — and fails SILENTLY, reporting
//	    `loaded: true, last_error: none` while the strip shows a frozen frame.
//
// This guest allocates nothing per event, so it would survive gc=leaking; the
// flag stays because the next person to add an event handler here should not
// have to rediscover that.
package main

import "unsafe"

// staticUpdate registers three items. The ids are `plugin:<plugin-id>:<action-id>`,
// which is the form the host parses to dispatch a plugin action
// (`input/mouse.rs`), and each must match an `[[actions]]` id in the manifest.
//
// The host has no per-menu-kind scoping: `plugin_items` is handed to the
// workspace, tab AND pane menus alike (`input/mouse.rs`, three call sites), so
// all three items appear in every context menu. Each action therefore resolves
// its own target and degrades gracefully rather than assuming which menu it was
// invoked from. The spec for scoping is filed in the overnight execution plan.
const staticUpdate = `{"layers":[{"surface":"context_menu","nodes":[{"kind":"menu","title":"Workspace ops","items":[` +
	`{"id":"plugin:builtin.workspace-ops:convert-to-workspace","label":"Convert to workspace"},` +
	`{"id":"plugin:builtin.workspace-ops:move-to-workspace","label":"Move to workspace..."},` +
	`{"id":"plugin:builtin.workspace-ops:copy-metadata","label":"Copy metadata"}` +
	`],"selected":0}]}]}`

// eventResponse is returned for every event: an empty update, meaning "I draw
// nothing". It is a package-level constant string so its memory is static and
// the pointer we hand the host stays valid forever.
const eventResponse = `{"layers":[]}`

// inputBuf is the host's scratch space for event JSON. Reused deliberately: the
// v3 ABI has no dealloc, so a fresh allocation per event would ratchet linear
// memory up until the module dies. 4 KiB covers the frame events this guest
// ignores; a larger request grows it once and keeps it.
var inputBuf = make([]byte, 4096)

//export herdr_ui_api_version
func herdrUIAPIVersion() int32 { return 3 }

//export herdr_ui_alloc
func herdrUIAlloc(size int32) int32 {
	if size < 0 {
		return 0
	}
	if int(size) > len(inputBuf) {
		inputBuf = make([]byte, size)
	}
	return int32(uintptr(unsafe.Pointer(unsafe.SliceData(inputBuf))))
}

//export herdr_ui_static_update_ptr
func herdrUIStaticUpdatePtr() int32 {
	return int32(uintptr(unsafe.Pointer(unsafe.StringData(staticUpdate))))
}

//export herdr_ui_static_update_len
func herdrUIStaticUpdateLen() int32 { return int32(len(staticUpdate)) }

//export herdr_ui_on_event
func herdrUIOnEvent(ptr int32, length int32) int32 {
	_, _ = ptr, length // the menu is static; no event changes it
	return int32(uintptr(unsafe.Pointer(unsafe.StringData(eventResponse))))
}

//export herdr_ui_on_event_len
func herdrUIOnEventLen() int32 { return int32(len(eventResponse)) }

// main is required for a TinyGo main package but is never called: the host does
// not run a start function.
func main() {}
