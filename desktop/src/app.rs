//! Composición de la aplicación: conecta adaptadores, servicios y UI.
//!
//! Es la única capa que conoce las dos caras a la vez: la UI de Slint y los
//! adaptadores de infraestructura. La lógica de negocio vive en los
//! servicios de [`crate::services`]; la integración con el sistema, en
//! [`crate::platform`]; la red, en [`crate::adapters::net`].
//!
//! El adaptador de Slint cablea los callbacks contra [`crate::ports::AppPort`]
//! y este archivo solo construye las piezas.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use slint::ComponentHandle;

use crate::AppWindow;
use crate::adapters::media::MediaAdapter;
use crate::adapters::net::NetAdapter;
use crate::adapters::ui::slint::SlintAdapter;
use crate::platform;
use crate::ports::{MediaPort, NetPort, UiPort};
use crate::services::controller::AppController;
use crate::services::pairing::PairingService;
use crate::services::state::StateService;

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
        Arc::clone(&ui_port),
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

    // ── Entrada de la UI → servicios ──────────────────────────────────
    let app = Arc::new(AppController::new(
        Arc::clone(&state),
        pairing,
        net,
        ui_port,
    ));
    SlintAdapter::wire(&ui, app);

    platform::configure(&ui);

    ui.run()
}
