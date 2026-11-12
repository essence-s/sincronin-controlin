//! Único punto de despacho por plataforma del proyecto.
//!
//! Regla: **`#[cfg(...)]` solo aparece dentro de este módulo y sus hijos.**
//! El resto de `src/` es agnóstico de plataforma. Así, compilar en Linux
//! elimina de raíz el código de Windows y viceversa: no hay un
//! `if platform ==` en runtime que se pueda invertir por error.
//!
//! - **Windows** → backend real sobre GSMTC (`Windows.Media.Control`) más
//!   los estilos de ventana de `windows::window` y la vigilancia de red de
//!   `windows::network`.
//! - **Linux** → backend real sobre MPRIS (D-Bus) más `linux::window` y la
//!   vigilancia de red de `linux::network`.
//! - **Otros** → `stub`, un shim para iterar la UI sin backend real. No es
//!   una implementación de macOS: solo permite compilar y ver la interfaz.

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::unbounded;

use crate::domain::media::Snapshot;

/// Callback con el que un backend de plataforma publica cada cambio.
///
/// Los backends viven detrás de `#[cfg]`; el tipo queda acá para que el
/// adaptador de medios no dependa de una plataforma concreta.
pub type Sink = Arc<dyn Fn(Snapshot) + Send + Sync + 'static>;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::gsmtc::spawn;
#[cfg(windows)]
pub use windows::window::{animates_resize, configure, reposition};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::mpris::spawn;
#[cfg(target_os = "linux")]
use linux::network::spawn_network_source;
#[cfg(target_os = "linux")]
pub use linux::window::{animates_resize, configure, reposition};

#[cfg(windows)]
use windows::network::spawn_network_source;

#[cfg(not(any(windows, target_os = "linux")))]
mod stub;
#[cfg(not(any(windows, target_os = "linux")))]
use stub::spawn_network_source;
#[cfg(not(any(windows, target_os = "linux")))]
pub use stub::{animates_resize, configure, reposition, spawn};

/// Cuánto se espera para juntar los avisos que llegan en ráfaga.
///
/// Una sola interfaz puede generar varios avisos seguidos (baja, cambio de
/// dirección, alta): se reenumera una vez, no una por aviso.
const NETWORK_COALESCE: Duration = Duration::from_millis(200);

/// Observa los cambios de red del sistema y avisa una vez por ráfaga.
///
/// El mecanismo es de cada plataforma (netlink en Linux,
/// `NotifyIpInterfaceChange` en Windows); el hilo de acá solo junta los
/// avisos, para que el adaptador de red no abra enlaces de una red a medio
/// cambiar. No hay polling: el hilo duerme hasta que el sistema avisa.
pub fn spawn_network_watch(notify: impl Fn() + Send + 'static) {
    let (tx, rx) = unbounded::<()>();

    std::thread::Builder::new()
        .name("net-watch".to_string())
        .spawn(move || {
            while rx.recv().is_ok() {
                std::thread::sleep(NETWORK_COALESCE);
                while rx.try_recv().is_ok() {}
                notify();
            }
        })
        .expect("no se pudo crear el hilo de vigilancia de red");

    spawn_network_source(tx);
}
