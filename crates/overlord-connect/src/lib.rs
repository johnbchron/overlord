//! Connectors: read-only adapters for the systems overlord observes.
//!
//! SPEC.md section 11. Three things live here, and the separation is
//! deliberate: the [`Connector`] trait says what a connector does,
//! [`RestrictedHttp`] bounds what it can reach, and [`Ruleset`] turns
//! what it read into the guaranteed overlay — as versioned data rather
//! than code, because operators revise normalization.

pub mod connector;
pub mod error;
pub mod http;
pub mod normalize;

pub use connector::{Connector, Observation, ObserveCtx, Registry, Snapshot};
pub use error::ConnectorError;
pub use http::{Allow, PathPattern, ReadMethod, RestrictedHttp};
pub use normalize::{Coerce, FieldRule, Normalized, Ruleset, StatusRule};
