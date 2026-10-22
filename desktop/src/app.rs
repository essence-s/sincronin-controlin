//! Cableado entre la UI de Slint y los backends.
//!
//! La UI nunca toca `WinRT`: envía [`media::Command`] por un canal y recibe
//! [`media::Snapshot`] a través de un [`media::Sink`]. Lo mismo con la red:
//! [`net::Command`] y [`net::Event`].
//!
//! Es la única capa que conoce las dos caras a la vez. La lógica de medios
//! vive en [`media`] y la integración con el sistema en [`platform`]; la
//! lógica de la red, en [`net`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::unbounded;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::media::{Command, PlaybackState, Sink, Snapshot};
use crate::net;
use crate::platform;
use crate::{AppWindow, MediaSession};

/// Cuántas líneas guarda el registro de la ventana de red.
const MAX_LOG_LINES: usize = 40;

/// El mensaje de prueba que manda el botón.
const TEST_MESSAGE: &str = "hola";

pub fn run() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;

    // Canal UI -> backend (comandos).
    let (cmd_tx, cmd_rx) = unbounded::<Command>();

    // Backend -> UI: cada evento del sistema publica una fotografía nueva.
    let sink: Sink = {
        let weak = ui.as_weak();
        std::sync::Arc::new(move |snapshot: Snapshot| {
            let _ = weak.upgrade_in_event_loop(move |ui| apply(&ui, &snapshot));
        })
    };

    platform::spawn(cmd_rx, sink);

    // La red tiene su propio canal y su propio sink: no se mezcla con
    // los de medios.
    let (net_cmd_tx, net_cmd_rx) = unbounded::<net::Command>();

    // Si el cliente ya está en una sala. Sirve para no salir de ella al
    // abrir el panel.
    let in_room = Arc::new(AtomicBool::new(false));

    let net_sink: net::Sink = {
        let weak = ui.as_weak();
        let in_room = Arc::clone(&in_room);
        Arc::new(move |event: net::Event| {
            let in_room = Arc::clone(&in_room);
            let _ = weak.upgrade_in_event_loop(move |ui| apply_net(&ui, &event, &in_room));
        })
    };

    net::spawn(net_cmd_rx, net_sink);

    {
        let tx = cmd_tx.clone();
        ui.on_toggle_play_pause(move || {
            let _ = tx.send(Command::TogglePlayPause);
        });
    }
    {
        let tx = cmd_tx.clone();
        ui.on_seek_backward(move || {
            let _ = tx.send(Command::SeekBackward);
        });
    }
    {
        let tx = cmd_tx.clone();
        ui.on_seek_forward(move || {
            let _ = tx.send(Command::SeekForward);
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
fn apply(ui: &AppWindow, snapshot: &Snapshot) {
    ui.set_has_session(snapshot.has_session);
    ui.set_playing(snapshot.state == PlaybackState::Playing);
    ui.set_can_toggle(snapshot.can_toggle);
    ui.set_can_seek(snapshot.can_seek);

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

/// Vuelca un evento de la red en el panel de emparejamiento.
fn apply_net(ui: &AppWindow, event: &net::Event, in_room: &AtomicBool) {
    match event {
        net::Event::State(state) => {
            in_room.store(*state != net::NetState::Desconectado, Ordering::Relaxed);
            ui.set_state_text(state.label().into());
            ui.set_can_send(*state == net::NetState::Conectado);
        }
        net::Event::Message(text) => {
            append_log(ui, &format!("← {text}"));
        }
        net::Event::Notice(text) => {
            append_log(ui, &format!("· {text}"));
        }
    }
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
