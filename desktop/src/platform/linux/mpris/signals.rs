//! Escucha de las señales D-Bus que el crate `mpris` no expone.
//!
//! Son dos:
//!
//! - `Seeked`: trae la posición exacta de un seek. Hace falta escucharla acá
//!   porque el `ProgressTracker` del crate la consume pero después relee
//!   `Position`, que en Firefox devuelve `0` tras un seek mientras reproduce.
//! - `NameOwnerChanged`: avisa cuándo entra o sale un reproductor MPRIS. Así
//!   no hay que hacer `find_all()` en cada tick.
//!
//! Cada conexión de D-Bus tiene un solo lector. El hilo del backend usa la
//! conexión del crate `mpris`; este módulo abre la suya y se queda escuchando
//! señales. No consulta propiedades: solo recibe avisos.
//!
//! **Ojo con el remitente:** las señales llegan con el nombre único del bus
//! (`:1.42`), no con el nombre bien conocido (`org.mpris.MediaPlayer2...`).
//! Por eso `Signal::Seeked` lleva el nombre único y el bucle lo compara con el
//! reproductor actual.

use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use dbus::blocking::Connection;
use dbus::message::MatchRule;

/// Lo que el hilo de señales le avisa al bucle del backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// Cambió el conjunto de reproductores del bus.
    BusChanged,
    /// Un reproductor hizo un seek; nueva posición en microsegundos.
    ///
    /// Lleva el nombre único del emisor: el bucle descarta los seeks de otros
    /// reproductores, incluidos los que quedaron en cola al cambiar de sesión.
    Seeked { player: String, position_us: i64 },
}

/// Arranca el hilo de señales y devuelve el canal por el que avisa.
///
/// Espera a que las reglas de D-Bus estén registradas antes de devolver: así
/// no se pierde un `Seeked` que ocurra justo al arrancar.
pub fn spawn() -> Receiver<Signal> {
    let (tx, rx) = unbounded();
    let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);

    let spawned = std::thread::Builder::new()
        .name("mpris-signals".to_string())
        .spawn(move || listen(tx, ready_tx));

    match spawned {
        Ok(_) => {
            let _ = ready_rx.recv_timeout(Duration::from_secs(1));
        }
        Err(err) => eprintln!("[media] no se pudo crear el hilo de señales: {err}"),
    }

    rx
}

/// Cuerpo del hilo de señales. Si algo falla, se corta la escucha y el bucle
/// del backend queda con los reintentos de descubrimiento.
fn listen(tx: Sender<Signal>, ready: Sender<()>) {
    let conn = match Connection::new_session() {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("[media] no se pudo crear la conexión de señales: {err}");
            let _ = ready.try_send(());
            return;
        }
    };

    // Entra o sale un reproductor del bus.
    let bus_tx = tx.clone();
    let rule = MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged");
    if let Err(err) =
        conn.add_match::<(String, String, String), _>(rule, move |signal, _conn, _msg| {
            let (name, old_owner, new_owner) = signal;
            // El bus de sesión cambia por un montón de cosas más. Solo
            // interesan los reproductores MPRIS.
            if name.starts_with("org.mpris.MediaPlayer2") {
                eprintln!("[media] cambió el reproductor {name} ({old_owner} → {new_owner})");
                let _ = bus_tx.send(Signal::BusChanged);
            }
            true
        })
    {
        eprintln!("[media] no se pudo escuchar NameOwnerChanged: {err}");
        let _ = ready.try_send(());
        return;
    }

    // Seek de cualquier reproductor. El bucle se queda solo con los del
    // reproductor actual.
    let seeked_tx = tx.clone();
    let rule = MatchRule::new_signal("org.mpris.MediaPlayer2.Player", "Seeked");
    if let Err(err) = conn.add_match::<(i64,), _>(rule, move |signal, _conn, msg| {
        let (position_us,) = signal;
        let player = msg
            .sender()
            .map(|name| name.to_string())
            .unwrap_or_default();
        let _ = seeked_tx.send(Signal::Seeked {
            player,
            position_us,
        });
        true
    }) {
        eprintln!("[media] no se pudo escuchar Seeked: {err}");
        let _ = ready.try_send(());
        return;
    }

    // Las dos reglas ya están registradas: el backend puede empezar.
    let _ = ready.try_send(());

    // Atiende el bus para siempre: las señales llegan por acá.
    loop {
        if let Err(err) = conn.process(Duration::from_millis(500)) {
            eprintln!("[media] se cortó la escucha de señales: {err}");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use dbus::arg::ReadAll;

    #[test]
    fn the_signal_position_is_signed() {
        // El tipo MPRIS de `Seeked` es `x` (i64). Este test documenta el
        // contrato: si alguien lo cambia a u64, la señal deja de decodificar.
        let message = dbus::Message::new_signal(
            "/org/mpris/MediaPlayer2",
            "org.mpris.MediaPlayer2.Player",
            "Seeked",
        )
        .unwrap()
        .append1(42_i64);

        let mut iter = message.iter_init();
        let signal: (i64,) = ReadAll::read(&mut iter).expect("el argumento es i64");
        assert_eq!(signal.0, 42);
    }
}
