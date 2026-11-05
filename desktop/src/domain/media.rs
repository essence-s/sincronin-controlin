//! Tipos de medios del dominio.
//!
//! Es el contrato agnóstico de plataforma para comandos y snapshots de
//! reproducción. Los backends de `platform` y el adaptador de medios los
//! comparten sin depender entre sí.

/// Orden que envía la UI al backend de medios.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Reproduce (no invierte el estado).
    Play,
    /// Pausa (no invierte el estado).
    Pause,
    Stop,
    SeekBackward,
    SeekForward,
    /// Buscar a una posición absoluta, en ticks de 100 ns.
    SeekTo(i64),
    /// Elegir qué sesión de medios controlar.
    SelectSession(String),
}

/// Una sesión de medios que el usuario puede elegir.
///
/// El `id` es con lo que el backend reconoce la sesión. El `label`
/// es lo que se ve en el selector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// Identificador de la sesión.
    pub id: String,
    /// Texto que se muestra en el selector.
    pub label: String,
}

/// Estado de reproducción.
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
    pub position_ticks: i64,
    pub duration_ticks: i64,
    /// Tasa de reproducción (1.0 = velocidad normal).
    pub playback_rate: f64,
    /// Las sesiones de medios disponibles.
    pub sessions: Vec<Session>,
    /// El `id` de la sesión que se está controlando.
    pub session_id: String,
}

impl Snapshot {
    /// Estado cuando no hay ninguna sesión multimedia.
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
            playback_rate: 1.0,
            sessions: Vec::new(),
            session_id: String::new(),
        }
    }
}
