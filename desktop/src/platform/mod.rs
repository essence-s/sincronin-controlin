//! Único punto de despacho por plataforma del proyecto.
//!
//! Regla: **`#[cfg(...)]` solo aparece dentro de este módulo y sus hijos.**
//! El resto de `src/` es agnóstico de plataforma. Así, compilar en Linux ya
//! elimina de raíz el código de Windows y viceversa — no hay `if platform ==`
//! en runtime que se pueda equivocar.
//!
//! - **Windows** → backend real sobre GSMTC (`Windows.Media.Control`) más los
//!   estilos de ventana de [`windows::window`].
//! - **Otros** → [`stub`], un shim de desarrollo que simula una sesión para
//!   poder iterar la UI en Linux. No es una implementación de Linux.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::gsmtc::spawn;
#[cfg(windows)]
pub use windows::window::configure;

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::{configure, spawn};
