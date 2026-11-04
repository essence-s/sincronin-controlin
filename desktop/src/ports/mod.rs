//! Puertos de la capa de aplicación.
//!
//! Los traits definen lo que la aplicación necesita de la infraestructura,
//! sin saber cómo se implementa. Cada adaptador (Slint, str0m, GSMTC, MPRIS)
//! implementa uno o más de estos puertos.

pub mod media_port;
pub mod net_port;
pub mod ui_port;
