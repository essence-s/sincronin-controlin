//! Adaptador de UI sobre Slint.
//!
//! Implementa [`crate::ports::UiPort`]. Todas las operaciones se encolan en
//! el bucle de eventos de Slint: los servicios las llaman desde el hilo de
//! medios o el de red, y Slint no es thread-safe.
//!
//! También es el adaptador de entrada: traduce los callbacks de Slint a
//! [`AppPort`], para que `app` solo componga las piezas.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::domain::media::{PlaybackState, Snapshot};
use crate::domain::messages::RemoteState;
use crate::domain::room::Role;
use crate::platform;
use crate::ports::{AppPort, UiPort};
use crate::{AppWindow, MediaSession};

/// Cuántas líneas guarda el registro de la ventana de red.
const MAX_LOG_LINES: usize = 40;

/// El mensaje que manda el botón de prueba del panel.
const TEST_MESSAGE: &str = "hola";

/// Adaptador de UI.
pub struct SlintAdapter {
    ui: slint::Weak<AppWindow>,
}

impl SlintAdapter {
    /// Crea el adaptador sobre la ventana ya construida.
    pub fn new(ui: &AppWindow) -> Self {
        Self { ui: ui.as_weak() }
    }

    /// Encola una operación en el hilo de la UI.
    fn run<F>(&self, operation: F)
    where
        F: FnOnce(&AppWindow) + Send + 'static,
    {
        let _ = self.ui.upgrade_in_event_loop(move |ui| operation(&ui));
    }

    /// Cablea los callbacks de entrada de la ventana contra [`AppPort`].
    ///
    /// Se llama una sola vez, cuando la ventana ya existe y el controlador
    /// está construido. La UI no conoce los servicios: solo este adaptador.
    pub fn wire(ui: &AppWindow, app: Arc<dyn AppPort>) {
        {
            let app = Arc::clone(&app);
            let weak = ui.as_weak();
            ui.on_toggle_play_pause(move || {
                let playing = weak.upgrade().map(|ui| ui.get_playing()).unwrap_or(false);
                app.toggle_play_pause(playing);
            });
        }

        {
            let app = Arc::clone(&app);
            ui.on_seek_backward(move || app.seek_backward());
        }

        {
            let app = Arc::clone(&app);
            ui.on_seek_forward(move || app.seek_forward());
        }

        {
            let app = Arc::clone(&app);
            ui.on_seek_to(move |seconds: f32| app.seek_to(seconds));
        }

        {
            let app = Arc::clone(&app);
            ui.on_select_session(move |id: SharedString| app.select_session(id.to_string()));
        }

        {
            let app = Arc::clone(&app);
            ui.on_connect(move |room: SharedString| app.connect(room.to_string()));
        }

        {
            let app = Arc::clone(&app);
            ui.on_disconnect(move || app.disconnect());
        }

        {
            let app = Arc::clone(&app);
            ui.on_send_message(move || app.send_message(TEST_MESSAGE.to_string()));
        }

        {
            let app = Arc::clone(&app);
            let weak = ui.as_weak();
            ui.on_net_toggled(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };

                platform::configure(&ui);
                app.panel_toggled(ui.get_net_open());
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
    }
}

impl UiPort for SlintAdapter {
    fn show_local_snapshot(&self, snapshot: &Snapshot, role: Option<Role>) {
        let snapshot = snapshot.clone();
        self.run(move |ui| apply_local(ui, &snapshot, role));
    }

    fn show_remote_state(&self, state: &RemoteState, position_ticks: i64) {
        let state = state.clone();
        self.run(move |ui| apply_remote(ui, &state, position_ticks));
    }

    fn set_state_text(&self, text: String) {
        self.run(move |ui| ui.set_state_text(text.into()));
    }

    fn set_role_text(&self, text: String) {
        self.run(move |ui| ui.set_role_text(text.into()));
    }

    fn set_room_code(&self, code: String) {
        self.run(move |ui| ui.set_room_code(code.into()));
    }

    fn set_peer_room(&self, room: String) {
        self.run(move |ui| ui.set_peer_room(room.into()));
    }

    fn set_can_send(&self, can: bool) {
        self.run(move |ui| ui.set_can_send(can));
    }

    fn append_log(&self, line: String) {
        self.run(move |ui| {
            let mut lines: Vec<String> = ui.get_log_text().lines().map(str::to_string).collect();
            lines.push(line);

            if lines.len() > MAX_LOG_LINES {
                lines.drain(..lines.len() - MAX_LOG_LINES);
            }

            ui.set_log_text(lines.join("\n").into());
        });
    }

    fn open_panel(&self) {
        self.run(|ui| {
            if ui.get_net_open() {
                return;
            }

            ui.set_net_open(true);
            platform::configure(ui);
        });
    }
}

/// Vuelca una fotografía del backend en las propiedades de la UI.
///
/// Se llama en el hilo de la UI, así que puede tocar el modelo de sesiones:
/// los modelos de Slint usan `Rc` y no se pueden mandar a otro hilo.
fn apply_local(ui: &AppWindow, snapshot: &Snapshot, role: Option<Role>) {
    ui.set_has_session(snapshot.has_session);
    ui.set_playing(snapshot.state == PlaybackState::Playing);

    // El controlado nunca controla desde su propia barra. Sin rol asignado
    // se asume controlado, como antes de entrar a una sala.
    let (can_toggle, can_seek) = if role == Some(Role::Controlador) {
        (snapshot.can_toggle, snapshot.can_seek)
    } else {
        (false, false)
    };
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

/// Vuelca el estado que publica el controlado en la UI del controlador.
///
/// No toca las sesiones: el selector sigue siendo el del reproductor local.
/// Los controles sí reflejan las capacidades del reproductor del controlado,
/// que es el que va a ejecutar los comandos.
fn apply_remote(ui: &AppWindow, state: &RemoteState, position_ticks: i64) {
    ui.set_has_session(state.has_session);
    ui.set_playing(state.playback == PlaybackState::Playing);
    ui.set_can_toggle(state.can_toggle);
    ui.set_can_seek(state.can_seek);

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
