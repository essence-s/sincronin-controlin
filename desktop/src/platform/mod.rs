//! Único punto de despacho por plataforma del proyecto.
//!
//! Regla: **`#[cfg(...)]` solo aparece dentro de este módulo y sus hijos.**
//! El resto de `src/` es agnóstico de plataforma. Así, compilar en Linux
//! elimina de raíz el código de Windows y viceversa: no hay un
//! `if platform ==` en runtime que se pueda invertir por error.
//!
//! - **Windows** → backend real sobre GSMTC (`Windows.Media.Control`) más los
//!   estilos de ventana de [`windows::window`].
//! - **Linux** → backend real sobre MPRIS (D-Bus) más [`linux::window`].
//! - **Otros** → [`stub`], un shim para iterar la UI sin backend real. No es
//!   una implementación de macOS: solo permite compilar y ver la interfaz.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::gsmtc::spawn;
#[cfg(windows)]
pub use windows::window::configure;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::mpris::spawn;
#[cfg(target_os = "linux")]
pub use linux::window::configure;

#[cfg(not(any(windows, target_os = "linux")))]
mod stub;
#[cfg(not(any(windows, target_os = "linux")))]
pub use stub::{configure, spawn};
