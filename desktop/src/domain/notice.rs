//! Avisos breves que la ventana muestra en su tamaño intermedio.
//!
//! Es lo único que justifica ese tamaño: un comando que llega de otro equipo
//! o la entrada de alguien a la sala. El aviso no lleva controles ni título.

use crate::domain::media::Command;

/// Lo que dispara el tamaño intermedio de la ventana.
#[derive(Clone, Debug, PartialEq)]
pub enum Notice {
    /// Un equipo conectado mandó un comando de reproducción.
    RemoteCommand(Command),
    /// Entró otro equipo a la sala.
    PeerJoined,
}

impl Notice {
    /// El texto que se muestra en el aviso.
    pub fn label(&self) -> &'static str {
        match self {
            Self::RemoteCommand(Command::Play) => "Play",
            Self::RemoteCommand(Command::Pause) => "Pausa",
            Self::RemoteCommand(Command::Stop) => "Stop",
            Self::RemoteCommand(Command::SeekBackward) => "-10 s",
            Self::RemoteCommand(Command::SeekForward) => "+10 s",
            Self::RemoteCommand(Command::SeekTo(_)) => "Salto en la reproducción",
            Self::RemoteCommand(Command::SelectSession(_)) => "Cambio de sesión",
            Self::PeerJoined => "Se conectó otro equipo",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_notice_names_the_command() {
        assert_eq!(Notice::RemoteCommand(Command::Pause).label(), "Pausa");
        assert_eq!(Notice::RemoteCommand(Command::Play).label(), "Play");
    }

    #[test]
    fn the_peer_notice_says_someone_connected() {
        assert_eq!(Notice::PeerJoined.label(), "Se conectó otro equipo");
    }
}
