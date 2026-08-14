//! Host-side capability policy for the v3 UI plugin API bridge (spec §5.3):
//! maps each socket API method name reachable through `HostEffect::Api` to the
//! `PluginUiCapability` a module must hold to invoke it, plus an always-denied
//! set (host-reserved administration) and a blocking-methods set that is never
//! allowed regardless of capability (running them synchronously in-process
//! would wedge the render thread).
//!
//! Match semantics (normative, §5.3): allow rows match by exact method name —
//! the serde `rename` strings from `src/api/schema.rs`'s `Method` enum, which is
//! also how `App::handle_api_request_after_internal_events_drained` dispatches.
//! Deny rows may additionally use one trailing `.*` wildcard meaning
//! literal-prefix. Deny wins over allow; anything matching no row is denied.

use crate::api::schema::PluginUiCapability;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiMethodDecision {
    Allow(PluginUiCapability),
    Deny,
}

const READ_METHODS: &[&str] = &[
    "workspace.list",
    "workspace.get",
    "tab.list",
    "tab.get",
    "pane.list",
    "pane.get",
    "pane.read",
    "pane.process_info",
    "agent.list",
    "agent.get",
    "agent.read",
    "agent.explain",
    "layout.export",
    "plugin.list",
    "plugin.action.list",
    // fork(+127): read-only and side-effect free. A plugin being able to read its
    // OWN diagnostic state (did my panel get dropped? why?) is the difference
    // between a plugin that can report a problem and one that can only vanish.
    "plugin.ui.status",
];

const WRITE_METHODS: &[&str] = &[
    "pane.split",
    "pane.resize",
    "pane.close",
    // fork(+129): pane reordering. Deliberately NOT split into its own
    // capability, unlike `PaneGraphics` / `ApiInput` / `PluginAdmin`, because it
    // is not a new KIND of power: `layout.apply` is already in this row and can
    // rearrange every pane in the tab wholesale. Withholding `pane.swap` while
    // allowing `layout.apply` would not protect the user from anything — it
    // would only force a plugin that wants to swap two panes to rebuild and
    // reapply the entire layout, which is strictly more destructive and more
    // likely to lose state. Granting these makes an already-reachable outcome
    // ergonomic and precise; it does not widen the blast radius.
    //
    // `pane.move` relocates a pane to another tab/workspace. It stays here with
    // `pane.swap` for the same reason: `layout.apply` and `pane.close` (both
    // already `ApiWrite`) can each make a pane leave the user's current view,
    // and `pane.move` at least keeps it alive somewhere they can get back to.
    "pane.swap",
    "pane.move",
    "tab.create",
    "tab.close",
    "tab.focus",
    "tab.rename",
    // fork(ADR-001/ADR-003): restoring a hidden tab is an `api_write` power so
    // `invisible-dash` can put a tab the user hid back on the tab bar. It sits
    // with the other `tab.*` writes rather than in its own capability: it can
    // only make an already-owned tab visible again, which is strictly less
    // destructive than `tab.close`, already allowed here.
    "tab.show",
    "tab.hide",
    "workspace.create",
    "workspace.focus",
    "workspace.rename",
    "layout.apply",
    "plugin.action.invoke",
];

// fork(0.7.5-merge): upstream renamed `agent.send` to `agent.send_keys` in 0.7.5.
// Upstream's new `agent.prompt` is deliberately NOT allowed here: it accepts
// `wait` options and can block the host inside the bridge's synchronous call
// path, which is the same reason `events.wait` / `pane.wait_for_output` are in
// the never-allowed list below.
const INPUT_METHODS: &[&str] = &["pane.send_text", "agent.send_keys"];

/// fork: pane-graphics methods, gated on `PaneGraphics`.
///
/// `pane.graphics.set` is the host-side primitive the image plugin is built on:
/// the plugin decides which image goes where, the host owns the bytes and the
/// terminal escape emission. `.info` is read-only (cell pixel size) but lives
/// here rather than in `READ_METHODS` so one capability covers the whole group —
/// a module that cannot paint has no use for the cell metrics.
///
/// Not blocking: `set` is a synchronous state write with a 512 KiB payload cap,
/// no waiting and no streaming. `pane.graphics.stream` is the streaming sibling
/// and is deliberately absent — it is listed in `BLOCKING_METHODS` instead.
const PANE_GRAPHICS_METHODS: &[&str] = &[
    "pane.graphics.set",
    "pane.graphics.clear",
    "pane.graphics.info",
    // fork(+128): text overlays sit under `PaneGraphics` rather than getting a
    // capability of their own, for the same reason `pane.swap` sits under
    // `ApiWrite` — a split is justified by distinct POWER, not by a distinct
    // noun. A plugin holding `pane_graphics` can already paint arbitrary pixels
    // over every cell of a pane, which includes rendering any text it likes as
    // an image. Withholding `pane.text.*` from it would withhold nothing except
    // the legible, theme-aware, cheap way to do what it can already do badly.
    //
    // The converse also holds and is what makes this safe: `pane.text.*` is
    // strictly weaker. It composites at render time and cannot reach
    // `PaneState`, scrollback, or `pane.read`, so it cannot change what any
    // reader — including a resumed agent — believes was printed. If that ever
    // stops being true, this row is the wrong home for it.
    "pane.text.set",
    "pane.text.clear",
    // fork(+131): carve sits here too, and this is the first entry in the row
    // that is NOT a pure compositing call — it resizes the PTY. It still does
    // not warrant a capability of its own: a `pane_graphics` holder can already
    // paint over every cell of a pane, so it can already make the pane's text
    // unreadable. Carve makes the pane SMALLER, which is strictly less than
    // that, and unlike an overlay it is visible to the program in the pane and
    // reversible by the user with `pane.carve.clear`.
    "pane.carve.set",
    "pane.carve.clear",
];

/// fork: flip an already-installed plugin's enabled bit, gated on `PluginAdmin`.
///
/// These were host-reserved denies until the plugin-manager pane needed them.
/// They are an allow row rather than a `WRITE_METHODS` entry because a module
/// holding this can switch OTHER modules off — including the one drawing the
/// user's images — so it has to be declared in the manifest and visible in
/// `plugin list`, not folded into ordinary structural writes.
///
/// `plugin.link`/`plugin.unlink` stay denied below and are NOT part of this
/// capability: installing code from a filesystem path is a strictly larger
/// power than toggling something the user already chose to install.
const PLUGIN_ADMIN_METHODS: &[&str] = &["plugin.enable", "plugin.disable"];

/// fork(#34): the one method `PluginUiCapability::Metadata` grants. Narrow on
/// purpose — a plugin that colours panes must not thereby be able to claim a
/// pane's agent or session identity, so `pane.report_metadata` stays in the
/// deny list below and this is its only sanctioned sibling.
const METADATA_METHODS: &[&str] = &["pane.set_color"];

/// Host-reserved administration: exact-name denies regardless of capability.
const DENIED_EXACT_METHODS: &[&str] = &[
    "plugin.link",
    "plugin.unlink",
    "integration.install",
    "integration.uninstall",
    "worktree.create",
    "worktree.remove",
    "agent.start",
    "pane.report_agent",
    "pane.report_agent_session",
    "pane.report_metadata",
    "pane.clear_agent_authority",
    "pane.release_agent",
];

/// Literal-prefix denies (single trailing `.*`, per the deny-row wildcard rule).
const DENIED_PREFIXES: &[&str] = &["server."];

/// Blocking/streaming methods: MUST NOT appear in any allow row regardless of
/// capability (§5.3) — run synchronously in-process they would wedge the render
/// thread. The bridge is strictly request/response.
const BLOCKING_METHODS: &[&str] = &[
    "events.wait",
    "events.subscribe",
    "pane.wait_for_output",
    // fork: `pane.graphics.stream` is a multi-frame streaming upload (16 MiB
    // budget) held open across frames. It is `#[schemars(skip)]` but NOT
    // `#[serde(skip)]`, so it deserializes fine from a guest-supplied method
    // name and was surviving on default-deny alone. Listing it here makes the
    // refusal explicit and load-bearing rather than incidental — the moment
    // `PaneGraphics` exists, the streaming sibling of an allowed method is
    // exactly the thing someone would reach for next.
    "pane.graphics.stream",
];

/// Decides whether `method` may be invoked through `HostEffect::Api`, and if so
/// which capability is required. Default-deny: anything matching no row is
/// denied. Callers are expected to `tracing::warn!` on every denial themselves
/// (this function is pure policy, not logging).
pub(crate) fn decide(method: &str) -> ApiMethodDecision {
    if BLOCKING_METHODS.contains(&method) {
        return ApiMethodDecision::Deny;
    }
    if DENIED_EXACT_METHODS.contains(&method) {
        return ApiMethodDecision::Deny;
    }
    if DENIED_PREFIXES
        .iter()
        .any(|prefix| method.starts_with(prefix))
    {
        return ApiMethodDecision::Deny;
    }
    if READ_METHODS.contains(&method) {
        return ApiMethodDecision::Allow(PluginUiCapability::ApiRead);
    }
    if WRITE_METHODS.contains(&method) {
        return ApiMethodDecision::Allow(PluginUiCapability::ApiWrite);
    }
    if INPUT_METHODS.contains(&method) {
        return ApiMethodDecision::Allow(PluginUiCapability::ApiInput);
    }
    if PANE_GRAPHICS_METHODS.contains(&method) {
        return ApiMethodDecision::Allow(PluginUiCapability::PaneGraphics);
    }
    if PLUGIN_ADMIN_METHODS.contains(&method) {
        return ApiMethodDecision::Allow(PluginUiCapability::PluginAdmin);
    }
    if METADATA_METHODS.contains(&method) {
        return ApiMethodDecision::Allow(PluginUiCapability::Metadata);
    }
    ApiMethodDecision::Deny
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_allow_names() -> impl Iterator<Item = &'static str> {
        READ_METHODS
            .iter()
            .chain(WRITE_METHODS.iter())
            .chain(INPUT_METHODS.iter())
            .chain(PANE_GRAPHICS_METHODS.iter())
            .chain(PLUGIN_ADMIN_METHODS.iter())
            .chain(METADATA_METHODS.iter())
            .copied()
    }

    /// fork(#34): `pane.set_color` is reachable ONLY with the Metadata
    /// capability, and granting it must not drag `pane.report_metadata` along —
    /// colouring a pane is a preference, claiming its agent or session identity
    /// is an authority claim.
    #[test]
    fn metadata_capability_grants_set_color_and_nothing_else() {
        assert!(matches!(
            decide("pane.set_color"),
            ApiMethodDecision::Allow(PluginUiCapability::Metadata)
        ));
        assert!(matches!(
            decide("pane.report_metadata"),
            ApiMethodDecision::Deny
        ));
        assert!(matches!(
            decide("pane.report_agent"),
            ApiMethodDecision::Deny
        ));
        assert!(matches!(
            decide("pane.report_agent_session"),
            ApiMethodDecision::Deny
        ));
    }

    /// Guard demanded by the adversarial spec review: every allow-row method
    /// name must be a real socket API method tag (the exact
    /// `#[serde(rename = "...")]` string on `Method` in src/api/schema.rs), not a
    /// typo that would silently never match. `Method` is adjacently tagged
    /// (`tag = "method", content = "params"`), so feeding a `null` params value
    /// isolates exactly the tag-recognition step: an unknown tag fails with
    /// "unknown variant"; a recognized tag with wrong param shape fails with a
    /// different (type-specific) error. This gives a round-trip check against
    /// the real enum without needing a valid params payload per method.
    #[test]
    fn every_allow_name_round_trips_through_the_real_method_tag() {
        for name in all_allow_names() {
            let probe = serde_json::json!({"method": name, "params": serde_json::Value::Null});
            if let Err(err) = serde_json::from_value::<crate::api::schema::Method>(probe) {
                let message = err.to_string();
                assert!(
                    !message.contains("unknown variant"),
                    "policy table method '{name}' does not match any Method tag in src/api/schema.rs: {message}"
                );
            }
        }
    }

    #[test]
    fn blocking_methods_are_never_allowed_even_though_unlisted_as_denies() {
        for name in BLOCKING_METHODS {
            assert_eq!(
                decide(name),
                ApiMethodDecision::Deny,
                "{name} must never be allowed"
            );
            assert!(
                !READ_METHODS.contains(name)
                    && !WRITE_METHODS.contains(name)
                    && !INPUT_METHODS.contains(name)
                    && !PANE_GRAPHICS_METHODS.contains(name),
                "{name} must not appear in any allow row"
            );
        }
    }

    #[test]
    fn deny_wins_over_host_reserved_and_prefix_rows() {
        assert_eq!(decide("server.stop"), ApiMethodDecision::Deny);
        assert_eq!(decide("server.live_handoff"), ApiMethodDecision::Deny);
        assert_eq!(decide("plugin.link"), ApiMethodDecision::Deny);
        assert_eq!(decide("worktree.create"), ApiMethodDecision::Deny);
    }

    /// fork: toggling an installed plugin is reachable, installing one is not.
    /// These two live one method-namespace apart, so the split has to be
    /// asserted directly — moving `plugin.enable` out of the deny list must not
    /// drag `plugin.link` along with it.
    #[test]
    fn plugin_toggle_needs_its_own_capability_while_link_stays_host_reserved() {
        assert_eq!(
            decide("plugin.enable"),
            ApiMethodDecision::Allow(PluginUiCapability::PluginAdmin)
        );
        assert_eq!(
            decide("plugin.disable"),
            ApiMethodDecision::Allow(PluginUiCapability::PluginAdmin)
        );
        assert_eq!(decide("plugin.link"), ApiMethodDecision::Deny);
        assert_eq!(decide("plugin.unlink"), ApiMethodDecision::Deny);
    }

    /// The toggle must NOT be reachable by holding `ApiWrite`, which almost
    /// every plugin that touches panes already declares. If it ever resolves to
    /// `ApiWrite`, a plugin that only asked to split panes silently gains the
    /// power to switch off the user's other plugins.
    #[test]
    fn plugin_toggle_is_not_reachable_through_ordinary_api_write() {
        for method in ["plugin.enable", "plugin.disable"] {
            assert_ne!(
                decide(method),
                ApiMethodDecision::Allow(PluginUiCapability::ApiWrite),
                "{method} must not ride along with ApiWrite"
            );
        }
    }

    /// fork(+128): text overlays must be reachable, and must resolve to
    /// `PaneGraphics` — the same capability that already permits painting
    /// arbitrary pixels over the pane. A plugin holding that can render any text
    /// it likes as an image, so a separate capability here would gate nothing
    /// while making the manifest longer.
    #[test]
    fn pane_text_overlays_are_reachable_under_the_pane_graphics_capability() {
        for method in ["pane.text.set", "pane.text.clear"] {
            assert_eq!(
                decide(method),
                ApiMethodDecision::Allow(PluginUiCapability::PaneGraphics),
                "{method} must be reachable through PaneGraphics"
            );
        }
    }

    /// The companion to that argument: a module WITHOUT `pane_graphics` must not
    /// reach text overlays either. If this ever passes as `Allow`, the overlay
    /// API has quietly become ungated.
    #[test]
    fn text_overlays_are_not_reachable_as_an_ordinary_write() {
        for method in ["pane.text.set", "pane.text.clear"] {
            assert_ne!(
                decide(method),
                ApiMethodDecision::Allow(PluginUiCapability::ApiWrite),
                "{method} must not fall through to ApiWrite"
            );
        }
    }

    /// fork(+129): the two reorder methods must be reachable, and must resolve
    /// to `ApiWrite` rather than to a bespoke capability — the whole argument for
    /// granting them is that they add no power `layout.apply` did not already
    /// carry. If someone later splits them out, this test should be deleted
    /// deliberately, with the new reasoning written down, not silently updated.
    #[test]
    fn pane_reorder_is_reachable_as_an_ordinary_structural_write() {
        for method in ["pane.swap", "pane.move"] {
            assert_eq!(
                decide(method),
                ApiMethodDecision::Allow(PluginUiCapability::ApiWrite),
                "{method} must be reachable through ApiWrite"
            );
        }
    }

    /// The companion to the row comment: `layout.apply` really is already
    /// allowed at the same level. If this ever stops being true, the rationale
    /// for granting `pane.swap`/`pane.move` collapses and they must be
    /// re-examined rather than left behind as an unjustified grant.
    #[test]
    fn the_reorder_rationale_still_holds_because_layout_apply_is_api_write() {
        assert_eq!(
            decide("layout.apply"),
            ApiMethodDecision::Allow(PluginUiCapability::ApiWrite)
        );
    }

    #[test]
    fn unknown_method_defaults_denied() {
        assert_eq!(decide("totally.unknown.method"), ApiMethodDecision::Deny);
    }

    #[test]
    fn known_allow_rows_resolve_to_the_documented_capability() {
        assert_eq!(
            decide("pane.list"),
            ApiMethodDecision::Allow(PluginUiCapability::ApiRead)
        );
        assert_eq!(
            decide("pane.split"),
            ApiMethodDecision::Allow(PluginUiCapability::ApiWrite)
        );
        assert_eq!(
            decide("pane.send_text"),
            ApiMethodDecision::Allow(PluginUiCapability::ApiInput)
        );
        assert_eq!(
            decide("agent.send_keys"),
            ApiMethodDecision::Allow(PluginUiCapability::ApiInput)
        );
        // fork(0.7.5-merge): the pre-0.7.5 name must no longer be allowed, and
        // upstream's blocking-capable `agent.prompt` stays default-denied.
        assert_eq!(decide("agent.send"), ApiMethodDecision::Deny);
        assert_eq!(decide("agent.prompt"), ApiMethodDecision::Deny);
    }

    /// fork: the three pane-graphics methods resolve to their own capability,
    /// not to `ApiWrite`. Painting into a pane is a distinct power from
    /// splitting one, so a module granted `api_write` must NOT inherit it.
    #[test]
    fn pane_graphics_methods_require_their_own_capability() {
        for method in [
            "pane.graphics.set",
            "pane.graphics.clear",
            "pane.graphics.info",
        ] {
            assert_eq!(
                decide(method),
                ApiMethodDecision::Allow(PluginUiCapability::PaneGraphics),
                "{method} must resolve to PaneGraphics"
            );
        }
        assert!(!WRITE_METHODS.contains(&"pane.graphics.set"));
        assert!(!READ_METHODS.contains(&"pane.graphics.info"));
    }

    /// fork: the streaming variant must stay denied even for a module that holds
    /// `PaneGraphics`. It is reachable by name (`#[schemars(skip)]` hides it from
    /// the generated schema but serde still accepts the tag), so default-deny is
    /// not enough on its own once the sibling methods are allowed.
    #[test]
    fn pane_graphics_stream_is_denied_even_with_the_capability() {
        assert_eq!(decide("pane.graphics.stream"), ApiMethodDecision::Deny);
        assert!(BLOCKING_METHODS.contains(&"pane.graphics.stream"));
        assert!(
            serde_json::from_value::<crate::api::schema::Method>(serde_json::json!({
                "method": "pane.graphics.stream",
            }))
            .err()
            .map(|err| !err.to_string().contains("unknown variant"))
            .unwrap_or(true),
            "pane.graphics.stream is reachable by name, so the deny must be explicit"
        );
    }
}
