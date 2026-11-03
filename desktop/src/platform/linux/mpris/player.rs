//! Operaciones sobre el reproductor MPRIS actual.
//!
//! Acá vive todo lo que habla con `mpris::Player`: leer las capacidades,
//! ejecutar los comandos de la UI y armar la foto que consume Slint.
//!
//! Las capacidades (`CanSeek`, `CanPlay`, `CanPause`) se cachean y solo se
//! releen cuando llega un evento del reproductor. Si se consultaran en cada
//! tick, el bucle estaría sondeando D-Bus cada 100 ms, que es justo lo que el
//! diseño evita.

use std::time::Instant;

use mpris::{Metadata, Player, Progress};

use crate::media::{Command, Session, Snapshot};

use super::timeline::Timeline;

/// Ruta que MPRIS usa como "sin pista". No sirve para `SetPosition`.
const NO_TRACK_ID: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";

/// Cuánto se avanza o retrocede con los botones, en ticks de 100 ns.
const SEEK_STEP_TICKS: i64 = 100_000_000;
/// Lo mismo, en microsegundos, la unidad de MPRIS.
const SEEK_STEP_US: i64 = SEEK_STEP_TICKS / 10;

/// Lo que el reproductor permite hacer, cacheado hasta el próximo evento.
#[derive(Clone, Copy, Debug)]
pub struct Controls {
    pub can_toggle: bool,
    pub can_seek: bool,
}

impl Controls {
    pub fn read(player: &Player) -> Self {
        Self {
            // Si la consulta falla, el reproductor probablemente ya no esté:
            // es más honesto no ofrecer la acción que ofrecerla rota.
            can_toggle: player.can_play().unwrap_or(false) && player.can_pause().unwrap_or(false),
            can_seek: player.can_seek().unwrap_or(false),
        }
    }
}

/// Arma la foto que se pinta en la UI.
///
/// La posición sale de la línea de tiempo local, no de una consulta nueva a
/// D-Bus: entre eventos la posición se interpola.
pub fn snapshot(
    timeline: &Timeline,
    progress: &Progress,
    player: &Player,
    sessions: &[Session],
    controls: Controls,
    now: Instant,
) -> Snapshot {
    let metadata = progress.metadata();

    Snapshot {
        has_session: true,
        title: metadata.title().unwrap_or_default().to_string(),
        artist: metadata.artists().unwrap_or_default().join(", "),
        album: metadata.album_name().unwrap_or_default().to_string(),
        source_app: player.identity().to_string(),
        state: timeline.state(),
        can_toggle: controls.can_toggle,
        can_seek: controls.can_seek,
        position_ticks: timeline.position(now),
        duration_ticks: timeline.duration_ticks(),
        playback_rate: timeline.rate(),
        sessions: sessions.to_vec(),
        session_id: player.bus_name().to_string(),
    }
}

/// Ejecuta un comando de la UI.
///
/// `SelectSession` no se atiende acá: lo resuelve el bucle, que es el que sabe
/// a qué reproductor estamos suscritos.
pub fn execute(
    command: Command,
    player: &Player,
    metadata: &Metadata,
    timeline: &mut Timeline,
    now: Instant,
) {
    match command {
        Command::Play => {
            run(player.play(), "reproducir");
        }
        Command::Pause => {
            run(player.pause(), "pausa");
        }
        Command::Stop => {
            run(player.stop(), "detener");
        }
        Command::SeekBackward => {
            if run(player.seek(-SEEK_STEP_US), "retroceder") {
                timeline.seek_by(-SEEK_STEP_TICKS, now);
            }
        }
        Command::SeekForward => {
            if run(player.seek(SEEK_STEP_US), "avanzar") {
                timeline.seek_by(SEEK_STEP_TICKS, now);
            }
        }
        Command::SeekTo(ticks) => seek_to(player, metadata, ticks, timeline, now),
        Command::SelectSession(id) => {
            eprintln!("[media] elegir sesión se atiende en el bucle: {id}");
        }
    }
}

/// Ejecuta una acción y devuelve si salió bien.
fn run(result: Result<(), mpris::DBusError>, action: &str) -> bool {
    match result {
        Ok(()) => true,
        Err(err) => {
            eprintln!("[media] {action} falló: {err}");
            false
        }
    }
}

/// El `mpris:trackid` que sirve para `SetPosition`, si lo hay.
///
/// El sentinel `NoTrack` es válido como identificador para otras operaciones,
/// pero no identifica una pista sobre la que se pueda posicionar.
fn absolute_track_id(metadata: &Metadata) -> Option<mpris::TrackID> {
    metadata.track_id().filter(|id| id.as_str() != NO_TRACK_ID)
}

/// Salta a una posición absoluta.
///
/// MPRIS tiene dos caminos:
///
/// - `SetPosition(TrackId, Position)` es absoluto y es el correcto. Necesita
///   el `mpris:trackid` de la pista actual.
/// - `Seek(Offset)` es **relativo**. Se usa como respaldo cuando el
///   reproductor no publica un `trackid` utilizable (algunos, como mpv,
///   publican el sentinel `NoTrack`).
///
/// En ambos casos el ancla local se actualiza en el acto para que el slider no
/// rebote; el `Seeked` que llegue después confirma la posición exacta.
fn seek_to(
    player: &Player,
    metadata: &Metadata,
    target_ticks: i64,
    timeline: &mut Timeline,
    now: Instant,
) {
    let target = timeline.clamp_position(target_ticks);
    let target_us = ticks_to_micros(target).max(0);
    let current = timeline.position(now);
    let delta_us = ticks_to_micros(target.saturating_sub(current));

    let result = match absolute_track_id(metadata) {
        Some(track_id) => player
            .set_position_in_microseconds(track_id, u64::try_from(target_us).unwrap_or(u64::MAX)),
        None => {
            // Sin trackid no hay forma de usar SetPosition sin arriesgarse a
            // cambiar de pista en el medio. `Seek` es relativo: se calcula el
            // delta desde la posición que conocemos.
            player.seek(delta_us)
        }
    };

    match result {
        Ok(()) => {
            timeline.seek_to(target, now);
            eprintln!(
                "[media] posición cambiada a {:.1}s",
                target as f64 / 10_000_000.0
            );
        }
        Err(err) => eprintln!("[media] buscar falló: {err}"),
    }
}

/// Convierte ticks de 100 ns a microsegundos, la unidad de MPRIS.
fn ticks_to_micros(ticks: i64) -> i64 {
    ticks / 10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_ticks_to_microseconds() {
        assert_eq!(ticks_to_micros(10_000_000), 1_000_000);
        assert_eq!(ticks_to_micros(0), 0);
        assert_eq!(ticks_to_micros(-10_000_000), -1_000_000);
    }

    #[test]
    fn ignores_the_no_track_sentinel() {
        // mpv publica este trackid aunque haya una pista sonando.
        assert!(absolute_track_id(&Metadata::new(NO_TRACK_ID)).is_none());
    }

    #[test]
    fn accepts_a_real_track_id() {
        let metadata = Metadata::new("/org/mpris/MediaPlayer2/firefox");
        assert_eq!(
            absolute_track_id(&metadata).map(|id| id.as_str().to_string()),
            Some("/org/mpris/MediaPlayer2/firefox".to_string())
        );
    }
}
