//! Shim de desarrollo. **No es una implementación de Linux.**
//!
//! No existe GSMTC fuera de Windows, así que simulamos una sesión para poder
//! iterar el diseño de la barra desde Linux. En Windows se compila
//! [`super::windows`] en su lugar.
//!
//! Cuando exista una implementación real para Linux (MPRIS sobre D-Bus, por
//! ejemplo), este archivo se reemplaza por ella y deja de hacer falta.

use crossbeam_channel::Receiver;

use crate::AppWindow;
use crate::media::{Command, PlaybackState, Sink, Snapshot};

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
                    title: "Backend de prueba (sin GSMTC)".to_string(),
                    artist: "solo en Linux · en Windows manda el sistema".to_string(),
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
                });
            };

            publish(playing, position);

            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    Command::TogglePlayPause => playing = !playing,
                    Command::SeekBackward => {
                        position = (position - 10 * TICKS_PER_SEC).max(0);
                    }
                    Command::SeekForward => {
                        position = (position + 10 * TICKS_PER_SEC).min(TOTAL_SECS * TICKS_PER_SEC);
                    }
                }
                publish(playing, position);
            }
        })
        .expect("no se pudo crear el hilo de prueba");
}

/// No-op: en Linux no hay estilos de ventana que ajustar.
///
/// Existe para que [`crate::app`] pueda llamar a `configure` sin saber en qué
/// plataforma está — el despacho vive en [`super`].
pub fn configure(_ui: &AppWindow) {}
