//! Integración real con Windows.
//!
//! - [`gsmtc`]: backend de medios sobre `Windows.Media.Control` (`GSMTC`), el
//!   hilo que habla con `WinRT`.
//! - [`window`]: estilos de ventana para que la barra se comporte como un
//!   widget flotante (sin foco, fuera de la barra de tareas, abajo al centro).

pub mod gsmtc;
pub mod window;
