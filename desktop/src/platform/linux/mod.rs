//! Implementación para Linux.
//!
//! - [`mpris`]: backend de medios sobre MPRIS (D-Bus), el estándar de Linux
//!   para controlar reproductores.
//! - [`window`]: lo que se puede hacer con la ventana. En Wayland, casi
//!   nada: el compositor manda. Las reglas van en su configuración.

pub mod mpris;
pub mod window;
