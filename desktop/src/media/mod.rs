//! Tipos compartidos entre la UI y el backend de medios.
//!
//! Los tipos viven ahora en [`crate::domain::media`]; este módulo solo
//! re-exporta y añade el `Sink` que comparten backend y UI.

pub use crate::domain::media::{Command, PlaybackState, Session, Snapshot};

use std::sync::Arc;

/// Callback con el que el backend publica cada cambio.
pub type Sink = Arc<dyn Fn(Snapshot) + Send + Sync + 'static>;
