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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::Receiver;
use dbus::message::MatchRule;
use mpris::{PlaybackStatus, Player, PlayerFinder, Progress};

use crate::media::{Command, PlaybackState, Session, Sink, Snapshot};

/// Los argumentos de la señal `NameOwnerChanged` del bus: el nombre del
/// recurso, su dueño anterior y el nuevo.
struct NameOwnerChanged {
    name: String,
    old_owner: String,
    new_owner: String,
}

impl dbus::arg::ReadAll for NameOwnerChanged {
    fn read(i: &mut dbus::arg::Iter<'_>) -> Result<Self, dbus::arg::TypeMismatchError> {
        Ok(Self {
            name: i.read()?,
            old_owner: i.read()?,
            new_owner: i.read()?,
        })
    }
}

/// Cada cuánto se refresca la posición interpolada.
///
/// También es la latencia máxima de un clic en los botones, porque el hilo
/// espera acá: el click se atiende justo después del próximo tick.
const INTERVAL_MS: u32 = 100;

/// Cuánto se avanza o retrocede con los botones, en microsegundos.
const SEEK_STEP_US: i64 = 10_000_000;

/// Los nombres de bus de los proxies conocidos.
///
/// Un proxy refleja el reproductor de otro proceso: no es una sesión propia,
/// sino una copia de otra. `playerctld` es el caso habitual.
const PROXY_NAMES: &[&str] = &["playerctld"];

/// Qué se espera cuando no hay ningún reproductor en el bus antes de
/// volver a buscar uno.
const RETRY_WITHOUT_PLAYER: Duration = Duration::from_secs(2);

/// Errores del backend.
#[derive(Debug)]
enum Error {
    Bus(mpris::DBusError),
    Find(mpris::FindingError),
    Dbus(dbus::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bus(err) => write!(f, "error de D-Bus: {err}"),
            Self::Find(err) => write!(f, "no se encontró un reproductor: {err}"),
            Self::Dbus(err) => write!(f, "error del bus: {err}"),
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

impl From<dbus::Error> for Error {
    fn from(err: dbus::Error) -> Self {
        Self::Dbus(err)
    }
}

/// Qué tiene que hacer el bucle después de atender al reproductor.
enum Outcome {
    /// Seguir con el mismo reproductor.
    Keep,
    /// El reproductor terminó: buscar otro.
    Gone,
    /// El usuario eligió otra sesión.
    Switch(String),
}

/// El estado que el bucle comparte entre iteraciones.
struct State {
    finder: PlayerFinder,
    /// Las sesiones disponibles.
    sessions: Vec<Session>,
    /// El bus avisó que cambió el conjunto de reproductores.
    dirty: Arc<AtomicBool>,
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

    // El conjunto de reproductores cambia cuando alguien entra o sale del
    // bus. `NameOwnerChanged` lo avisa; `find_all()` es caro (una
    // introspección por reproductor), así que no se consulta en cada tick.
    let mut state = State {
        finder,
        sessions: Vec::new(),
        dirty: Arc::new(AtomicBool::new(true)),
    };
    watch_bus(state.dirty.clone());

    let mut current = pick(&mut state, None, None);
    let mut last: Option<String> = None;

    loop {
        let Some(player) = current.as_ref() else {
            // No hay ningún reproductor: avisar y reintentar más tarde.
            sink(Snapshot::no_session());
            std::thread::sleep(RETRY_WITHOUT_PLAYER);
            current = pick(&mut state, last.as_deref(), None);
            continue;
        };

        last = Some(player.bus_name().to_string());
        match handle(player, &cmd_rx, &sink, &mut state)? {
            Outcome::Keep => {}
            Outcome::Gone => {
                eprintln!("[media] el reproductor se fue: {}", player.bus_name());
                // Se busca otro del mismo programa, pero no el que se acaba
                // de ir: si vuelve a aparecer en el bus, el bucle se
                // engancharía con él.
                current = pick(&mut state, Some(player.identity()), Some(player.bus_name()));
            }
            Outcome::Switch(id) => {
                eprintln!("[media] se elige otra sesión: {id}");
                current = pick(&mut state, Some(&id), None);
            }
        }
    }
}

/// Mira el bus para saber cuándo cambia el conjunto de reproductores.
///
/// Va en su propio hilo con su propia conexión: la del backend la usa el
/// crate `mpris` para hablar con los reproductores.
fn watch_bus(dirty: Arc<AtomicBool>) {
    let _ = std::thread::Builder::new()
        .name("mpris-watch".to_string())
        .spawn(move || {
            let conn = match dbus::blocking::Connection::new_session() {
                Ok(conn) => conn,
                Err(err) => {
                    eprintln!("[media] no se pudo mirar el bus: {err}");
                    return;
                }
            };
            let rule = MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged");
            if conn
                .add_match::<NameOwnerChanged, _>(rule, move |signal, _conn, _msg| {
                    // Solo los reproductores MPRIS nos interesan: el bus de
                    // sesión cambia por un montón de cosas más.
                    if !signal.name.starts_with("org.mpris.MediaPlayer2") {
                        return true;
                    }
                    eprintln!(
                        "[media] cambió el reproductor {} ({} → {})",
                        signal.name, signal.old_owner, signal.new_owner
                    );
                    dirty.store(true, Ordering::Relaxed);
                    true
                })
                .is_err()
            {
                return;
            }
            // Atiende el bus para siempre: la señal llega por acá.
            loop {
                if let Err(err) = conn.process(Duration::from_millis(500)) {
                    eprintln!("[media] se cortó la escucha del bus: {err}");
                    return;
                }
            }
        });
}

/// Atiende el reproductor actual hasta que termina.
fn handle(
    player: &Player,
    cmd_rx: &Receiver<Command>,
    sink: &Sink,
    state: &mut State,
) -> Result<Outcome, Error> {
    let mut tracker = player.track_progress(INTERVAL_MS)?;
    eprintln!(
        "[media] reproductor MPRIS: {} ({})",
        player.identity(),
        player.bus_name()
    );

    loop {
        // Espera la señal de D-Bus o el fin del intervalo, lo que pase
        // primero. Las propiedades solo se releen si llegó algo.
        let tick = tracker.tick();

        // Los comandos de la UI se atienden sin bloquear: `tick()` ya
        // esperó el intervalo.
        let mut outcome = Outcome::Keep;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Command::TogglePlayPause => {
                    if let Err(err) = player.play_pause() {
                        eprintln!("[media] play/pausa falló: {err}");
                    }
                }
                Command::SeekBackward => {
                    if let Err(err) = player.seek(-SEEK_STEP_US) {
                        eprintln!("[media] retroceder falló: {err}");
                    }
                }
                Command::SeekForward => {
                    if let Err(err) = player.seek(SEEK_STEP_US) {
                        eprintln!("[media] avanzar falló: {err}");
                    }
                }
                Command::SelectSession(id) => {
                    outcome = Outcome::Switch(id);
                    break;
                }
            }
        }

        // El bus avisó que cambió el conjunto de reproductores: la lista de
        // sesiones se relee, pero solo de una vez.
        if state.dirty.swap(false, Ordering::Relaxed) {
            refresh(state);
        }

        if matches!(outcome, Outcome::Switch(_)) {
            return Ok(outcome);
        }
        if tick.player_quit {
            return Ok(Outcome::Gone);
        }

        sink(snapshot(tick.progress, player, &state.sessions));
    }
}

/// Elige con qué reproductor trabajar.
///
/// `preferred` es lo que se busca primero: el `id` de la sesión que eligió el
/// usuario, o —si el reproductor se fue— el nombre del programa, para seguir
/// con la misma aplicación aunque sea otra instancia.
///
/// `exclude` es el reproductor que acaba de irse: no se vuelve a elegir, o el
/// bucle se engancharía con él.
///
/// Si no hay ninguno de los dos, se prefiere el que está sonando; si hay
/// varios pausados, el último que se vio: es el que el usuario está mirando.
fn pick(state: &mut State, preferred: Option<&str>, exclude: Option<&str>) -> Option<Player> {
    let all: Vec<Player> = refresh(state)
        .into_iter()
        .filter(|p| Some(p.bus_name()) != exclude)
        .collect();
    if all.is_empty() {
        return None;
    }

    // 1. El preferido, si sigue en la lista.
    let chosen = preferred
        .and_then(|want| all.iter().position(|p| p.bus_name() == want))
        // 2. El mismo programa, si es otro el que se fue.
        .or_else(|| preferred.and_then(|want| all.iter().position(|p| p.identity() == want)))
        // 3. El que está sonando.
        .or_else(|| {
            all.iter()
                .position(|p| matches!(p.get_playback_status(), Ok(PlaybackStatus::Playing)))
        })
        // 4. El primero que exista.
        .unwrap_or(0);

    all.into_iter().nth(chosen)
}

/// Relee el bus y deja la lista de sesiones al día.
///
/// Devuelve los reproductores que hay, sin los proxies.
fn refresh(state: &mut State) -> Vec<Player> {
    match state.finder.find_all() {
        Ok(all) => {
            let players = without_proxies(all);
            state.sessions = build_sessions(&players);
            players
        }
        Err(err) => {
            eprintln!("[media] no se pudieron listar los reproductores: {err}");
            Vec::new()
        }
    }
}

/// Quita los proxies: nombres de bus que reflejan el reproductor de otro.
///
/// `playerctld` se anuncia con su propio nombre de bus pero con la identidad
/// del reproductor que refleja. Si no se filtra, el mismo reproductor
/// aparece dos veces en el selector.
fn without_proxies(all: Vec<Player>) -> Vec<Player> {
    all.into_iter()
        .filter(|p| !is_proxy(p.bus_name()))
        .collect()
}

/// Si el nombre de bus corresponde a un proxy conocido.
fn is_proxy(bus_name: &str) -> bool {
    let tail = bus_name.rsplit('.').next().unwrap_or(bus_name);
    PROXY_NAMES.iter().any(|p| tail.eq_ignore_ascii_case(p))
}

/// Arma la lista de sesiones a partir de los reproductores del bus.
///
/// Cada reproductor es una sesión: dos ventanas del mismo navegador o dos
/// instancias de mpv son sesiones distintas, aunque compartan nombre. Por eso
/// la clave es el nombre de bus y no la identidad.
///
/// Cuando varias sesiones comparten nombre se numeran, para que el usuario
/// pueda distinguirlas.
fn build_sessions(players: &[Player]) -> Vec<Session> {
    let raw: Vec<(String, String)> = players
        .iter()
        .map(|p| (p.bus_name().to_string(), p.identity().to_string()))
        .collect();
    sessions_from(&raw)
}

/// Como [`build_sessions`], pero sobre datos planos: así se puede probar sin
/// un bus.
fn sessions_from(raw: &[(String, String)]) -> Vec<Session> {
    let mut sessions: Vec<Session> = raw
        .iter()
        .map(|(id, label)| Session {
            id: id.clone(),
            label: label.clone(),
        })
        .collect();

    // Orden estable: por etiqueta y después por id. Así los números de las
    // sesiones repetidas no cambian entre una foto y otra.
    sessions.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.id.cmp(&b.id)));

    // Numerar las que comparten etiqueta.
    let mut i = 0;
    while i < sessions.len() {
        let mut j = i + 1;
        while j < sessions.len() && sessions[j].label == sessions[i].label {
            j += 1;
        }
        if j - i > 1 {
            for (k, s) in sessions.iter_mut().enumerate().take(j).skip(i) {
                s.label = format!("{} ({})", s.label, k - i + 1);
            }
        }
        i = j;
    }

    sessions
}

fn snapshot(progress: &Progress, player: &Player, sessions: &[Session]) -> Snapshot {
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
        sessions: sessions.to_vec(),
        session_id: player.bus_name().to_string(),
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

    #[test]
    fn recognizes_the_known_proxies() {
        assert!(is_proxy("org.mpris.MediaPlayer2.playerctld"));
        assert!(is_proxy("org.mpris.MediaPlayer2.PLAYERCTLD"));
        assert!(!is_proxy("org.mpris.MediaPlayer2.mpv.instance-sLbacCqj"));
        assert!(!is_proxy("org.mpris.MediaPlayer2.firefox.instance_1_40"));
        // El nombre del proxy puede ser el prefijo de otro, pero no su cola.
        assert!(!is_proxy("org.mpris.MediaPlayer2.playerctld.helper"));
    }

    #[test]
    fn keeps_players_with_the_same_name_apart() {
        // Dos ventanas del mismo navegador: mismo nombre, sesiones distintas.
        let sesiones = sessions_from(&[
            (
                "org.mpris.MediaPlayer2.firefox.instance_1_40".to_string(),
                "Mozilla zen".to_string(),
            ),
            (
                "org.mpris.MediaPlayer2.firefox.instance_2_41".to_string(),
                "Mozilla zen".to_string(),
            ),
        ]);

        assert_eq!(sesiones.len(), 2);
        assert_eq!(
            sesiones[0].id,
            "org.mpris.MediaPlayer2.firefox.instance_1_40"
        );
        assert_eq!(sesiones[0].label, "Mozilla zen (1)");
        assert_eq!(
            sesiones[1].id,
            "org.mpris.MediaPlayer2.firefox.instance_2_41"
        );
        assert_eq!(sesiones[1].label, "Mozilla zen (2)");
    }

    #[test]
    fn leaves_unique_names_alone() {
        let sesiones = sessions_from(&[
            (
                "org.mpris.MediaPlayer2.mpv.instance-sLbacCqj".to_string(),
                "mpv".to_string(),
            ),
            (
                "org.mpris.MediaPlayer2.firefox.instance_1_40".to_string(),
                "Mozilla zen".to_string(),
            ),
        ]);

        assert_eq!(sesiones[0].label, "Mozilla zen");
        assert_eq!(sesiones[1].label, "mpv");
    }

    #[test]
    fn the_order_does_not_depend_on_the_order_of_the_bus() {
        let a = (
            "org.mpris.MediaPlayer2.firefox.instance_2_41".to_string(),
            "Mozilla zen".to_string(),
        );
        let b = (
            "org.mpris.MediaPlayer2.firefox.instance_1_40".to_string(),
            "Mozilla zen".to_string(),
        );

        let uno = sessions_from(&[a.clone(), b.clone()]);
        let otro = sessions_from(&[b, a]);

        assert_eq!(uno, otro);
        assert_eq!(uno[0].id, "org.mpris.MediaPlayer2.firefox.instance_1_40");
    }
}
