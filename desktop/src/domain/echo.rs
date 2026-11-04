//! Supresión de eco: correlaciona comandos remotos con eventos locales.

use std::time::{Duration, Instant};

use crate::domain::media::Snapshot;

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
