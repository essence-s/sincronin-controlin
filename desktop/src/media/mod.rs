//! Tipos compartidos entre la UI y el backend de medios.
//!
//! Este módulo es **agnóstico de plataforma**: no tiene ningún `#[cfg]`.
//! Solo define el contrato que ambos lados implementan. El dispatch entre el
//! backend real de Windows y el shim de desarrollo vive en [`crate::platform`].

use std::sync::Arc;

/// Orden que envía la UI al backend.
#[derive(Clone, Copy, Debug)]
pub enum Command {
    TogglePlayPause,
    SeekBackward,
    SeekForward,
}

/// Estado de reproducción tal y como lo reporta el sistema.
///
/// Es agnóstico de plataforma: los estados que son propios de un sistema (el
/// ciclo de vida de una sesión WinRT, por ejemplo) se traducen en su adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
    Unknown,
}

/// Fotografía del reproductor actual, lista para pintar en la UI.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub has_session: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub source_app: String,
    pub state: PlaybackState,
    pub can_toggle: bool,
    pub can_seek: bool,
    /// Posición y duración en ticks de 100 ns.
    ///
    /// Es la unidad del contrato: los adapters convierten a la suya (Windows
    /// ya usa ticks de 100 ns; MPRIS usa microsegundos).
    pub position_ticks: i64,
    pub duration_ticks: i64,
}

impl Snapshot {
    /// Estado cuando no hay ninguna sesión multimedia.
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
    pub fn no_session() -> Self {
        Self {
            has_session: false,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            source_app: String::new(),
            state: PlaybackState::Unknown,
            can_toggle: false,
            can_seek: false,
            position_ticks: 0,
            duration_ticks: 0,
        }
    }
}

/// Callback con el que el backend publica cada cambio. Es `Send + Sync` porque
/// lo comparten el hilo de medios y los manejadores de eventos del sistema.
pub type Sink = Arc<dyn Fn(Snapshot) + Send + Sync + 'static>;
