//! Único punto de despacho por plataforma del proyecto.
//!
//! Regla: **`#[cfg(...)]` solo aparece dentro de este módulo y sus hijos.**
//! El resto de `src/` es agnóstico de plataforma. Así, compilar en Linux
//! elimina de raíz el código de Windows y viceversa: no hay un
//! `if platform ==` en runtime que se pueda invertir por error.
//!
//! - **Windows** → backend real sobre GSMTC (`Windows.Media.Control`) más los
//!   estilos de ventana de `windows::window`.
//! - **Linux** → backend real sobre MPRIS (D-Bus) más `linux::window`.
//! - **Otros** → `stub`, un shim para iterar la UI sin backend real. No es
//!   una implementación de macOS: solo permite compilar y ver la interfaz.

use std::sync::Arc;

use crate::domain::media::Snapshot;

/// Callback con el que un backend de plataforma publica cada cambio.
///
/// Los backends viven detrás de `#[cfg]`; el tipo queda acá para que el
/// adaptador de medios no dependa de una plataforma concreta.
pub type Sink = Arc<dyn Fn(Snapshot) + Send + Sync + 'static>;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::gsmtc::spawn;
#[cfg(windows)]
pub use windows::window::{animates_resize, configure, reposition};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::mpris::spawn;
#[cfg(target_os = "linux")]
pub use linux::window::{animates_resize, configure, reposition};

#[cfg(not(any(windows, target_os = "linux")))]
mod stub;
#[cfg(not(any(windows, target_os = "linux")))]
pub use stub::{animates_resize, configure, reposition, spawn};
