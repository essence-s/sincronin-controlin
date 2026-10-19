//! Cableado entre la UI de Slint y el backend de medios.
//!
//! La UI nunca toca `WinRT`: envía [`media::Command`] por un canal y recibe
//! [`media::Snapshot`] a través de un [`media::Sink`]. Acá está todo el
//! armado — canal, callbacks y traducción de `Snapshot` a propiedades de Slint.
//!
//! Es la única capa que conoce las dos caras a la vez. La lógica de medios
//! vive en [`media`] y la integración con el sistema en [`platform`].

use crossbeam_channel::unbounded;
use slint::ComponentHandle;

use crate::AppWindow;
use crate::media::{Command, PlaybackState, Sink, Snapshot};
use crate::platform;

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
}

/// Convierte ticks de 100 ns (unidad de `WinRT`) a segundos.
///
/// Los ticks son `i64` pero las propiedades de Slint son `i32`. El `try_from`
/// evita una truncación silenciosa si algún día un valor fuera absurdo.
fn segundos(ticks: i64) -> i32 {
    let s = ticks / 10_000_000;
    i32::try_from(s).unwrap_or(if s > 0 { i32::MAX } else { i32::MIN })
}
