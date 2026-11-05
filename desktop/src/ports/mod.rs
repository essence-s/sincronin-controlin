//! Puertos de la capa de aplicación.
//!
//! Los traits definen el contrato entre la aplicación y la infraestructura,
//! sin saber cómo se implementa. [`UiPort`], [`MediaPort`] y [`NetPort`] son
//! puertos de salida; [`AppPort`] es el puerto de entrada que usan los
//! adaptadores de UI.

pub mod app_port;
pub mod media_port;
pub mod net_port;
pub mod ui_port;

pub use app_port::AppPort;
pub use media_port::MediaPort;
pub use net_port::NetPort;
pub use ui_port::UiPort;
