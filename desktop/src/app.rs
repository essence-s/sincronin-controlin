//! Composición de la aplicación: conecta adaptadores, servicios y UI.
//!
//! Es la única capa que conoce las dos caras a la vez: la UI de Slint y los
//! adaptadores de infraestructura. La lógica de negocio vive en los
//! servicios de [`crate::services`]; la integración con el sistema, en
//! [`crate::platform`]; la red, en [`crate::adapters::net`].
//!
//! El adaptador de Slint cablea los callbacks contra [`crate::ports::AppPort`]
//! y este archivo solo construye las piezas.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use slint::ComponentHandle;

use crate::AppWindow;
use crate::adapters::media::MediaAdapter;
use crate::adapters::net::NetAdapter;
use crate::adapters::ui::slint::SlintAdapter;
use crate::domain::window::{COLLAPSE_SLACK, IDLE_COLLAPSE, WindowIdle, WindowMode};
use crate::platform;
use crate::ports::{AppPort, MediaPort, NetPort, UiPort};
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

    // ── Cambios de red → adaptador de red ─────────────────────────────
    // El hilo de vigilancia vive lo que vive la app: cuando cambia una
    // interfaz, cada par reenumera y anuncia los enlaces nuevos.
    {
        let net = net.clone();
        platform::spawn_network_watch(move || net.network_changed());
    }

    // ── Entrada de la UI → servicios ──────────────────────────────────
    let app: Arc<dyn AppPort> = Arc::new(AppController::new(Arc::clone(&state), pairing, ui_port));
    SlintAdapter::wire(&ui, Arc::clone(&app));

    // La app arranca en Conexión: se entra a la sala propia sin esperar a
    // que el usuario cambie de panel. Es el mismo camino que dispara abrir
    // Conexión desde el menú.
    app.panel_toggled(true);

    // Al cambiar de panel cambia el ancho de la ventana. En Windows hay que
    // volver a anclarla abajo al centro; en Linux el compositor manda.
    {
        let weak = ui.as_weak();
        ui.on_size_changed(move || {
            if let Some(ui) = weak.upgrade() {
                platform::reposition(&ui);
            }
        });
    }

    // ── Colapso por inactividad ───────────────────────────────────────
    // La ventana avisa cuándo el puntero entra y sale; la regla de los 2 s
    // vive en el dominio. Cada aviso de salida arma un temporizador de un
    // solo tiro: no hay polling, y si el mouse volvió a entrar la regla dice
    // que todavía no toca achicarse.
    let idle = Rc::new(RefCell::new(WindowIdle::new(Instant::now())));
    {
        let idle = Rc::clone(&idle);
        let weak = ui.as_weak();
        ui.on_pointer_entered(move || {
            idle.borrow_mut().pointer_entered(Instant::now());
            // Con el puntero encima la regla ya dice "normal": volver al
            // tamaño default es aplicarla, no un caso aparte.
            if let Some(ui) = weak.upgrade() {
                ui.set_collapsed(false);
                // El aviso del tamaño intermedio se descarta al abrir la
                // interfaz: ya se ve el estado.
                ui.set_notice_text("".into());
            }
        });
    }
    {
        let idle = Rc::clone(&idle);
        let weak = ui.as_weak();
        ui.on_pointer_left(move || {
            idle.borrow_mut().pointer_left(Instant::now());
            arm_collapse(weak.clone(), Rc::clone(&idle));
        });
    }
    // Al arrancar el mouse puede estar en otra parte: la cuenta corre desde
    // ya, sin esperar a que entre y salga.
    arm_collapse(ui.as_weak(), Rc::clone(&idle));

    // Animar el cambio de tamaño depende de quién sea dueño de la posición:
    // donde el compositor recoloca la ventana (Wayland), animar acumula un
    // corrimiento de unos píxeles por ciclo y conviene saltar de golpe.
    ui.set_animate_resize(platform::animates_resize());

    platform::configure(&ui);

    ui.run()
}

/// Arma la cuenta de inactividad. Cuando vence, la regla de dominio decide si
/// la ventana ya puede achicarse; si el mouse volvió a entrar, no hace nada.
fn arm_collapse(weak: slint::Weak<AppWindow>, idle: Rc<RefCell<WindowIdle>>) {
    // El margen evita la carrera del límite: un temporizador que vence justo
    // en los 2 s puede adelantarse unas décimas y la cuenta todavía no llega.
    slint::Timer::single_shot(IDLE_COLLAPSE + COLLAPSE_SLACK, move || {
        if idle.borrow().mode(Instant::now()) != WindowMode::Circle {
            return;
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_collapsed(true);
        }
    });
}
