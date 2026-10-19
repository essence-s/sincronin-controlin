//! Barra flotante para controlar la reproducción multimedia de Windows.
//!
//! Punto de entrada delgado: solo declara los módulos y delega en
//! [`app::run`]. La UI (Slint) habla únicamente con [`media`], que en Windows
//! usa GSMTC (`Windows.Media.Control`) de forma 100 % dirigida por eventos.
//!
//! Regla del proyecto: `#[cfg(...)]` aparece **solo** dentro de
//! [`platform`]. Este archivo y [`media`] son agnósticos de plataforma.
//!
//! Única excepción a la regla: el `#![cfg_attr]` de abajo. Controla si el
//! binario muestra consola, y los atributos a nivel de crate no se pueden
//! mover. No es despacho de plataforma.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod media;
mod platform;

// Genera `AppWindow` en la raíz del crate. Tiene que quedarse acá: los
// módulos que reciben la ventana la referencian como `crate::AppWindow`.
slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    app::run()
}
