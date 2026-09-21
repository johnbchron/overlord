//! One module per screen of SPEC.md section 5.
//!
//! Each module exports the full-page handler and, where the screen has
//! htmx-swapped parts, the fragment handlers that render exactly the
//! same markup the full page would have produced for that region. The
//! fragment is always the smaller of the two: a page calls into it, so
//! the two cannot drift.

pub mod entities;
pub mod identity;
pub mod rules;
pub mod settings;
pub mod signin;
pub mod subject;
pub mod sweeps;
pub mod systems;
pub mod users;
pub mod violations;
