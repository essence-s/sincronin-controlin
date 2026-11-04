//! Barra flotante para controlar la reproducción multimedia de Windows.
//!
//! Punto de entrada delgado: solo declara los módulos y delega en
//! [`app::run`]. La composición vive en `app`: los adaptadores implementan
//! los puertos y los servicios orquestan los casos de uso. El backend de
//! medios usa GSMTC (`Windows.Media.Control`) en Windows y MPRIS en Linux,
//! ambos dirigidos por eventos.
//!
//! Regla del proyecto: `#[cfg(...)]` aparece **solo** dentro de
//! [`platform`]. Este archivo, [`media`] y [`net`] son agnósticos de
//! plataforma.
//!
//! Única excepción a la regla: el `#![cfg_attr]` de abajo. Controla si el
//! binario muestra consola, y los atributos a nivel de crate no se pueden
//! mover. No es despacho de plataforma.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod adapters;
mod app;
mod domain;
mod media;
mod net;
mod platform;
mod ports;
mod services;

// Genera `AppWindow` y `NetWindow` en la raíz del crate. Tiene que
// quedarse acá: los módulos que reciben la ventana la referencian como
// `crate::AppWindow`.
slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    app::run()
}
