//! Composición de la aplicación: conecta adaptadores, servicios y UI.
//!
//! Es la única capa que conoce las dos caras a la vez: la UI de Slint y los
//! adaptadores de infraestructura. La lógica de negocio vive en los
//! servicios de [`services`]; la integración con el sistema, en
//! [`platform`]; la lógica de la red, en [`net`] y [`adapters::net`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::adapters::media::MediaAdapter;
use crate::adapters::net::NetAdapter;
use crate::adapters::ui::slint::SlintAdapter;
use crate::ports::{MediaPort, NetPort, UiPort};
use crate::domain::media::{Command, PlaybackState, Snapshot};
use crate::domain::net::{NetEvent as DomainNetEvent, NetState as DomainNetState};
use crate::net::{self, Role as NetRole};
use crate::platform;
use crate::services::media::MediaService;
use crate::services::state::{RemoteState, StateService};
use crate::{AppWindow, MediaSession};

/// Cuántas líneas guarda el registro de la ventana de red.
const MAX_LOG_LINES: usize = 40;

/// El mensaje de prueba que manda el botón.
const TEST_MESSAGE: &str = "hola";

pub fn run() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;

    // ── Adaptadores de infraestructura ────────────────────────────────
    let net = NetAdapter::spawn();
    let media = MediaAdapter::spawn();

    // ── Servicios de la capa de aplicación ────────────────────────────
    let media_service = MediaService::new(media.clone());

    // Si el cliente es el controlador de la sala. Se fija al recibir el
    // rol del servidor y se usa para habilitar o deshabilitar los
    // controles de reproducción en la UI.
    let is_controlador = Arc::new(AtomicBool::new(false));
    let is_controlador_media = is_controlador.clone();
    let is_controlador_net = is_controlador.clone();

    // SlintAdapter implementa UiPort: se le pasa a StateService para que
    // el servicio pueda actualizar la UI cuando llega estado remoto.
    let slint = SlintAdapter::new(&ui, net.clone(), media.clone());
    let slint_arc: Arc<dyn UiPort> = Arc::new(slint);

    let state = StateService::new(media.clone(), net.clone(), Arc::clone(&slint_arc));
    let state = Arc::new(Mutex::new(state));

    let ui_weak_media = ui.as_weak();

    // ── Backend de medios → StateService + UI ─────────────────────────
    let state_media = Arc::clone(&state);
    media.on_snapshot(Box::new(move |snapshot: Snapshot| {
        let now = Instant::now();
        let is_ctrl = is_controlador_media.load(Ordering::Relaxed);
        let ui_weak = ui_weak_media.clone();
        // Publicar por red (rate limiting, eco, etc.).
        state_media
            .lock()
            .expect("el servicio de estado no está poisoned")
            .on_local_snapshot(snapshot.clone(), now);
        // Actualizar la UI con el snapshot local.
        ui_weak.upgrade_in_event_loop(move |ui| {
            apply(&ui, &snapshot, is_ctrl);
        });
    }));

    let is_controlador_net = is_controlador.clone();

    // ── Red → eventos de dominio → StateService / UI ─────────────────
    let state_net = Arc::clone(&state);
    let media_net = media_service.clone();
    let weak_net = ui.as_weak();
    let weak_toggle = ui.as_weak();
    let weak_connect = ui.as_weak();
    net.on_event(Box::new(move |event: DomainNetEvent| {
        let ui_weak = weak_net.clone();
        let is_controlador = is_controlador_net.clone();
        match event {
            DomainNetEvent::State(net_state) => {
                let connected = net_state == DomainNetState::Conectado;
                let texto = match net_state {
                    DomainNetState::Desconectado => "Desconectado",
                    DomainNetState::Conectando => "Esperando al otro equipo",
                    DomainNetState::Conectado => "Conectado",
                };
                if let Some(handle) = ui_weak.upgrade() {
                    handle.set_state_text(texto.into());
                    handle.set_can_send(connected);
                }
            }
            DomainNetEvent::AssignedRole(role) => {
                let is_ctrl = role == crate::domain::room::Role::Controlador;
                is_controlador.store(is_ctrl, Ordering::Relaxed);
                let texto = match role {
                    crate::domain::room::Role::Controlado => "controlado",
                    crate::domain::room::Role::Controlador => "controlador",
                };
                if let Some(handle) = ui_weak.upgrade() {
                    handle.set_role_text(texto.into());
                }
            }
            DomainNetEvent::Notice(text) => {
                append_log_to_ui(&ui_weak, &format!("· {text}"));
            }
            DomainNetEvent::Message(text) => {
                if let Some(cmd) = parse_media_command(&text) {
                    let now = Instant::now();
                    state_net
                        .lock()
                        .expect("el servicio de estado no está poisoned")
                        .on_remote_command(text);
                    // El controlado aplica localmente lo que el controlador le envió.
                    let _ = media_net.execute(cmd.clone());
                    let _ = now;
                } else if parse_state_message(&text).is_some() {
                    // Estado remoto: solo el controlador lo pinta en la UI.
                    state_net
                        .lock()
                        .expect("el servicio de estado no está poisoned")
                        .on_remote_state(text);
                } else {
                    append_log_to_ui(&ui_weak, &format!("← {text}"));
                }
            }
            DomainNetEvent::ControlledMoved { room } => {
                append_log_to_ui(
                    &ui_weak,
                    &format!("· el controlado se fue a la sala {room}: entrando"),
                );
                if let Some(handle) = ui_weak.upgrade() {
                    handle.set_room_code(room.as_str().into());
                    handle.set_peer_room("".into());
                    handle.set_net_open(true);
                    platform::configure(&handle);
                }
            }
            DomainNetEvent::ControlledLeft => {
                append_log_to_ui(
                    &ui_weak,
                    "· el controlado se fue: esperando que vuelva",
                );
                if let Some(handle) = ui_weak.upgrade() {
                    handle.set_peer_room("".into());
                    handle.set_net_open(true);
                    platform::configure(&handle);
                }
            }
        }
    }));

    // ── Callbacks de la UI ────────────────────────────────────────────
    // Los callbacks de Slint ya están conectados dentro de SlintAdapter::new
    // a PairingService y MediaService. Aquí solo conectamos lo que falta:
    // el panel de emparejamiento (net-toggled), connect, disconnect,
    // send-message y quit.

    {
        let net = net.clone();
        ui.on_net_toggled(move || {
            let ui_weak = weak_toggle.clone();
            let Some(handle) = ui_weak.upgrade() else {
                return;
            };
            ui_weak.upgrade_in_event_loop(move |ui| {
                platform::configure(&ui);
            });

            if handle.get_net_open() {
                // Mostrar el código de sala propia en el panel.
                handle.set_room_code(net::local_room_code().into());
                // Al abrir el panel, entrar a la sala propia.
                net.connect(None);
            }
        });
    }

    {
        let tx = net.clone();
        let weak_connect = ui.as_weak();
        ui.on_connect(move |room: SharedString| {
            let ui_weak = weak_connect.clone();
            let room = room.trim();
            let room = if room.is_empty() {
                net::local_room_code().to_string()
            } else {
                room.to_ascii_uppercase()
            };

            if let Some(handle) = ui_weak.upgrade() {
                handle.set_room_code(room.as_str().into());
            }

            let _ = tx.connect(Some(room.clone()));
        });
    }

    {
        let tx = net.clone();
        ui.on_disconnect(move || {
            let _ = tx.disconnect();
        });
    }

    {
        let tx = net.clone();
        ui.on_send_message(move || {
            let _ = tx.send_data(TEST_MESSAGE.to_string());
        });
    }

    {
        ui.on_quit(move || {
            let _ = slint::quit_event_loop();
        });
    }

    platform::configure(&ui);

    ui.run()
}

/// Agrega una línea al registro del panel de red desde una Weak referencia.
fn append_log_to_ui(ui_weak: &slint::Weak<AppWindow>, line: &str) {
    let Some(handle) = ui_weak.upgrade() else {
        return;
    };
    let mut lines: Vec<String> = handle.get_log_text().lines().map(str::to_string).collect();
    lines.push(line.to_string());

    if lines.len() > MAX_LOG_LINES {
        lines.drain(..lines.len() - MAX_LOG_LINES);
    }

    handle.set_log_text(lines.join("\n").into());
}

/// Vuelca una fotografía del backend en las propiedades de la UI.
///
/// Se llama en el hilo de la UI, así que puede tocar el modelo de sesiones:
/// los modelos de Slint usan `Rc` y no se pueden mandar a otro hilo.
fn apply(ui: &AppWindow, snapshot: &Snapshot, is_controlador: bool) {
    ui.set_has_session(snapshot.has_session);
    ui.set_playing(snapshot.state == PlaybackState::Playing);
    let (can_toggle, can_seek) = media_controls_gated(
        if is_controlador {
            NetRole::Controlador
        } else {
            NetRole::Controlado
        },
        snapshot.can_toggle,
        snapshot.can_seek,
    );
    ui.set_can_toggle(can_toggle);
    ui.set_can_seek(can_seek);

    let titulo = if !snapshot.has_session {
        "Sin reproducción".to_string()
    } else if snapshot.title.is_empty() {
        "Desconocido".to_string()
    } else {
        snapshot.title.clone()
    };
    ui.set_track_title(titulo.into());

    let subtitulo = if !snapshot.has_session {
        String::new()
    } else if !snapshot.artist.is_empty() {
        snapshot.artist.clone()
    } else if !snapshot.album.is_empty() {
        snapshot.album.clone()
    } else {
        snapshot.source_app.clone()
    };
    ui.set_track_artist(subtitulo.into());

    ui.set_position_secs(segundos(snapshot.position_ticks));
    ui.set_duration_secs(segundos(snapshot.duration_ticks));

    // La sesión que se está controlando y las disponibles.
    ui.set_session_id(snapshot.session_id.clone().into());
    ui.set_session_count(snapshot.sessions.len() as i32);

    // El modelo solo se rehace cuando la lista cambia de verdad: reconstruirlo
    // en cada foto reiniciaría el selector cada 100 ms.
    let new: Vec<MediaSession> = snapshot
        .sessions
        .iter()
        .map(|s| MediaSession {
            id: SharedString::from(s.id.as_str()),
            label: SharedString::from(s.label.as_str()),
        })
        .collect();
    if changed(&ui.get_sessions(), &new) {
        ui.set_sessions(ModelRc::new(VecModel::from(new)));
    }
}

/// Si la lista de sesiones de la UI es distinta de la que llega del backend.
fn changed(current: &ModelRc<MediaSession>, new: &[MediaSession]) -> bool {
    if current.row_count() != new.len() {
        return true;
    }
    (0..new.len()).any(|i| match current.row_data(i) {
        Some(row) => row.id != new[i].id || row.label != new[i].label,
        None => true,
    })
}

/// Convierte ticks de 100 ns (unidad de `WinRT`) a segundos.
///
/// Los ticks son `i64` pero las propiedades de Slint son `i32`. El `try_from`
/// evita una truncación silenciosa si algún día un valor fuera absurdo.
fn segundos(ticks: i64) -> i32 {
    let s = ticks / 10_000_000;
    i32::try_from(s).unwrap_or(if s > 0 { i32::MAX } else { i32::MIN })
}

/// Traduce un mensaje de texto del canal de datos a un comando de medios.
///
/// Solo reconoce comandos simples de un solo comando. Un mensaje que no
/// sea un comando válido devuelve `None` y se ignora.
fn parse_media_command(text: &str) -> Option<crate::domain::media::Command> {
    let text = text.trim();
    match text {
        "play" => Some(crate::domain::media::Command::Play),
        "pause" => Some(crate::domain::media::Command::Pause),
        "stop" => Some(crate::domain::media::Command::Stop),
        "seek_backward" => Some(crate::domain::media::Command::SeekBackward),
        "seek_forward" => Some(crate::domain::media::Command::SeekForward),
        _ if text.starts_with("seek_to ") => {
            let rest = text["seek_to ".len()..].trim();
            rest.parse::<i64>().ok().map(crate::domain::media::Command::SeekTo)
        }
        _ => None,
    }
}

/// Decide cuándo el controlado publica cada snapshot por la red.
struct StatePublisher {
    last_observed: Option<(Snapshot, Instant)>,
    last_sent: Option<Instant>,
}

impl StatePublisher {
    fn new() -> Self {
        Self {
            last_observed: None,
            last_sent: None,
        }
    }

    /// Marca una foto como enviada sin pasar por el filtro.
    ///
    /// Se usa al abrirse el canal, cuando el controlado manda la última foto
    /// cacheada: el siguiente snapshot de posición no tiene que salir de
    /// inmediato.
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

/// Campos del estado que no cambian con solo avanzar la posición.
fn is_discrete_change(previous: &Snapshot, current: &Snapshot) -> bool {
    previous.has_session != current.has_session
        || previous.title != current.title
        || previous.artist != current.artist
        || previous.state != current.state
        || previous.duration_ticks != current.duration_ticks
        || (previous.playback_rate - current.playback_rate).abs() > f64::EPSILON
}

/// Detecta un salto de posición que no explica el avance normal.
fn is_seek(previous: &Snapshot, observed_at: Instant, current: &Snapshot, now: Instant) -> bool {
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

/// Intervalo mínimo entre publicaciones de posición por red.
const STATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Tolerancia para distinguir un seek de un avance normal de reproducción.
const SEEK_TOLERANCE_TICKS: i64 = 10_000_000;

/// Decide cuáles controles de medios están habilitados según el rol.
fn media_controls_gated(role: NetRole, can_toggle: bool, can_seek: bool) -> (bool, bool) {
    if role == NetRole::Controlado {
        (false, false)
    } else {
        (can_toggle, can_seek)
    }
}

/// Arma el mensaje de estado que viaja por el canal de datos.
fn state_message(snapshot: &Snapshot) -> String {
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

/// Lo que puede llegar por el canal de datos.
#[derive(Clone, Debug, PartialEq)]
enum Incoming {
    State(RemoteState),
    Command(crate::domain::media::Command),
    Ignored,
}

/// Clasifica un mensaje del canal de datos.
fn route_message(text: &str) -> Incoming {
    if let Some(state) = parse_state_message(text) {
        Incoming::State(state)
    } else if let Some(command) = parse_media_command(text) {
        Incoming::Command(command)
    } else {
        Incoming::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controlado_disables_media_controls() {
        let (toggle, seek) = media_controls_gated(NetRole::Controlado, true, true);
        assert_eq!((toggle, seek), (false, false));
    }

    #[test]
    fn controlador_enables_media_controls_when_session() {
        let (toggle, seek) = media_controls_gated(NetRole::Controlador, true, true);
        assert_eq!((toggle, seek), (true, true));
    }

    #[test]
    fn controlador_disables_media_controls_when_no_session() {
        let (toggle, seek) = media_controls_gated(NetRole::Controlador, false, false);
        assert_eq!((toggle, seek), (false, false));
    }

    #[test]
    fn parse_media_command_play() {
        let cmd = parse_media_command("play");
        assert_eq!(cmd, Some(crate::domain::media::Command::Play));
    }

    #[test]
    fn parse_media_command_ignores_invalid() {
        let cmd = parse_media_command("hola");
        assert_eq!(cmd, None);
        let cmd = parse_media_command("");
        assert_eq!(cmd, None);
    }

    #[test]
    fn parse_media_command_pause() {
        let cmd = parse_media_command("pause");
        assert_eq!(cmd, Some(crate::domain::media::Command::Pause));
    }

    #[test]
    fn parse_media_command_stop() {
        let cmd = parse_media_command("stop");
        assert_eq!(cmd, Some(crate::domain::media::Command::Stop));
    }

    #[test]
    fn parse_media_command_seek_to() {
        let cmd = parse_media_command("seek_to 125000");
        assert_eq!(cmd, Some(crate::domain::media::Command::SeekTo(125000)));
    }

    #[test]
    fn state_message_carries_the_track_and_playback() {
        let snapshot = Snapshot {
            has_session: true,
            title: "Canción".to_string(),
            artist: "Artista".to_string(),
            state: PlaybackState::Playing,
            position_ticks: 1_250_000_000,
            duration_ticks: 36_000_000_000,
            ..Snapshot::no_session()
        };

        let text = state_message(&snapshot);
        let remote = parse_state_message(&text).expect("el estado se entiende");

        assert!(remote.has_session);
        assert_eq!(remote.title, "Canción");
        assert_eq!(remote.artist, "Artista");
        assert_eq!(remote.playback, PlaybackState::Playing);
    }

    #[test]
    fn state_message_carries_the_position_and_duration() {
        let snapshot = Snapshot {
            has_session: true,
            position_ticks: 1_250_000_000,
            duration_ticks: 36_000_000_000,
            ..Snapshot::no_session()
        };

        let text = state_message(&snapshot);
        let json: serde_json::Value = serde_json::from_str(&text).expect("es JSON");
        assert_eq!(json["position_ms"].as_i64(), Some(125_000));
        assert_eq!(json["duration_ms"].as_i64(), Some(3_600_000));

        let remote = parse_state_message(&text).expect("el estado se entiende");
        assert_eq!(remote.position_ticks, 1_250_000_000);
        assert_eq!(remote.duration_ticks, 36_000_000_000);
    }

    #[test]
    fn state_message_carries_the_playback_rate() {
        let snapshot = Snapshot {
            playback_rate: 1.25,
            ..playing(0)
        };

        let text = state_message(&snapshot);
        let remote = parse_state_message(&text).expect("el estado se entiende");

        assert_eq!(remote.playback_rate, 1.25);
    }

    #[test]
    fn state_message_is_not_parsed_as_a_command() {
        let snapshot = Snapshot {
            has_session: true,
            title: "Canción".to_string(),
            ..Snapshot::no_session()
        };
        let text = state_message(&snapshot);

        match route_message(&text) {
            Incoming::State(state) => assert_eq!(state.title, "Canción"),
            otro => panic!("el estado no se tiene que tratar como comando: {otro:?}"),
        }

        match route_message("play") {
            Incoming::Command(Command::Play) => {}
            otro => panic!("un comando se sigue tratando como comando: {otro:?}"),
        }

        assert!(matches!(route_message("hola"), Incoming::Ignored));
    }

    #[test]
    fn state_message_reports_when_there_is_no_session() {
        let text = state_message(&Snapshot::no_session());
        let remote = parse_state_message(&text).expect("el estado se entiende");

        assert!(!remote.has_session);
        assert_eq!(remote.title, "");
        assert_eq!(remote.artist, "");
        assert_eq!(remote.position_ticks, 0);
        assert_eq!(remote.duration_ticks, 0);
    }

    fn playing(position_ticks: i64) -> Snapshot {
        Snapshot {
            has_session: true,
            title: "Canción".to_string(),
            artist: "Artista".to_string(),
            state: PlaybackState::Playing,
            position_ticks,
            duration_ticks: 100_000_000,
            ..Snapshot::no_session()
        }
    }

    #[test]
    fn position_updates_are_published_at_most_once_per_second() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();

        assert!(publisher.should_publish(&playing(0), start));
        assert!(!publisher.should_publish(
            &playing(1_000_000),
            start + std::time::Duration::from_millis(100)
        ));
        assert!(!publisher.should_publish(
            &playing(9_000_000),
            start + std::time::Duration::from_millis(900)
        ));
        assert!(publisher.should_publish(
            &playing(10_000_000),
            start + std::time::Duration::from_secs(1)
        ));
    }

    #[test]
    fn discrete_changes_are_published_immediately() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();
        let base = playing(0);
        assert!(publisher.should_publish(&base, start));

        let mut changed = base.clone();
        changed.title = "Otra canción".to_string();
        assert!(publisher.should_publish(
            &changed,
            start + std::time::Duration::from_millis(100)
        ));

        let mut changed = changed.clone();
        changed.artist = "Otro artista".to_string();
        assert!(publisher.should_publish(
            &changed,
            start + std::time::Duration::from_millis(200)
        ));

        let mut changed = changed.clone();
        changed.state = PlaybackState::Paused;
        assert!(publisher.should_publish(
            &changed,
            start + std::time::Duration::from_millis(300)
        ));

        let mut changed = changed.clone();
        changed.duration_ticks += 10_000_000;
        assert!(publisher.should_publish(
            &changed,
            start + std::time::Duration::from_millis(400)
        ));

        let mut changed = changed.clone();
        changed.has_session = false;
        assert!(publisher.should_publish(
            &changed,
            start + std::time::Duration::from_millis(500)
        ));
    }

    #[test]
    fn seeks_are_published_immediately() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();
        assert!(publisher.should_publish(&playing(0), start));

        assert!(publisher.should_publish(
            &playing(30 * 10_000_000),
            start + std::time::Duration::from_millis(100)
        ));
        assert!(
            publisher.should_publish(&playing(5 * 10_000_000), start + std::time::Duration::from_millis(200))
        );
    }

    #[test]
    fn paused_playback_does_not_publish_position_updates() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();
        let paused = Snapshot {
            has_session: true,
            state: PlaybackState::Paused,
            position_ticks: 5_000_000,
            ..Snapshot::no_session()
        };

        assert!(publisher.should_publish(&paused, start));
        assert!(!publisher.should_publish(&paused, start + std::time::Duration::from_millis(100)));
        assert!(!publisher.should_publish(&paused, start + std::time::Duration::from_secs(1)));
        assert!(!publisher.should_publish(&paused, start + std::time::Duration::from_secs(2)));
    }

    #[test]
    fn cached_snapshot_on_connect_starts_the_window() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();
        let cached = playing(0);

        publisher.reset(&cached, start);

        assert!(!publisher.should_publish(
            &playing(1_000_000),
            start + std::time::Duration::from_millis(100)
        ));
        assert!(publisher.should_publish(
            &playing(10_000_000),
            start + std::time::Duration::from_secs(1)
        ));
    }
}
