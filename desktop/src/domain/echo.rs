//! Supresión de eco: correlaciona comandos remotos con eventos locales.

use std::time::{Duration, Instant};

use crate::domain::media::{Command, Snapshot};

/// Periodo de gracia tras el cual un comando remoto pendiente se descarta
/// aunque no llegue el evento local coincidente.
pub const ECHO_GRACE_PERIOD: Duration = Duration::from_millis(300);

/// Comando remoto aplicado localmente, pendiente de correlación con el
/// evento local que genera para suprimir su eco.
pub struct PendingCommand {
    pub action: String,
    pub target_ticks: Option<i64>,
    pub received_at: Instant,
}

impl PendingCommand {
    /// Registra un comando remoto para suprimir el evento local que genera.
    pub fn from_command(command: &Command, received_at: Instant) -> Self {
        let (action, target_ticks) = match command {
            Command::Play => ("play", None),
            Command::Pause => ("pause", None),
            Command::Stop => ("stop", None),
            Command::SeekBackward => ("seek_backward", None),
            Command::SeekForward => ("seek_forward", None),
            Command::SeekTo(ticks) => ("seek_to", Some(*ticks)),
            Command::SelectSession(_) => ("select_session", None),
        };

        Self {
            action: action.to_string(),
            target_ticks,
            received_at,
        }
    }
}

/// Decide si un snapshot local es el eco de un comando remoto pendiente.
pub fn is_echo(pending: &PendingCommand, snapshot: &Snapshot) -> bool {
    let Some(target) = pending.target_ticks else {
        return false;
    };
    pending.action == "seek_to" && snapshot.position_ticks == target
}

/// Decide si un comando pendiente ya expiró por periodo de gracia.
pub fn is_expired(pending: &PendingCommand, now: Instant) -> bool {
    now >= pending.received_at + ECHO_GRACE_PERIOD
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::media::PlaybackState;

    #[test]
    fn seek_to_echo_is_suppressed() {
        let start = Instant::now();
        let pending = PendingCommand::from_command(&Command::SeekTo(125_000_000), start);
        let snapshot = Snapshot {
            has_session: true,
            position_ticks: 125_000_000,
            state: PlaybackState::Playing,
            ..Snapshot::no_session()
        };

        assert!(is_echo(&pending, &snapshot));
    }

    #[test]
    fn non_matching_local_event_is_published() {
        let start = Instant::now();
        let pending = PendingCommand::from_command(&Command::Play, start);
        let snapshot = Snapshot {
            has_session: true,
            position_ticks: 5_000_000,
            state: PlaybackState::Paused,
            ..Snapshot::no_session()
        };

        assert!(!is_echo(&pending, &snapshot));
    }

    #[test]
    fn pending_command_expires_after_grace_period() {
        let start = Instant::now();
        let pending = PendingCommand::from_command(&Command::Play, start);

        assert!(!is_expired(&pending, start + Duration::from_millis(100)));
        assert!(is_expired(
            &pending,
            start + ECHO_GRACE_PERIOD + Duration::from_millis(1)
        ));
    }

    #[test]
    fn seek_to_records_the_target_position() {
        let pending = PendingCommand::from_command(&Command::SeekTo(42), Instant::now());

        assert_eq!(pending.action, "seek_to");
        assert_eq!(pending.target_ticks, Some(42));
    }
}
