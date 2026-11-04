//! Servicios de la capa de aplicación (casos de uso).
//!
//! Cada servicio orquesta los puertos sin conocer adaptadores concretos.
//! Se instancian y se conectan en [`crate::app`].

pub mod pairing;
pub mod state;
