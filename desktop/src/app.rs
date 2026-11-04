//! Cableado entre la UI de Slint y los backends.
//!
//! La UI nunca toca `WinRT`: envía [`media::Command`] por un canal y recibe
//! [`media::Snapshot`] a través de un [`media::Sink`]. Lo mismo con la red:
//! [`net::Command`] y [`net::Event`].
//!
//! Es la única capa que conoce las dos caras a la vez. La lógica de medios
//! vive en [`media`] y la integración con el sistema en [`platform`]; la
//! lógica de la red, en [`net`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, unbounded};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::media::{Command, PlaybackState, Sink, Snapshot};
use crate::net;
use crate::platform;
use crate::{AppWindow, MediaSession};

/// Cuántas líneas guarda el registro de la ventana de red.
const MAX_LOG_LINES: usize = 40;

/// El mensaje de prueba que manda el botón.
const TEST_MESSAGE: &str = "hola";

/// Intervalo mínimo entre publicaciones de posición por red.
const STATE_INTERVAL: Duration = Duration::from_secs(1);

/// Tolerancia para distinguir un seek de un avance normal de reproducción.
const SEEK_TOLERANCE_TICKS: i64 = 10_000_000;

/// Periodo de gracia tras el cual un comando remoto pendiente se descarta
/// aunque no llegue el evento local coincidente.
const ECHO_GRACE_PERIOD: Duration = Duration::from_millis(300);

/// Comando remoto aplicado localmente, pendiente de correlación con el
/// evento local que genera para suprimir su eco.
struct PendingCommand {
    action: String,
    target_ticks: Option<i64>,
    received_at: Instant,
}

/// Decide si un snapshot local es el eco de un comando remoto pendiente.
fn is_echo(pending: &PendingCommand, snapshot: &Snapshot) -> bool {
    let Some(target) = pending.target_ticks else {
        return false;
    };
    pending.action == "seek_to" && snapshot.position_ticks == target
}

/// Decide si un comando pendiente ya expiró por periodo de gracia.
fn is_expired(pending: &PendingCommand, now: Instant) -> bool {
    now >= pending.received_at + ECHO_GRACE_PERIOD
}

pub fn run() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;

    // Canal UI -> backend (comandos).
    let (cmd_tx, cmd_rx) = unbounded::<Command>();

    // Canales de red. El de comandos se crea antes del sink de medios
    // porque el controlado publica su estado por ahí.
    let (net_cmd_tx, net_cmd_rx) = unbounded::<net::Command>();

    // Si el cliente ya está en una sala. Sirve para no salir de ella al
    // abrir el panel.
    let in_room = Arc::new(AtomicBool::new(false));

    // Si el cliente es el controlador de la sala. Se fija al recibir el
    // rol del servidor y se usa para habilitar o deshabilitar los
    // controles de reproducción en la UI.
    let is_controlador = Arc::new(AtomicBool::new(false));

    // Si el canal de datos está abierto. El controlado solo publica estado
    // cuando hay alguien al otro lado.
    let connected = Arc::new(AtomicBool::new(false));

    // La última foto del reproductor local. Se manda al abrirse el canal
    // porque el backend de Windows no publica si no hay un evento.
    let last_snapshot = Arc::new(Mutex::new(Snapshot::no_session()));

    // Filtra las publicaciones de estado para no mandar posición a 10 Hz.
    let state_publisher = Arc::new(Mutex::new(StatePublisher::new()));

    // Si el controlador está mostrando el estado del controlado. Mientras
    // tanto, las fotos de su propio reproductor no pisan la UI.
    let remote_active = Arc::new(AtomicBool::new(false));

    // Posición interpolada del controlado, persistente entre mensajes de red.
    let remote_position = Arc::new(Mutex::new(None::<RemotePosition>));

    // Comando remoto aplicado localmente, pendiente de correlación con el
    // evento local que genera para suprimir su eco.
    let pending_command = Arc::new(Mutex::new(None::<PendingCommand>));

    // Backend -> UI: cada evento del sistema publica una fotografía nueva.
    let sink: Sink = {
        let weak = ui.as_weak();
        let is_controlador = Arc::clone(&is_controlador);
        let connected = Arc::clone(&connected);
        let last_snapshot = Arc::clone(&last_snapshot);
        let state_publisher = Arc::clone(&state_publisher);
        let remote_active = Arc::clone(&remote_active);
        let net_cmd_tx = net_cmd_tx.clone();
        let pending_command = Arc::clone(&pending_command);
        std::sync::Arc::new(move |snapshot: Snapshot| {
            *last_snapshot.lock().expect("el cache no está poisoned") = snapshot.clone();

            // El controlado publica su estado mientras haya un controlador
            // conectado, pero la posición se limita en frecuencia.
            if !is_controlador.load(Ordering::Relaxed) && connected.load(Ordering::Relaxed) {
                let now = Instant::now();
                let mut pending = pending_command
                    .lock()
                    .expect("el comando pendiente no está poisoned");
                let is_echo = pending.as_ref().is_some_and(|p| is_echo(p, &snapshot));
                if is_echo {
                    // Es el eco del comando remoto: no se publica y se descarta.
                    *pending = None;
                } else if pending.as_ref().is_some_and(|p| is_expired(p, now)) {
                    // El comando pendiente expiró: se descarta y se publica el evento legítimo.
                    *pending = None;
                    let publish = state_publisher
                        .lock()
                        .expect("el publicador no está poisoned")
                        .should_publish(&snapshot, now);
                    if publish {
                        let _ = net_cmd_tx.send(net::Command::Send(state_message(&snapshot)));
                    }
                } else {
                    let publish = state_publisher
                        .lock()
                        .expect("el publicador no está poisoned")
                        .should_publish(&snapshot, now);
                    if publish {
                        let _ = net_cmd_tx.send(net::Command::Send(state_message(&snapshot)));
                    }
                }
            }

            let is_controlador = Arc::clone(&is_controlador);
            let remote_active = Arc::clone(&remote_active);
            let _ = weak.upgrade_in_event_loop(move |ui| {
                // Mientras haya estado remoto, el reproductor local del
                // controlador no manda en la pantalla.
                if !remote_active.load(Ordering::Relaxed) {
                    apply(&ui, &snapshot, is_controlador.load(Ordering::Relaxed));
                }
            });
        })
    };

    platform::spawn(cmd_rx, sink);

    // El canal de comandos de medios también se usa para ejecutar los
    // mensajes que llegan por el canal de datos (el controlado aplica
    // localmente lo que el controlador le envía).
    let media_tx = cmd_tx.clone();

    let net_sink: net::Sink = {
        let weak = ui.as_weak();
        let in_room = Arc::clone(&in_room);
        let is_controlador = Arc::clone(&is_controlador);
        let connected = Arc::clone(&connected);
        let last_snapshot = Arc::clone(&last_snapshot);
        let state_publisher = Arc::clone(&state_publisher);
        let remote_active = Arc::clone(&remote_active);
        let media_tx = media_tx.clone();
        let net_cmd_tx = net_cmd_tx.clone();
        let remote_position = Arc::clone(&remote_position);
        let pending_command = Arc::clone(&pending_command);
        Arc::new(move |event: net::Event| {
            if let net::Event::State(state) = &event {
                connected.store(*state == net::NetState::Conectado, Ordering::Relaxed);

                // El controlado manda su última foto al abrirse el canal.
                // En Windows el backend solo publica con eventos, así que
                // sin esto el controlador podría no ver nada hasta el
                // próximo cambio.
                if *state == net::NetState::Conectado && !is_controlador.load(Ordering::Relaxed) {
                    let snapshot = last_snapshot
                        .lock()
                        .expect("el cache no está poisoned")
                        .clone();
                    state_publisher
                        .lock()
                        .expect("el publicador no está poisoned")
                        .reset(&snapshot, Instant::now());
                    let _ = net_cmd_tx.send(net::Command::Send(state_message(&snapshot)));
                }
            }

            let in_room = Arc::clone(&in_room);
            let is_controlador = Arc::clone(&is_controlador);
            let media_tx = media_tx.clone();
            let remote_active = Arc::clone(&remote_active);
            let last_snapshot = Arc::clone(&last_snapshot);
            let remote_position = Arc::clone(&remote_position);
            let pending_command = Arc::clone(&pending_command);
            let _ = weak.upgrade_in_event_loop(move |ui| {
                apply_net(
                    &ui,
                    &event,
                    &in_room,
                    &is_controlador,
                    &media_tx,
                    &remote_active,
                    &last_snapshot,
                    &remote_position,
                    &pending_command,
                );
            });
        })
    };

    net::spawn(net_cmd_rx, net_sink);

    {
        let tx = net_cmd_tx.clone();
        let weak = ui.as_weak();
        ui.on_toggle_play_pause(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let playing = ui.get_playing();
            let cmd = if playing { "pause" } else { "play" };
            let _ = tx.send(net::Command::Send(cmd.to_string()));
        });
    }
    {
        let tx = net_cmd_tx.clone();
        ui.on_seek_backward(move || {
            let _ = tx.send(net::Command::Send("seek_backward".to_string()));
        });
    }
    {
        let tx = net_cmd_tx.clone();
        ui.on_seek_forward(move || {
            let _ = tx.send(net::Command::Send("seek_forward".to_string()));
        });
    }
    {
        let tx = net_cmd_tx.clone();
        ui.on_seek_to(move |secs: f32| {
            let ticks = (secs as f64 * 10_000_000.0) as i64;
            let _ = tx.send(net::Command::Send(format!("seek_to {ticks}")));
        });
    }
    {
        let tx = cmd_tx.clone();
        ui.on_select_session(move |session: SharedString| {
            let _ = tx.send(Command::SelectSession(session.to_string()));
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_quit(move || {
            if let Some(ui) = weak.upgrade() {
                let _ = ui.hide();
            }
            let _ = slint::quit_event_loop();
        });
    }

    // El panel de emparejamiento vive en la misma ventana que la barra.
    {
        let tx = net_cmd_tx.clone();
        let weak = ui.as_weak();
        ui.on_connect(move |room: SharedString| {
            // Si no escribió nada, se entra a la sala propia.
            let room = room.trim();
            let room = if room.is_empty() {
                net::local_room_code().to_string()
            } else {
                room.to_ascii_uppercase()
            };

            // El panel muestra la sala en la que estamos.
            if let Some(ui) = weak.upgrade() {
                ui.set_room_code(room.as_str().into());
            }

            let _ = tx.send(net::Command::Connect { room });
        });
    }
    {
        let tx = net_cmd_tx.clone();
        ui.on_disconnect(move || {
            let _ = tx.send(net::Command::Disconnect);
        });
    }
    {
        let tx = net_cmd_tx.clone();
        ui.on_send_message(move || {
            let _ = tx.send(net::Command::Send(TEST_MESSAGE.to_string()));
        });
    }
    {
        // La ventana cambió de alto (se abrió o se cerró el panel): hay
        // que volver a anclarla abajo.
        let weak = ui.as_weak();
        let tx = net_cmd_tx.clone();
        let in_room = Arc::clone(&in_room);
        ui.on_net_toggled(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };

            platform::configure(&ui);

            // Al abrir el panel, el cliente entra solo en su propia sala:
            // así el código que muestra está vivo y el otro equipo se
            // puede conectar sin pasos de por medio. Si ya estaba en una
            // sala, no se mueve de ella.
            if ui.get_net_open() && !in_room.load(Ordering::Relaxed) {
                ui.set_room_code(net::local_room_code().into());
                let _ = tx.send(net::Command::Connect {
                    room: String::new(),
                });
            }
        });
    }

    platform::configure(&ui);

    ui.run()
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
            net::Role::Controlador
        } else {
            net::Role::Controlado
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

/// Vuelca el estado que publica el controlado en la UI del controlador.
///
/// No toca las sesiones ni los controles: el selector y los botones siguen
/// siendo los del reproductor local del controlador. La posición se interpola
/// con [`RemotePosition`] para que la barra avance suavemente entre mensajes
/// de red en lugar de saltar a cada foto.
fn apply_remote(
    ui: &AppWindow,
    state: &RemoteState,
    remote_position: &Mutex<Option<RemotePosition>>,
) {
    let now = Instant::now();
    let mut rp = remote_position
        .lock()
        .expect("el interpolador remoto no está poisoned");
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

    ui.set_has_session(state.has_session);
    ui.set_playing(state.playback == PlaybackState::Playing);

    let titulo = if !state.has_session {
        "Sin reproducción".to_string()
    } else if state.title.is_empty() {
        "Desconocido".to_string()
    } else {
        state.title.clone()
    };
    ui.set_track_title(titulo.into());
    ui.set_track_artist(state.artist.clone().into());

    ui.set_position_secs(segundos(position_ticks));
    ui.set_duration_secs(segundos(state.duration_ticks));
}

/// Vuelca un evento de la red en el panel de emparejamiento.
#[allow(clippy::too_many_arguments)]
fn apply_net(
    ui: &AppWindow,
    event: &net::Event,
    in_room: &AtomicBool,
    is_controlador: &AtomicBool,
    media_tx: &Sender<Command>,
    remote_active: &AtomicBool,
    last_snapshot: &Mutex<Snapshot>,
    remote_position: &Mutex<Option<RemotePosition>>,
    pending_command: &Mutex<Option<PendingCommand>>,
) {
    match event {
        net::Event::State(state) => {
            in_room.store(*state != net::NetState::Desconectado, Ordering::Relaxed);
            ui.set_state_text(state.label().into());
            ui.set_can_send(*state == net::NetState::Conectado);

            // Si se cierra el canal, el estado remoto deja de ser válido:
            // se vuelve a la reproducción local.
            if *state != net::NetState::Conectado && remote_active.swap(false, Ordering::Relaxed) {
                let snapshot = last_snapshot
                    .lock()
                    .expect("el cache no está poisoned")
                    .clone();
                apply(ui, &snapshot, is_controlador.load(Ordering::Relaxed));
            }
        }
        net::Event::Message(text) => match route_message(text) {
            // Los comandos se ejecutan en el reproductor local: es el
            // controlado aplicando lo que el controlador le envió.
            Incoming::Command(cmd) => {
                // Registra el comando como pendiente para suprimir su eco
                // cuando llegue el evento local resultante.
                let now = Instant::now();
                let pending = match &cmd {
                    Command::Play => PendingCommand {
                        action: "play".to_string(),
                        target_ticks: None,
                        received_at: now,
                    },
                    Command::Pause => PendingCommand {
                        action: "pause".to_string(),
                        target_ticks: None,
                        received_at: now,
                    },
                    Command::Stop => PendingCommand {
                        action: "stop".to_string(),
                        target_ticks: None,
                        received_at: now,
                    },
                    Command::SeekBackward => PendingCommand {
                        action: "seek_backward".to_string(),
                        target_ticks: None,
                        received_at: now,
                    },
                    Command::SeekForward => PendingCommand {
                        action: "seek_forward".to_string(),
                        target_ticks: None,
                        received_at: now,
                    },
                    Command::SeekTo(ticks) => PendingCommand {
                        action: "seek_to".to_string(),
                        target_ticks: Some(*ticks),
                        received_at: now,
                    },
                    Command::SelectSession(_) => PendingCommand {
                        action: "select_session".to_string(),
                        target_ticks: None,
                        received_at: now,
                    },
                };
                *pending_command
                    .lock()
                    .expect("el comando pendiente no está poisoned") = Some(pending);
                let _ = media_tx.send(cmd);
            }
            // El estado remoto solo se pinta en el controlador. Un
            // controlado que lo reciba lo ignora.
            Incoming::State(state) => {
                if is_controlador.load(Ordering::Relaxed) {
                    remote_active.store(true, Ordering::Relaxed);
                    apply_remote(ui, &state, remote_position);
                }
            }
            Incoming::Ignored => append_log(ui, &format!("← {text}")),
        },
        net::Event::Notice(text) => {
            append_log(ui, &format!("· {text}"));
        }
        net::Event::AssignedRole(role) => {
            let texto = match role {
                net::Role::Controlado => "controlado",
                net::Role::Controlador => "controlador",
            };
            ui.set_role_text(texto.into());
            is_controlador.store(*role == net::Role::Controlador, Ordering::Relaxed);
        }
        // El controlado se fue con el código a la mano: la red ya lo sigue
        // sola, así que la sala nueva pasa a ser la propia.
        net::Event::ControlledMoved { room } => {
            append_log(
                ui,
                &format!("· el controlado se fue a la sala {room}: entrando"),
            );
            ui.set_room_code(room.as_str().into());
            ui.set_peer_room("".into());
            open_panel(ui);
        }
        // Sin destino, la sala quedó sin controlado. El cliente sigue en
        // ella esperando que el controlado vuelva solo; para irse a otra, se
        // escribe el código en el panel.
        net::Event::ControlledLeft => {
            append_log(ui, "· el controlado se fue: esperando que vuelva");
            ui.set_peer_room("".into());
            open_panel(ui);
        }
    }
}

/// Abre el panel de emparejamiento desde el código.
///
/// El alto de la ventana depende de `net-open`, y el re-anclaje de la
/// ventana vive en el callback `net-toggled`, que acá no se dispara: hay
/// que re-anclarla a mano.
fn open_panel(ui: &AppWindow) {
    if ui.get_net_open() {
        return;
    }

    ui.set_net_open(true);
    platform::configure(ui);
}

/// Agrega una línea al registro del panel de red.
///
/// El registro vive en la propiedad de la UI: no hace falta guardar nada
/// del otro lado. Se corta a las últimas líneas para que no crezca sin
/// fin.
fn append_log(ui: &AppWindow, line: &str) {
    let mut lines: Vec<String> = ui.get_log_text().lines().map(str::to_string).collect();
    lines.push(line.to_string());

    if lines.len() > MAX_LOG_LINES {
        lines.drain(..lines.len() - MAX_LOG_LINES);
    }

    ui.set_log_text(lines.join("\n").into());
}

fn media_controls_gated(role: net::Role, can_toggle: bool, can_seek: bool) -> (bool, bool) {
    if role == net::Role::Controlado {
        (false, false)
    } else {
        (can_toggle, can_seek)
    }
}

/// Traduce un mensaje de texto del canal de datos a un comando de medios.
///
/// Solo reconoce comandos simples de un solo comando. Un mensaje que no
/// sea un comando válido devuelve `None` y se ignora.
fn parse_media_command(text: &str) -> Option<crate::media::Command> {
    let text = text.trim();
    match text {
        "play" => Some(crate::media::Command::Play),
        "pause" => Some(crate::media::Command::Pause),
        "stop" => Some(crate::media::Command::Stop),
        "seek_backward" => Some(crate::media::Command::SeekBackward),
        "seek_forward" => Some(crate::media::Command::SeekForward),
        _ if text.starts_with("seek_to ") => {
            let rest = text["seek_to ".len()..].trim();
            rest.parse::<i64>().ok().map(crate::media::Command::SeekTo)
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

/// Interpola la posición remota para la barra de progreso del controlador.
///
/// Solo avanza cuando el estado es `Playing`; en pausa o detenido se congela.
/// Cada estado nuevo reemplaza el ancla y la tasa, así que seek, cambio de
/// pista y cambio de velocidad se resincronizan de inmediato.
struct RemotePosition {
    position_ticks: i64,
    playback_rate: f64,
    state: PlaybackState,
    anchor_at: Instant,
}

impl RemotePosition {
    fn new(position_ticks: i64, playback_rate: f64, state: PlaybackState, now: Instant) -> Self {
        Self {
            position_ticks,
            playback_rate,
            state,
            anchor_at: now,
        }
    }

    fn update(
        &mut self,
        position_ticks: i64,
        playback_rate: f64,
        state: PlaybackState,
        now: Instant,
    ) {
        self.position_ticks = position_ticks;
        self.playback_rate = playback_rate;
        self.state = state;
        self.anchor_at = now;
    }

    /// Posición interpolada en `now`, en ticks.
    fn position(&self, now: Instant) -> i64 {
        let anchor = self.position_ticks.max(0);
        if self.state == PlaybackState::Playing {
            let elapsed = now.saturating_duration_since(self.anchor_at);
            let advanced =
                (elapsed.as_secs_f64() * self.playback_rate * 10_000_000.0).round() as i64;
            anchor.saturating_add(advanced)
        } else {
            anchor
        }
    }
}

/// Estado que el controlado publica por el canal de datos.
#[derive(Clone, Debug, PartialEq)]
struct RemoteState {
    has_session: bool,
    playback: PlaybackState,
    position_ticks: i64,
    duration_ticks: i64,
    playback_rate: f64,
    title: String,
    artist: String,
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
    Command(Command),
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
        let (toggle, seek) = media_controls_gated(net::Role::Controlado, true, true);
        assert_eq!((toggle, seek), (false, false));
    }

    #[test]
    fn controlador_enables_media_controls_when_session() {
        let (toggle, seek) = media_controls_gated(net::Role::Controlador, true, true);
        assert_eq!((toggle, seek), (true, true));
    }

    #[test]
    fn controlador_disables_media_controls_when_no_session() {
        let (toggle, seek) = media_controls_gated(net::Role::Controlador, false, false);
        assert_eq!((toggle, seek), (false, false));
    }

    #[test]
    fn parse_media_command_play() {
        let cmd = parse_media_command("play");
        assert_eq!(cmd, Some(crate::media::Command::Play));
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
        assert_eq!(cmd, Some(crate::media::Command::Pause));
    }

    #[test]
    fn parse_media_command_stop() {
        let cmd = parse_media_command("stop");
        assert_eq!(cmd, Some(crate::media::Command::Stop));
    }

    #[test]
    fn parse_media_command_seek_to() {
        let cmd = parse_media_command("seek_to 125000");
        assert_eq!(cmd, Some(crate::media::Command::SeekTo(125000)));
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
    fn remote_position_advances_with_the_playback_rate() {
        let start = Instant::now();
        let position = RemotePosition::new(0, 1.5, PlaybackState::Playing, start);

        let advanced = position.position(start + Duration::from_secs(2));
        assert_eq!(advanced, 3 * 10_000_000);
    }

    #[test]
    fn remote_position_does_not_advance_when_not_playing() {
        let start = Instant::now();
        let position = RemotePosition::new(5_000_000, 1.0, PlaybackState::Paused, start);

        assert_eq!(position.position(start + Duration::from_secs(2)), 5_000_000);
    }

    #[test]
    fn a_new_remote_state_reanchors_the_position() {
        let start = Instant::now();
        let mut position = RemotePosition::new(0, 1.0, PlaybackState::Playing, start);
        position.update(
            30_000_000,
            1.0,
            PlaybackState::Playing,
            start + Duration::from_millis(100),
        );

        // Justo después del reanclaje, la posición es el valor anclado.
        assert_eq!(
            position.position(start + Duration::from_millis(100)),
            30_000_000
        );
        // Y sigue interpolando desde ese ancla con la tasa nueva.
        assert_eq!(
            position.position(start + Duration::from_millis(200)),
            31_000_000
        );
    }

    #[test]
    fn a_rate_change_counts_as_a_discrete_change() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();
        let base = Snapshot {
            playback_rate: 1.0,
            ..playing(0)
        };
        assert!(publisher.should_publish(&base, start));

        let changed = Snapshot {
            playback_rate: 2.0,
            ..playing(0)
        };
        assert!(publisher.should_publish(&changed, start + Duration::from_millis(100)));
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
        assert!(!publisher.should_publish(&playing(1_000_000), start + Duration::from_millis(100)));
        assert!(!publisher.should_publish(&playing(9_000_000), start + Duration::from_millis(900)));
        assert!(publisher.should_publish(&playing(10_000_000), start + Duration::from_secs(1)));
    }

    #[test]
    fn discrete_changes_are_published_immediately() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();
        let base = playing(0);
        assert!(publisher.should_publish(&base, start));

        let mut changed = base.clone();
        changed.title = "Otra canción".to_string();
        assert!(publisher.should_publish(&changed, start + Duration::from_millis(100)));

        let mut changed = changed.clone();
        changed.artist = "Otro artista".to_string();
        assert!(publisher.should_publish(&changed, start + Duration::from_millis(200)));

        let mut changed = changed.clone();
        changed.state = PlaybackState::Paused;
        assert!(publisher.should_publish(&changed, start + Duration::from_millis(300)));

        let mut changed = changed.clone();
        changed.duration_ticks += 10_000_000;
        assert!(publisher.should_publish(&changed, start + Duration::from_millis(400)));

        let mut changed = changed.clone();
        changed.has_session = false;
        assert!(publisher.should_publish(&changed, start + Duration::from_millis(500)));
    }

    #[test]
    fn seeks_are_published_immediately() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();
        assert!(publisher.should_publish(&playing(0), start));

        assert!(publisher.should_publish(
            &playing(30 * 10_000_000),
            start + Duration::from_millis(100)
        ));
        assert!(
            publisher.should_publish(&playing(5 * 10_000_000), start + Duration::from_millis(200))
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
        assert!(!publisher.should_publish(&paused, start + Duration::from_millis(100)));
        assert!(!publisher.should_publish(&paused, start + Duration::from_secs(1)));
        assert!(!publisher.should_publish(&paused, start + Duration::from_secs(2)));
    }

    #[test]
    fn cached_snapshot_on_connect_starts_the_window() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();
        let cached = playing(0);

        publisher.reset(&cached, start);

        assert!(!publisher.should_publish(&playing(1_000_000), start + Duration::from_millis(100)));
        assert!(publisher.should_publish(&playing(10_000_000), start + Duration::from_secs(1)));
    }

    #[test]
    fn seek_to_echo_is_suppressed() {
        let start = Instant::now();
        let pending = PendingCommand {
            action: "seek_to".to_string(),
            target_ticks: Some(125_000_000),
            received_at: start,
        };
        let snapshot = Snapshot {
            has_session: true,
            position_ticks: 125_000_000,
            state: PlaybackState::Playing,
            ..Snapshot::no_session()
        };

        assert!(is_echo(&pending, &snapshot));
    }

    #[test]
    fn non_matching_local_event_is_published() {
        let start = Instant::now();
        let pending = PendingCommand {
            action: "play".to_string(),
            target_ticks: None,
            received_at: start,
        };
        let snapshot = Snapshot {
            has_session: true,
            position_ticks: 5_000_000,
            state: PlaybackState::Paused,
            ..Snapshot::no_session()
        };

        assert!(!is_echo(&pending, &snapshot));
    }

    #[test]
    fn pending_command_expires_after_grace_period() {
        let start = Instant::now();
        let pending = PendingCommand {
            action: "play".to_string(),
            target_ticks: None,
            received_at: start,
        };

        assert!(!is_expired(&pending, start + Duration::from_millis(100)));
        assert!(is_expired(
            &pending,
            start + ECHO_GRACE_PERIOD + Duration::from_millis(1)
        ));
    }
}
