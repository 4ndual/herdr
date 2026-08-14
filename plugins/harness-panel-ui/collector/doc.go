// Command harness-panel-collector is the data plane for the herdr Harness Panel
// plugin.
//
// # Why a process at all
//
// The panel's renderer is a wasm guest with no sockets, no filesystem and no
// clock. Everything it draws has to arrive as bytes in its `ui_input_file`, so
// something outside it must hold the connection to the fleet daemon. That is
// this binary.
//
// # Why long-lived rather than herdr's per-tick collector
//
// herdr can spawn a `collector` argv once per dashboard tick and read its
// stdout. That contract is a fresh process per tick with a 5 s budget
// (src/plugin_collector.rs), which means a new TCP connection, a new WebSocket
// handshake and a full refetch every tick — polling by construction, and it
// throws away exactly the thing the daemon is good at: pushing.
//
// So this runs as a supervised sidecar instead, started by the plugin's startup
// event hook and holding ONE WebSocket for its lifetime. The same shape
// version-dash already uses for its out-of-band collector agent.
//
// # Event-based, and where it is not
//
// The daemon pushes `{"type":"fleet","data":[...]}` on /ws whenever fleet state
// changes. That is the panel's primary signal: fleet list, statuses, session
// counts and the live-tree parentage all move on it, and none of them are
// polled.
//
// Three kinds of state have no push channel on the daemon, and are refreshed on
// an explicit trigger rather than a timer wherever possible:
//
//   - harness DETAIL (/api/harness/<name>) — fetched when the guest selects a
//     harness, and again when a fleet event reports that harness changed.
//   - orchestrator livetree/history/exposure and the templates/models catalogs —
//     fetched when the corresponding view is opened, and re-fetched on a fleet
//     event, because a spawn or stop is precisely what changes them.
//   - a slow SAFETY poll (see safetyPollInterval) covers the case where the
//     daemon mutates state without emitting a fleet frame — e.g. an operator
//     editing files under $HARNESS_ROOT directly, which the daemon reads on
//     demand and never announces. This is the "server-side-only state" the
//     brief allows polling for: there is no event to subscribe to, and the
//     alternative is a panel that silently goes stale. It is deliberately slow
//     and it never drives the fleet list, which is push-fed.
//
// # Modes
//
//	harness-panel-collector collect   long-lived; owns the WebSocket, writes the input file
//	harness-panel-collector op        one-shot; applies a control/mutation request and exits
//
// `op` is what the plugin's action runs on a click: it POSTs the mutation to the
// daemon and updates the control file the collector watches, so a click both
// acts on the fleet and moves the panel's own view state.
package main
