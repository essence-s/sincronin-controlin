//! Caso de uso: comandos de reproducción de medios.

use crate::ports::MediaPort;
use crate::domain::media::Command;

/// Servicio de medios: ejecutar comandos en el reproductor local.
#[derive(Clone)]
pub struct MediaService<M: MediaPort + Clone> {
    media: M,
}

impl<M: MediaPort + Clone> MediaService<M> {
    /// Crea un nuevo servicio con el puerto de medios inyectado.
    pub fn new(media: M) -> Self {
        Self { media }
    }

    /// Ejecuta un comando de reproducción.
    pub fn execute(&self, command: Command) {
        self.media.send_command(command);
    }

    /// Elegir qué sesión de medios controlar.
    pub fn select_session(&self, id: String) {
        self.media.send_command(Command::SelectSession(id));
    }
}
