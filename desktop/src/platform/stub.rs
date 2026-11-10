//! Shim de desarrollo. **No es una implementación real de ninguna plataforma.**
//!
//! Simula una sesión para poder iterar el diseño de la barra donde no hay
//! backend. En Windows se compila `super::windows`; en Linux, `super::linux`.
//!
//! Cuando exista una implementación real para esta plataforma, este archivo
//! se reemplaza por ella y deja de hacer falta.

use crossbeam_channel::Receiver;

use crate::AppWindow;
use crate::domain::media::{Command, PlaybackState, Snapshot};
use crate::platform::Sink;

const TICKS_PER_SEC: i64 = 10_000_000;
const TOTAL_SECS: i64 = 215;

pub fn spawn(cmd_rx: Receiver<Command>, sink: Sink) {
    std::thread::Builder::new()
        .name("stub".to_string())
        .spawn(move || {
            let mut playing = false;
            let mut position = 42 * TICKS_PER_SEC;

            let publish = |playing: bool, position: i64| {
                sink(Snapshot {
                    has_session: true,
                    title: "Backend de prueba (sin backend real)".to_string(),
                    artist: "shim de desarrollo".to_string(),
                    album: String::new(),
                    source_app: "stub".to_string(),
                    state: if playing {
                        PlaybackState::Playing
                    } else {
                        PlaybackState::Paused
                    },
                    can_toggle: true,
                    can_seek: true,
                    position_ticks: position,
                    duration_ticks: TOTAL_SECS * TICKS_PER_SEC,
                    playback_rate: 1.0,
                    // El shim no tiene sesiones reales que elegir.
                    sessions: Vec::new(),
                    session_id: String::new(),
                });
            };

            publish(playing, position);

            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    Command::Play => playing = true,
                    Command::Pause => playing = false,
                    Command::Stop => {
                        playing = false;
                        position = 0;
                    }
                    Command::SeekBackward => {
                        position = (position - 10 * TICKS_PER_SEC).max(0);
                    }
                    Command::SeekForward => {
                        position = (position + 10 * TICKS_PER_SEC).min(TOTAL_SECS * TICKS_PER_SEC);
                    }
                    Command::SeekTo(ticks) => {
                        position = ticks.clamp(0, TOTAL_SECS * TICKS_PER_SEC);
                    }
                    // El shim solo tiene una sesión: no hay a qué cambiar.
                    Command::SelectSession(_) => {}
                }
                publish(playing, position);
            }
        })
        .expect("no se pudo crear el hilo de prueba");
}

/// No-op: esta plataforma no tiene estilos de ventana que ajustar.
///
/// Existe para que [`crate::app`] pueda llamar a `configure` sin saber en qué
/// plataforma está — el despacho vive en [`super`].
pub fn configure(_ui: &AppWindow) {}

/// Lo mismo para el reanclaje: en el stub no hay ventana nativa que mover.
pub fn reposition(_ui: &AppWindow) {}

/// El shim no tiene un compositor que recolque la ventana al redimensionarla,
/// así que se puede animar el cambio de tamaño sin que se corra.
pub fn animates_resize() -> bool {
    true
}
