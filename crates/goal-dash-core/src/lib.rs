//! goal-widget embeddable core, carved from `~/Projects/goal-widget/src/main.rs`
//! (App, `.goal.json` resolver, history/plans/time-gate loaders,
//! proposals/constraints, the `Act` handlers, and
//! `claude_infer`/infer-tree/observe/sort).
//!
//! The host drives it through three contracts on [`state::App`]:
//! - [`App::refresh`](state::App::refresh) — adopt a new focused session.
//! - [`App::handle`](state::App::handle) — route a [`view::GoalAction`] intent,
//!   returning the [`crate::effect::Effect`]s the host must run.
//! - [`App::poll`](state::App::poll) — periodic tick (re-read files, observer).
//!
//! Rendering is pure: [`render::draw`] takes `&mut App` and only paints. The
//! core never depends on a terminal backend; the host maps its crossterm events
//! onto [`view::GoalAction`] (mirrors `usage-dash-core::Action`).

pub mod context;
pub mod effect;
pub mod infer;
pub mod model;
pub mod render;
pub mod state;
pub mod store;
pub mod timegate;
pub mod view;
pub mod wasm_bridge;

pub use model::{GoalDoc, GoalState, Links, Par, Proposal, St, Sub, Tree};
pub use render::draw;
pub use state::App;
pub use store::GoalStore;
pub use view::GoalAction;
