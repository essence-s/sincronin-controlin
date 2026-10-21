//! Integración real con Windows.
//!
//! - [`aumid`]: traduce el identificador de una sesión de GSMTC al nombre de
//!   la app, para que el selector diga algo legible.
//! - [`gsmtc`]: backend de medios sobre `Windows.Media.Control` (`GSMTC`), el
//!   hilo que habla con `WinRT`.
//! - [`window`]: estilos de ventana para que la barra se comporte como un
//!   widget flotante (sin foco, fuera de la barra de tareas, abajo al centro).

pub mod aumid;
pub mod gsmtc;
pub mod window;
