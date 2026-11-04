//! Servicios de la capa de aplicación (casos de uso).
//!
//! Cada servicio orquesta los puertos sin conocer adaptadores concretos.
//! Se instancian en [`crate::main`] y se conectan a `app.rs` en una fase
//! posterior.

pub mod media;
pub mod pairing;
pub mod state;
