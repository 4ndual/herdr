use herdr_ui_plugin_api::{
    HostEffect, MouseEventKind, UiActionRef, UiEvent, UiLayer, UiNode, UiSurface, UiTextSpan,
    UiUpdate,
};
use std::sync::Mutex;

static LAST_EVENT_RESPONSE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static UPDATE_JSON: &str = r#"{
  "layers": [
    {
      "surface": "header",
      "z_index": 10,
      "nodes": [
        {
          "kind": "status",
          "left": [
            { "text": " WASM UI ", "role": "accent" },
            { "text": "hot reload active", "role": "muted" }
          ],
          "right": [
            { "text": "plugin chrome", "role": "success" }
          ]
        }
      ]
    },
    {
      "surface": "footer",
      "z_index": 10,
      "nodes": [
        {
          "kind": "line",
          "spans": [
            { "text": "move mouse or press keys to live-update this plugin · ", "role": "muted" },
            { "text": "WASM overlays do not require rebuilding Herdr", "role": "accent" }
          ]
        }
      ]
    },
    {
      "surface": "tab_bar",
      "z_index": 11,
      "nodes": [
        {
          "kind": "tabs",
          "items": [
            { "id": "plugin:example.ui-wasm:fork", "label": "Fork" },
            { "id": "preview", "label": "Preview" },
            { "id": "inspect", "label": "Inspect" }
          ],
          "selected": 0
        }
      ]
    },
    {
      "surface": "right_panel",
      "z_index": 12,
      "nodes": [
        {
          "kind": "menu",
          "title": "WASM panel",
          "items": [
            { "id": "plugin:example.ui-wasm:fork", "label": "Fork current tab" },
            { "id": "copy", "label": "Copy pane metadata" }
          ],
          "selected": 0
        }
      ]
    },
    {
      "surface": "overlay",
      "z_index": 20,
      "nodes": [
        {
          "kind": "line",
          "spans": [
            { "text": "WASM overlay layer available", "role": "warning" }
          ]
        }
      ]
    },
    {
      "surface": "context_menu",
      "z_index": 30,
      "nodes": [
        {
          "kind": "menu",
          "title": "Tab actions",
          "items": [
            { "id": "plugin:example.ui-wasm:fork", "label": "Fork" }
          ],
          "selected": 0
        }
      ]
    }
  ]
}"#;

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_api_version() -> i32 {
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_static_update_ptr() -> i32 {
    UPDATE_JSON.as_ptr() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_static_update_len() -> i32 {
    UPDATE_JSON.len() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_alloc(len: i32) -> i32 {
    let mut bytes = Vec::<u8>::with_capacity(len.max(0) as usize);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_on_event(ptr: i32, len: i32) -> i32 {
    let Ok(event) = parse_event(ptr, len) else {
        return 0;
    };
    let update = update_for_event(&event);
    let encoded = serde_json::to_vec(&update).unwrap_or_default();
    let mut guard = LAST_EVENT_RESPONSE.lock().expect("response mutex poisoned");
    *guard = encoded;
    guard.as_ptr() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn herdr_ui_on_event_len() -> i32 {
    LAST_EVENT_RESPONSE
        .lock()
        .expect("response mutex poisoned")
        .len() as i32
}

fn parse_event(ptr: i32, len: i32) -> Result<UiEvent, serde_json::Error> {
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len.max(0) as usize) };
    serde_json::from_slice(bytes)
}

fn update_for_event(event: &UiEvent) -> UiUpdate {
    let mut effects = Vec::new();
    let status = match event {
        UiEvent::Mouse(mouse) => {
            if matches!(mouse.kind, MouseEventKind::RightDown) {
                effects.push(HostEffect::ShowToast {
                    message: format!("WASM saw right click at {},{}", mouse.column, mouse.row),
                });
            }
            format!("mouse {:?} @ {},{}", mouse.kind, mouse.column, mouse.row)
        }
        UiEvent::Key(key) => {
            if key.ctrl && key.key.eq_ignore_ascii_case("c") {
                effects.push(HostEffect::CopyText(
                    "copied from the ui-wasm-plugin example".into(),
                ));
                effects.push(HostEffect::ShowToast {
                    message: "WASM queued a clipboard write".into(),
                });
            }
            if key.key.eq_ignore_ascii_case("f") {
                effects.push(HostEffect::InvokePluginAction {
                    plugin_id: "example.ui-wasm".into(),
                    action_id: "fork".into(),
                    payload_json: Some("{\"source\":\"wasm-ui\",\"key\":\"f\"}".into()),
                });
            }
            format!("key {}", key.key)
        }
        UiEvent::Tick => "tick refresh".into(),
        UiEvent::FocusChanged(focus) => format!("focus changed: {:?}", focus),
        UiEvent::Frame(frame) => {
            let theme = frame
                .host
                .env
                .get("HERDR_THEME_NAME")
                .cloned()
                .unwrap_or_else(|| "unknown-theme".into());
            let pane = frame
                .host
                .env
                .get("HERDR_PANE_ID")
                .cloned()
                .unwrap_or_else(|| "no-pane".into());
            format!(
                "frame {:?} {}x{}+{},{} · {} · {}",
                frame.surface,
                frame.rect.width,
                frame.rect.height,
                frame.rect.x,
                frame.rect.y,
                theme,
                pane
            )
        },
        UiEvent::ThemeChanged => "theme changed".into(),
        UiEvent::ReloadRequested => "reload requested".into(),
    };

    UiUpdate {
        layers: vec![
            UiLayer {
                surface: UiSurface::Header,
                z_index: 10,
                consumes_mouse: false,
                nodes: vec![UiNode::Status {
                    left: vec![
                        UiTextSpan {
                            text: " WASM UI ".into(),
                            role: Some("accent".into()),
                        },
                        UiTextSpan {
                            text: status.clone(),
                            role: Some("muted".into()),
                        },
                    ],
                    right: vec![UiTextSpan {
                        text: "event-driven".into(),
                        role: Some("success".into()),
                    }],
                }],
            },
            UiLayer {
                surface: UiSurface::Footer,
                z_index: 10,
                consumes_mouse: false,
                nodes: vec![UiNode::Line {
                    spans: vec![
                        UiTextSpan {
                            text: "Ctrl-C copies via host effect · ".into(),
                            role: Some("muted".into()),
                        },
                        UiTextSpan {
                            text: "Right click triggers a toast from WASM".into(),
                            role: Some("accent".into()),
                        },
                    ],
                }],
            },
            UiLayer {
                surface: UiSurface::TabBar,
                z_index: 11,
                consumes_mouse: false,
                nodes: vec![UiNode::Tabs {
                    items: vec![
                        UiActionRef {
                            id: "plugin:example.ui-wasm:fork".into(),
                            label: "Fork".into(),
                        },
                        UiActionRef {
                            id: "preview".into(),
                            label: "Preview".into(),
                        },
                        UiActionRef {
                            id: "inspect".into(),
                            label: "Inspect".into(),
                        },
                    ],
                    selected: 0,
                }],
            },
            UiLayer {
                surface: UiSurface::RightPanel,
                z_index: 12,
                consumes_mouse: false,
                nodes: vec![UiNode::Menu {
                    title: "WASM panel".into(),
                    items: vec![
                        UiActionRef {
                            id: "plugin:example.ui-wasm:fork".into(),
                            label: "Fork current tab".into(),
                        },
                        UiActionRef {
                            id: "copy".into(),
                            label: "Copy pane metadata".into(),
                        },
                    ],
                    selected: 0,
                }],
            },
            UiLayer {
                surface: UiSurface::Overlay,
                z_index: 20,
                consumes_mouse: true,
                nodes: vec![UiNode::Line {
                    spans: vec![UiTextSpan {
                        text: format!("Live plugin status: {status}"),
                        role: Some("warning".into()),
                    }],
                }],
            },
            UiLayer {
                surface: UiSurface::ContextMenu,
                z_index: 30,
                consumes_mouse: false,
                nodes: vec![UiNode::Menu {
                    title: "Tab actions".into(),
                    items: vec![UiActionRef {
                        id: "plugin:example.ui-wasm:fork".into(),
                        label: "Fork".into(),
                    }],
                    selected: 0,
                }],
            },
        ],
        effects,
        consumed_action_ids: Vec::new(),
    }
}
