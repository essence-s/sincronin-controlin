//! Caso de uso: publicación de estado, supresión de echo e interpolación.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::ports::{MediaPort, NetPort, UiPort};
use crate::domain::echo::{is_echo, is_expired};
use crate::domain::interpolation::RemotePosition;
use crate::domain::media::{Command, PlaybackState, Snapshot};

/// Estado remoto parseado, para pintar en la UI del controlador.
#[derive(Clone, Debug)]
pub struct RemoteState {
    pub has_session: bool,
    pub playback: PlaybackState,
    pub position_ticks: i64,
    pub duration_ticks: i64,
    pub playback_rate: f64,
    pub title: String,
    pub artist: String,
}

/// Servicio de estado: publica estado local, suprime ecos, interpola remoto.
pub struct StateService<M: MediaPort, N: NetPort, U: UiPort> {
    media: M,
    net: N,
    ui: U,
    publisher: StatePublisher,
    pending: Mutex<Option<crate::domain::echo::PendingCommand>>,
    remote_position: Mutex<Option<RemotePosition>>,
}

/// Filtra publicaciones de estado para no mandar posición a 10 Hz.
struct StatePublisher {
    last_observed: Option<(Snapshot, Instant)>,
    last_sent: Option<Instant>,
}

const STATE_INTERVAL: Duration = Duration::from_secs(1);
const SEEK_TOLERANCE_TICKS: i64 = 10_000_000;

impl StatePublisher {
    fn new() -> Self {
        Self {
            last_observed: None,
            last_sent: None,
        }
    }

    fn reset(&mut self, snapshot: &Snapshot, now: Instant) {
        self.last_observed = Some((snapshot.clone(), now));
        self.last_sent = Some(now);
    }

    fn should_publish(&mut self, snapshot: &Snapshot, now: Instant) -> bool {
        let previous = self.last_observed.take();
        let (discrete, seek) = match previous {
            Some((previous, observed_at)) => (
                is_discrete_change(&previous, snapshot),
                is_seek(&previous, observed_at, snapshot, now),
            ),
            None => (true, false),
        };
        self.last_observed = Some((snapshot.clone(), now));

        let due = snapshot.state == PlaybackState::Playing
            && self
                .last_sent
                .is_none_or(|last| now.saturating_duration_since(last) >= STATE_INTERVAL);
        let publish = discrete || seek || due;

        if publish {
            self.last_sent = Some(now);
        }

        publish
    }
}

fn is_discrete_change(previous: &Snapshot, current: &Snapshot) -> bool {
    previous.has_session != current.has_session
        || previous.title != current.title
        || previous.artist != current.artist
        || previous.state != current.state
        || previous.duration_ticks != current.duration_ticks
        || (previous.playback_rate - current.playback_rate).abs() > f64::EPSILON
}

fn is_seek(
    previous: &Snapshot,
    observed_at: Instant,
    current: &Snapshot,
    now: Instant,
) -> bool {
    if !previous.has_session || !current.has_session {
        return false;
    }

    let elapsed = now.saturating_duration_since(observed_at);
    let advanced = if previous.state == PlaybackState::Playing {
        (elapsed.as_secs_f64() * 10_000_000.0).round() as i64
    } else {
        0
    };
    let expected = previous.position_ticks.saturating_add(advanced);

    current.position_ticks.abs_diff(expected) > SEEK_TOLERANCE_TICKS as u64
}

impl<M: MediaPort, N: NetPort, U: UiPort> StateService<M, N, U> {
    /// Crea un nuevo servicio con los puertos inyectados.
    pub fn new(media: M, net: N, ui: U) -> Self {
        Self {
            media,
            net,
            ui,
            publisher: StatePublisher::new(),
            pending: Mutex::new(None),
            remote_position: Mutex::new(None),
        }
    }

    /// El backend de medios publicó un nuevo snapshot local.
    ///
    /// Aplica supresión de echo y rate-limit antes de publicar por red.
    pub fn on_local_snapshot(&mut self, snapshot: Snapshot, now: Instant) {
        let mut pending = self.pending.lock().expect("el comando pendiente no está poisoned");
        let is_echo = pending.as_ref().is_some_and(|p| is_echo(p, &snapshot));
        if is_echo {
            *pending = None;
            return;
        }
        if pending.as_ref().is_some_and(|p| is_expired(p, now)) {
            *pending = None;
        }
        drop(pending);

        if self.publisher.should_publish(&snapshot, now) {
            let _ = self.net.send_data(serialize_state(&snapshot));
        }
    }

    /// Llegó un comando remoto por el canal de datos.
    ///
    /// Lo registra como pendiente para suprimir su eco y lo aplica localmente.
    pub fn on_remote_command(&self, text: String) {
        if let Some(cmd) = parse_media_command(&text) {
            let now = Instant::now();
            let pending = crate::domain::echo::PendingCommand {
                action: match &cmd {
                    Command::Play => "play".to_string(),
                    Command::Pause => "pause".to_string(),
                    Command::Stop => "stop".to_string(),
                    Command::SeekBackward => "seek_backward".to_string(),
                    Command::SeekForward => "seek_forward".to_string(),
                    Command::SeekTo(_) => "seek_to".to_string(),
                    Command::SelectSession(_) => "select_session".to_string(),
                },
                target_ticks: match &cmd {
                    Command::SeekTo(ticks) => Some(*ticks),
                    _ => None,
                },
                received_at: now,
            };
            *self
                .pending
                .lock()
                .expect("el comando pendiente no está poisoned") = Some(pending);
            self.media.send_command(cmd);
        }
    }

    /// Llegó un estado remoto por el canal de datos (solo para el controlador).
    pub fn on_remote_state(&self, text: String) {
        if let Some(state) = parse_state_message(&text) {
            self.ui.set_has_session(state.has_session);
            self.ui.set_playing(state.playback == PlaybackState::Playing);
            let titulo = if !state.has_session {
                "Sin reproducción".to_string()
            } else if state.title.is_empty() {
                "Desconocido".to_string()
            } else {
                state.title
            };
            self.ui.set_track_title(titulo);
            self.ui.set_track_artist(state.artist);
            let now = Instant::now();
            let mut rp = self
                .remote_position
                .lock()
                .expect("el interpolador no está poisoned");
            let position_ticks = match rp.as_mut() {
                Some(pos) => {
                    pos.update(
                        state.position_ticks,
                        state.playback_rate,
                        state.playback,
                        now,
                    );
                    pos.position(now)
                }
                None => {
                    *rp = Some(RemotePosition::new(
                        state.position_ticks,
                        state.playback_rate,
                        state.playback,
                        now,
                    ));
                    state.position_ticks
                }
            };
            self.ui.set_position_secs(segundos(position_ticks));
            self.ui.set_duration_secs(segundos(state.duration_ticks));
        }
    }

    /// Reinicia el publicador con la foto cacheada (al abrirse el canal).
    pub fn reset_publisher(&mut self, snapshot: &Snapshot, now: Instant) {
        self.publisher.reset(snapshot, now);
    }
}

/// Convierte ticks de 100 ns a segundos para la UI.
fn segundos(ticks: i64) -> i32 {
    let s = ticks / 10_000_000;
    i32::try_from(s).unwrap_or(if s > 0 { i32::MAX } else { i32::MIN })
}

/// Traduce un snapshot al JSON que viaja por el canal de datos.
fn serialize_state(snapshot: &Snapshot) -> String {
    let playback = match snapshot.state {
        PlaybackState::Playing => "playing",
        PlaybackState::Paused => "paused",
        PlaybackState::Stopped => "stopped",
        PlaybackState::Unknown => "unknown",
    };

    serde_json::json!({
        "type": "state",
        "has_session": snapshot.has_session,
        "playback_status": playback,
        "position_ms": snapshot.position_ticks / 10_000,
        "duration_ms": snapshot.duration_ticks / 10_000,
        "playback_rate": snapshot.playback_rate,
        "title": &snapshot.title,
        "artist": &snapshot.artist,
    })
    .to_string()
}

/// Lee un mensaje de estado. Devuelve `None` si no es un estado válido.
fn parse_state_message(text: &str) -> Option<RemoteState> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("type")?.as_str()? != "state" {
        return None;
    }

    let playback = match value.get("playback_status")?.as_str()? {
        "playing" => PlaybackState::Playing,
        "paused" => PlaybackState::Paused,
        "stopped" => PlaybackState::Stopped,
        _ => PlaybackState::Unknown,
    };

    Some(RemoteState {
        has_session: value.get("has_session")?.as_bool()?,
        playback,
        position_ticks: value.get("position_ms")?.as_i64()?.saturating_mul(10_000),
        duration_ticks: value.get("duration_ms")?.as_i64()?.saturating_mul(10_000),
        playback_rate: value
            .get("playback_rate")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0),
        title: value.get("title")?.as_str()?.to_string(),
        artist: value.get("artist")?.as_str()?.to_string(),
    })
}

/// Traduce un mensaje de texto del canal de datos a un comando de medios.
fn parse_media_command(text: &str) -> Option<Command> {
    let text = text.trim();
    match text {
        "play" => Some(Command::Play),
        "pause" => Some(Command::Pause),
        "stop" => Some(Command::Stop),
        "seek_backward" => Some(Command::SeekBackward),
        "seek_forward" => Some(Command::SeekForward),
        _ if text.starts_with("seek_to ") => {
            let rest = text["seek_to ".len()..].trim();
            rest.parse::<i64>().ok().map(Command::SeekTo)
        }
        _ => None,
    }
}
