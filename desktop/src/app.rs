//! Composición de la aplicación: conecta adaptadores, servicios y UI.
//!
//! Es la única capa que conoce las dos caras a la vez: la UI de Slint y los
//! adaptadores de infraestructura. La lógica de negocio vive en los
//! servicios de [`crate::services`]; la integración con el sistema, en
//! [`crate::platform`]; la lógica de la red, en [`crate::net`] y
//! [`crate::adapters::net`].

use std::sync::{Arc, Mutex};
use std::time::Instant;

use slint::{ComponentHandle, SharedString};

use crate::AppWindow;
use crate::adapters::media::MediaAdapter;
use crate::adapters::net::NetAdapter;
use crate::adapters::ui::slint::SlintAdapter;
use crate::domain::media::Command;
use crate::platform;
use crate::ports::{MediaPort, NetPort, UiPort};
use crate::services::pairing::PairingService;
use crate::services::state::StateService;

/// El mensaje de prueba que manda el botón.
const TEST_MESSAGE: &str = "hola";

pub fn run() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;

    // ── Adaptadores de infraestructura ────────────────────────────────
    let net = NetAdapter::spawn();
    let media = MediaAdapter::spawn();
    let ui_port: Arc<dyn UiPort> = Arc::new(SlintAdapter::new(&ui));

    // ── Servicios de la capa de aplicación ────────────────────────────
    let state = Arc::new(Mutex::new(StateService::new(
        media.clone(),
        net.clone(),
        ui_port,
    )));
    let pairing = PairingService::new(net.clone());

    // ── Backend de medios → servicio de estado ────────────────────────
    {
        let state = Arc::clone(&state);
        media.on_snapshot(Box::new(move |snapshot| {
            state
                .lock()
                .expect("el servicio de estado no está poisoned")
                .on_local_snapshot(snapshot, Instant::now());
        }));
    }

    // ── Red → servicio de estado ──────────────────────────────────────
    {
        let state = Arc::clone(&state);
        net.on_event(Box::new(move |event| {
            state
                .lock()
                .expect("el servicio de estado no está poisoned")
                .on_net_event(event, Instant::now());
        }));
    }

    // ── Callbacks de la UI ────────────────────────────────────────────
    {
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        ui.on_toggle_play_pause(move || {
            let playing = weak.upgrade().map(|ui| ui.get_playing()).unwrap_or(false);
            let command = if playing {
                Command::Pause
            } else {
                Command::Play
            };
            state
                .lock()
                .expect("el servicio de estado no está poisoned")
                .on_user_command(command);
        });
    }

    {
        let state = Arc::clone(&state);
        ui.on_seek_backward(move || {
            state
                .lock()
                .expect("el servicio de estado no está poisoned")
                .on_user_command(Command::SeekBackward);
        });
    }

    {
        let state = Arc::clone(&state);
        ui.on_seek_forward(move || {
            state
                .lock()
                .expect("el servicio de estado no está poisoned")
                .on_user_command(Command::SeekForward);
        });
    }

    {
        let state = Arc::clone(&state);
        ui.on_seek_to(move |secs: f32| {
            let ticks = (secs as f64 * 10_000_000.0) as i64;
            state
                .lock()
                .expect("el servicio de estado no está poisoned")
                .on_user_command(Command::SeekTo(ticks));
        });
    }

    {
        let state = Arc::clone(&state);
        ui.on_select_session(move |id: SharedString| {
            state
                .lock()
                .expect("el servicio de estado no está poisoned")
                .on_user_command(Command::SelectSession(id.to_string()));
        });
    }

    // El panel de emparejamiento vive en la misma ventana que la barra.
    {
        let pairing = pairing.clone();
        let weak = ui.as_weak();
        ui.on_connect(move |room: SharedString| {
            let room = room.trim();

            if room.is_empty() {
                // Si no escribió nada, se entra a la sala propia.
                let code = pairing.local_room_code();
                if let Some(ui) = weak.upgrade() {
                    ui.set_room_code(code.as_str().into());
                }
                pairing.connect(None);
            } else {
                let code = room.to_ascii_uppercase();
                if let Some(ui) = weak.upgrade() {
                    ui.set_room_code(code.as_str().into());
                }
                pairing.connect(Some(code));
            }
        });
    }

    {
        let pairing = pairing.clone();
        ui.on_disconnect(move || {
            pairing.disconnect();
        });
    }

    {
        let net = net.clone();
        ui.on_send_message(move || {
            net.send_data(TEST_MESSAGE.to_string());
        });
    }

    {
        // Al abrir el panel, el cliente entra solo en su propia sala: así el
        // código que muestra está vivo y el otro equipo se puede conectar
        // sin pasos de por medio. Si ya estaba en una sala, no se mueve de
        // ella.
        let state = Arc::clone(&state);
        let net = net.clone();
        let weak = ui.as_weak();
        ui.on_net_toggled(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };

            platform::configure(&ui);

            if ui.get_net_open() {
                let in_room = state
                    .lock()
                    .expect("el servicio de estado no está poisoned")
                    .is_in_room();

                if !in_room {
                    ui.set_room_code(net.local_room_code().into());
                    net.connect(None);
                }
            }
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
