//! The snapshot the Go collector writes, mirrored on the guest side.
//!
//! Every field is optional-by-default: the collector and the guest are separate
//! binaries that hot-reload independently, so a guest that predates a collector
//! field must render rather than fail to parse.

use serde::{Deserialize, Deserializer};

/// Accept an explicit `null` where a value is expected, yielding the default.
///
/// Go marshals a nil slice as `null`, not `[]`, and a collector that has not
/// fetched a view yet holds nil slices for it. serde's `default` only covers a
/// MISSING field, so without this the whole snapshot failed to parse the moment
/// any list was empty — and the guest, which falls back to an empty snapshot on
/// a parse error, rendered a permanently disconnected panel. Caught by the
/// byte-level contract test over real collector output.
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Harness {
    pub name: String,
    pub template: String,
    pub runtime: String,
    pub profile: bool,
    pub status: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub ctx: Option<f64>,
    pub pending: i64,
    pub sessions: i64,
    pub host: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TreeNode {
    pub name: String,
    pub status: String,
    pub state: String,
    pub parent: String,
    pub live: bool,
    pub task: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Template {
    pub name: String,
    pub version: String,
    pub description: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Memory {
    pub rel: String,
    pub name: String,
    pub body: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Session {
    pub id: String,
    pub bytes: i64,
    pub current: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Detail {
    pub name: String,
    pub status: String,
    pub runtime: String,
    pub template: String,
    #[serde(rename = "configDir")]
    pub config_dir: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub pending: i64,
    #[serde(deserialize_with = "null_to_default")]
    pub sessions: Vec<Session>,
    #[serde(rename = "liveRules")]
    pub live_rules: String,
    #[serde(rename = "memoryDir")]
    pub memory_dir: String,
    #[serde(deserialize_with = "null_to_default")]
    pub memories: Vec<Memory>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LastOp {
    pub action: String,
    pub target: String,
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub connected: bool,
    pub daemon: String,
    pub updated_unix: i64,
    pub reason: String,
    pub view: String,
    pub selected: String,
    pub detail_tab: String,
    pub scroll: i64,
    #[serde(deserialize_with = "null_to_default")]
    pub fleet: Vec<Harness>,
    #[serde(deserialize_with = "null_to_default")]
    pub tree: Vec<TreeNode>,
    #[serde(deserialize_with = "null_to_default")]
    pub templates: Vec<Template>,
    pub models: serde_json::Value,
    pub exposure: serde_json::Value,
    pub detail: Option<Detail>,
    pub last_op: Option<LastOp>,
}

impl Snapshot {
    pub fn view_or_default(&self) -> &str {
        if self.view.is_empty() {
            "fleet"
        } else {
            &self.view
        }
    }
}

/// The six views, in the site's nav order. Short labels because the panel is
/// narrow: the site can afford "Orchestrator", a 34-column column cannot.
pub const VIEWS: [(&str, &str); 6] = [
    ("fleet", "Fleet"),
    ("detail", "Detail"),
    ("orchestrator", "Orch"),
    ("templates", "Tmpl"),
    ("models", "Models"),
    ("memory", "Mem"),
];
