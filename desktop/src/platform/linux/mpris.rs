//! Backend de medios para Linux sobre MPRIS (D-Bus).
//!
//! MPRIS es el estándar de Linux para controlar reproductores: Spotify, mpv,
//! VLC, RhythmBox, Firefox... todos exponen `org.mpris.MediaPlayer2.Player`.
//!
//! Diseño dirigido por eventos: [`ProgressTracker::tick`] espera a que llegue
//! una señal de D-Bus y solo entonces vuelve a leer las propiedades del
//! reproductor. Entre señales, la posición se interpola en local
//! ([`Progress::position`]). No hay ninguna consulta periódica al bus.
//!
//! Un solo hilo habla con D-Bus: [`mpris::Player`] envuelve la conexión en un
//! `Rc`, así que no se puede mandar a otro hilo, y el socket de `dbus` no
//! admite dos lectores.

use std::time::Duration;

use crossbeam_channel::Receiver;
use mpris::{PlaybackStatus, Player, PlayerFinder, Progress};

use crate::media::{Command, PlaybackState, Sink, Snapshot};

/// Cada cuánto se refresca la posición interpolada.
///
/// También es la latencia máxima de un clic en los botones, porque el hilo
/// espera acá: el click se atiende justo después del próximo tick.
const INTERVAL_MS: u32 = 100;

/// Cuánto se avanza o retrocede con los botones, en microsegundos.
const SEEK_STEP_US: i64 = 10_000_000;

/// Qué se espera cuando no hay ningún reproductor en el bus antes de
/// volver a buscar uno.
const RETRY_WITHOUT_PLAYER: Duration = Duration::from_secs(2);

/// Errores del backend.
#[derive(Debug)]
enum Error {
    Bus(mpris::DBusError),
    Find(mpris::FindingError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bus(err) => write!(f, "error de D-Bus: {err}"),
            Self::Find(err) => write!(f, "no se encontró un reproductor: {err}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<mpris::DBusError> for Error {
    fn from(err: mpris::DBusError) -> Self {
        Self::Bus(err)
    }
}

impl From<mpris::FindingError> for Error {
    fn from(err: mpris::FindingError) -> Self {
        Self::Find(err)
    }
}

pub fn spawn(cmd_rx: Receiver<Command>, sink: Sink) {
    std::thread::Builder::new()
        .name("mpris".to_string())
        .spawn(move || {
            if let Err(err) = run(cmd_rx, sink) {
                eprintln!("[media] el backend MPRIS se detuvo: {err}");
            }
        })
        .expect("no se pudo crear el hilo de medios");
}

fn run(cmd_rx: Receiver<Command>, sink: Sink) -> Result<(), Error> {
    let finder = PlayerFinder::new()?;
    let mut player = pick(&finder, None);
    let mut last = String::new();

    loop {
        match player.as_ref() {
            Some(p) => {
                last.clear();
                last.push_str(p.identity());
                if handle(p, &cmd_rx, &sink)? {
                    // El reproductor terminó: buscar otro desde cero.
                    player = pick(&finder, Some(&last));
                }
            }
            None => {
                // No hay ningún reproductor: avisar y reintentar más tarde.
                sink(Snapshot::no_session());
                std::thread::sleep(RETRY_WITHOUT_PLAYER);
                player = pick(&finder, Some(&last));
            }
        }
    }
}

/// Atiende el reproductor actual hasta que termina.
///
/// Devuelve `true` si hay que buscar otro reproductor.
fn handle(player: &Player, cmd_rx: &Receiver<Command>, sink: &Sink) -> Result<bool, Error> {
    let mut tracker = player.track_progress(INTERVAL_MS)?;
    eprintln!("[media] reproductor MPRIS: {}", player.identity());

    loop {
        // Espera la señal de D-Bus o el fin del intervalo, lo que pase
        // primero. Las propiedades solo se releen si llegó algo.
        let tick = tracker.tick();

        // Los comandos de la UI se atienden sin bloquear: `tick()` ya
        // esperó el intervalo.
        while let Ok(cmd) = cmd_rx.try_recv() {
            execute_command(cmd, player);
        }

        if tick.player_quit {
            return Ok(true);
        }

        sink(snapshot(tick.progress, player));
    }
}

/// Elige con qué reproductor trabajar.
///
/// Se prefiere el que está sonando. Si hay varios pausados, el último que se
/// vio: es el que el usuario está mirando.
fn pick(finder: &PlayerFinder, last: Option<&str>) -> Option<Player> {
    let all = match finder.find_all() {
        Ok(all) => all,
        Err(err) => {
            eprintln!("[media] no se pudieron listar los reproductores: {err}");
            return None;
        }
    };
    if all.is_empty() {
        return None;
    }

    // 1. El que está sonando.
    let playing = all
        .iter()
        .find(|p| matches!(p.get_playback_status(), Ok(PlaybackStatus::Playing)))
        .map(|p| p.bus_name().to_string());
    if let Some(name) = playing {
        return all.into_iter().find(|p| p.bus_name() == name);
    }

    // 2. El último que se vio, si sigue en la lista.
    if let Some(identidad) = last {
        let seen = all
            .iter()
            .find(|p| p.identity() == identidad)
            .map(|p| p.bus_name().to_string());
        if let Some(name) = seen {
            return all.into_iter().find(|p| p.bus_name() == name);
        }
    }

    // 3. El primero que exista.
    all.into_iter().next()
}

fn execute_command(cmd: Command, player: &Player) {
    let result = match cmd {
        Command::TogglePlayPause => player.play_pause(),
        Command::SeekBackward => player.seek(-SEEK_STEP_US),
        Command::SeekForward => player.seek(SEEK_STEP_US),
    };
    if let Err(err) = result {
        eprintln!("[media] el comando {cmd:?} falló: {err}");
    }
}

fn snapshot(progress: &Progress, player: &Player) -> Snapshot {
    let metadata = progress.metadata();

    Snapshot {
        has_session: true,
        title: metadata.title().unwrap_or_default().to_string(),
        artist: metadata.artists().unwrap_or_default().join(", "),
        album: metadata.album_name().unwrap_or_default().to_string(),
        source_app: player.identity().to_string(),
        state: state(progress.playback_status()),
        // Si la consulta falla, el reproductor probablemente ya no esté: es
        // más honesto no ofrecer la acción que ofrecerla rota.
        can_toggle: player.can_play().unwrap_or(false) && player.can_pause().unwrap_or(false),
        can_seek: player.can_seek().unwrap_or(false),
        position_ticks: to_ticks(progress.position()),
        duration_ticks: progress.length().map(to_ticks).unwrap_or(0),
    }
}

/// Traduce el estado de MPRIS al del contrato.
fn state(status: PlaybackStatus) -> PlaybackState {
    match status {
        PlaybackStatus::Playing => PlaybackState::Playing,
        PlaybackStatus::Paused => PlaybackState::Paused,
        PlaybackStatus::Stopped => PlaybackState::Stopped,
    }
}

/// Convierte una duración a ticks de 100 ns, que es la unidad que usa el
/// contrato de `media`.
///
/// MPRIS trabaja en microsegundos y Windows en ticks de 100 ns, así que la
/// conversión es ×10.
fn to_ticks(duration: Duration) -> i64 {
    let ticks = duration.as_micros().saturating_mul(10);
    i64::try_from(ticks).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_microseconds_to_ticks() {
        // MPRIS: 1.400.000 µs = 1,4 s. Ticks de 100 ns: 14.000.000.
        assert_eq!(to_ticks(Duration::from_micros(1_400_000)), 14_000_000);
        assert_eq!(to_ticks(Duration::from_secs(10)), 100_000_000);
        assert_eq!(to_ticks(Duration::ZERO), 0);
    }

    #[test]
    fn does_not_overflow_with_absurd_durations() {
        assert_eq!(to_ticks(Duration::MAX), i64::MAX);
    }

    #[test]
    fn translates_the_states() {
        assert_eq!(state(PlaybackStatus::Playing), PlaybackState::Playing);
        assert_eq!(state(PlaybackStatus::Paused), PlaybackState::Paused);
        assert_eq!(state(PlaybackStatus::Stopped), PlaybackState::Stopped);
    }
}
