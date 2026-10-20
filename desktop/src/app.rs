//! Cableado entre la UI de Slint y el backend de medios.
//!
//! La UI nunca toca `WinRT`: envía [`media::Command`] por un canal y recibe
//! [`media::Snapshot`] a través de un [`media::Sink`]. Acá está todo el
//! armado — canal, callbacks y traducción de `Snapshot` a propiedades de Slint.
//!
//! Es la única capa que conoce las dos caras a la vez. La lógica de medios
//! vive en [`media`] y la integración con el sistema en [`platform`].

use crossbeam_channel::unbounded;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::media::{Command, PlaybackState, Sink, Snapshot};
use crate::platform;
use crate::{AppWindow, MediaSession};

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
    let nueva: Vec<MediaSession> = snapshot
        .sessions
        .iter()
        .map(|s| MediaSession {
            id: SharedString::from(s.id.as_str()),
            label: SharedString::from(s.label.as_str()),
        })
        .collect();
    if cambio(&ui.get_sessions(), &nueva) {
        ui.set_sessions(ModelRc::new(VecModel::from(nueva)));
    }
}

/// Si la lista de sesiones de la UI es distinta de la que llega del backend.
fn cambio(actual: &ModelRc<MediaSession>, nueva: &[MediaSession]) -> bool {
    if actual.row_count() != nueva.len() {
        return true;
    }
    (0..nueva.len()).any(|i| match actual.row_data(i) {
        Some(fila) => fila.id != nueva[i].id || fila.label != nueva[i].label,
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
