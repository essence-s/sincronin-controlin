//! Dominio puro: tipos y reglas de negocio sin dependencias de plataforma.
//!
//! Ningún módulo de esta carpeta usa `#[cfg]`, ni depende de `slint`,
//! `str0m`, `axum`, `windows` ni `dbus`. Solo `std` y `protocol`.

pub mod echo;
pub mod interpolation;
pub mod media;
pub mod room;
