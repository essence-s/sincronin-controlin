//! Puerto del backend de medios.
//!
//! La aplicación no sabe si el backend es GSMTC (Windows), MPRIS (Linux) o
//! el stub de desarrollo. Solo sabe que puede enviar comandos y recibir
//! snapshots.

use crate::domain::media::{Command, Snapshot};

/// Puerto del backend de medios.
///
/// Implementado por [`crate::adapters::media`] (GSMTC, MPRIS, stub).
pub trait MediaPort: Send + Sync + 'static {
    /// Ejecuta un comando de reproducción en el reproductor local.
    fn send_command(&self, command: Command);

    /// Se suscribe a los snapshots que publica el backend.
    ///
    /// El callback se invoca desde el hilo de medios; debe ser thread-safe.
    fn on_snapshot(&self, callback: Box<dyn Fn(Snapshot) + Send + Sync>);
}
