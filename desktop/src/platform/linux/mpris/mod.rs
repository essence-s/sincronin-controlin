//! Backend de medios para Linux sobre MPRIS (D-Bus).
//!
//! MPRIS es el estándar de Linux para controlar reproductores: Spotify, mpv,
//! VLC, RhythmBox, Firefox... todos exponen `org.mpris.MediaPlayer2.Player`.
//!
//! # Posición
//!
//! `Position` no emite `PropertiesChanged`: la única señal de un salto es
//! `Seeked`, y su payload es la posición exacta. Eso es lo que usa este
//! backend, igual que Windows usa `LastUpdatedTime` de `GSMTC`:
//!
//! 1. Se guarda un ancla (posición + instante + tasa).
//! 2. Entre eventos la posición se interpola en local.
//! 3. Un `Seeked` reemplaza el ancla con su payload, **sin** releer
//!    `Position`.
//! 4. Las lecturas de `Position` que llegan por otros eventos solo reanclan
//!    si el salto es plausible. Así no se rompe con el `0` transitorio que
//!    devuelve Firefox después de un seek.
//!
//! Todo esto vive en [`timeline`], que no toca D-Bus y se puede probar sin
//! bus.
//!
//! # Hilos
//!
//! - **mpris** (este módulo): es el dueño del `Player` del crate `mpris` y el
//!   único que ejecuta comandos o lee propiedades.
//! - **mpris-signals**: abre su propia conexión y solo escucha `Seeked` y
//!   `NameOwnerChanged`. Las conexiones de D-Bus no admiten dos lectores, por
//!   eso va aparte.
//!
//! El `ProgressTracker` espera a que llegue una señal y solo entonces relee
//! las propiedades. El despertar cada 100 ms no es polling: entre eventos no
//! se consulta el bus, solo se interpola la posición.
//!
//! # Archivos
//!
//! - [`session`]: descubrir reproductores y armar el selector de sesiones.
//! - [`signals`]: escuchar los avisos del bus (`Seeked`,
//!   `NameOwnerChanged`).
//! - [`timeline`]: ancla, interpolación y tolerancia a los bugs de posición.
//! - [`player`]: comandos y construcción de la foto para la UI.

mod player;
mod session;
mod signals;
mod timeline;

use std::time::Instant;

use crossbeam_channel::Receiver;
use mpris::Player;

use crate::media::{Command, Sink, Snapshot};

use self::session::Catalog;
use self::signals::Signal;
use self::timeline::{Observation, Timeline};

/// Cada cuánto se refresca la posición interpolada.
///
/// También es la latencia máxima de un clic en los botones, porque el hilo
/// espera acá: el clic se atiende justo después del próximo tick.
const INTERVAL_MS: u32 = 100;

/// Qué tiene que hacer el bucle después de atender al reproductor.
enum Outcome {
    /// El reproductor terminó: buscar otro.
    Gone,
    /// El usuario eligió otra sesión.
    Switch(String),
}

pub fn spawn(cmd_rx: Receiver<Command>, sink: Sink) {
    std::thread::Builder::new()
        .name("mpris".to_string())
        .spawn(move || {
            if let Err(err) = run(cmd_rx, sink) {
                eprintln!("[media] el backend MPRIS se detuvo: {err}");
            }
        })
        .expect("no se pudo crear el hilo de medios");
}

fn run(cmd_rx: Receiver<Command>, sink: Sink) -> Result<(), mpris::DBusError> {
    let mut catalog = Catalog::new()?;
    let signal_rx = signals::spawn();

    let mut current = catalog.pick(None, None);
    let mut last: Option<String> = None;

    loop {
        let Some(player) = current.as_ref() else {
            // No hay ningún reproductor: avisar y esperar un aviso del bus
            // (o reintentar pasado un tiempo si el bus está callado).
            sink(Snapshot::no_session());
            wait_without_player(&signal_rx, &cmd_rx);
            current = catalog.pick(last.as_deref(), None);
            continue;
        };

        let bus_name = player.bus_name().to_string();
        let identity = player.identity().to_string();
        last = Some(bus_name.clone());

        match handle(player, &cmd_rx, &signal_rx, &sink, &mut catalog)? {
            Outcome::Gone => {
                eprintln!("[media] el reproductor se fue: {bus_name}");
                // Se busca otro del mismo programa, pero no el que se acaba
                // de ir: si vuelve a aparecer en el bus, el bucle se
                // engancharía con él.
                current = catalog.pick(Some(&identity), Some(&bus_name));
            }
            Outcome::Switch(id) => {
                eprintln!("[media] se elige otra sesión: {id}");
                current = catalog.pick(Some(&id), None);
            }
        }
    }
}

/// Atiende el reproductor actual hasta que termina.
fn handle(
    player: &Player,
    cmd_rx: &Receiver<Command>,
    signal_rx: &Receiver<Signal>,
    sink: &Sink,
    catalog: &mut Catalog,
) -> Result<Outcome, mpris::DBusError> {
    let unique_name = player.unique_name();
    let mut tracker = player.track_progress(INTERVAL_MS)?;
    let mut controls = player::Controls::read(player);
    let mut timeline = {
        // `ProgressTracker::new` ya leyó el estado inicial; el primer `tick`
        // solo deja pasar el intervalo y entrega esa foto.
        let first = tracker.tick();
        Timeline::new(first.progress, Instant::now())
    };

    eprintln!(
        "[media] reproductor MPRIS: {} ({})",
        player.identity(),
        player.bus_name()
    );

    loop {
        // Espera la señal de D-Bus o el fin del intervalo, lo que pase
        // primero. Las propiedades solo se releen si llegó algo.
        let tick = tracker.tick();
        let now = Instant::now();

        // Los comandos de la UI se atienden sin bloquear: `tick()` ya
        // esperó el intervalo. `SelectSession` se resuelve después de drenar
        // todo, para no dejar comandos viejos para la sesión siguiente.
        let mut switch = None;
        let mut pending = Vec::new();
        while let Ok(command) = cmd_rx.try_recv() {
            match command {
                Command::SelectSession(id) => switch = Some(id),
                other => pending.push(other),
            }
        }
        if let Some(id) = switch {
            return Ok(Outcome::Switch(id));
        }

        // Avisos del bus: cambios de reproductores y seeks del actual.
        let mut bus_changed = false;
        let mut seeked = None;
        while let Ok(signal) = signal_rx.try_recv() {
            match signal {
                Signal::BusChanged => bus_changed = true,
                // Un Seeked del reproductor anterior no debe mover la línea
                // de tiempo del nuevo.
                other => {
                    if let Some(position_us) = seeked_position(other, unique_name) {
                        seeked = Some(position_us);
                    }
                }
            }
        }
        if bus_changed {
            catalog.refresh();
        }

        if tick.player_quit {
            return Ok(Outcome::Gone);
        }

        // Primero la lectura cruda (estado, metadatos, tasa y, si es
        // confiable, posición), y después el `Seeked`: su payload siempre
        // gana. Así el `0` que Firefox deja en `Position` no pisa el seek.
        if tick.progress_changed {
            controls = player::Controls::read(player);
            timeline.observe(Observation::from_progress(tick.progress), now);
        }
        if let Some(position_us) = seeked {
            timeline.seek_to(position_us.saturating_mul(10), now);
        }

        // Recién ahora los comandos, para que un seek relativo use la
        // posición más nueva que conocemos.
        for command in pending {
            player::execute(
                command,
                player,
                tick.progress.metadata(),
                &mut timeline,
                now,
            );
        }

        sink(player::snapshot(
            &timeline,
            tick.progress,
            player,
            catalog.sessions(),
            controls,
            Instant::now(),
        ));
    }
}

/// La posición de un `Seeked`, si viene del reproductor que se está mirando.
///
/// El hilo de señales filtra por su objetivo actual, pero un aviso del
/// reproductor anterior puede quedar en la cola justo cuando se cambia de
/// sesión. Este segundo filtro evita que ese aviso mueva la línea de tiempo
/// equivocada.
fn seeked_position(signal: Signal, current: &str) -> Option<i64> {
    match signal {
        Signal::Seeked {
            player,
            position_us,
        } if player == current => Some(position_us),
        _ => None,
    }
}

/// Espera un cambio en el bus, un comando o el reintento periódico.
///
/// Sin reproductor no hay propiedades que consultar: esto no es polling del
/// estado, es la espera de un aviso de que apareció alguien.
fn wait_without_player(signal_rx: &Receiver<Signal>, cmd_rx: &Receiver<Command>) {
    let deadline = Instant::now() + session::RETRY_WITHOUT_PLAYER;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }

        crossbeam_channel::select! {
            recv(signal_rx) -> message => match message {
                Ok(Signal::BusChanged) => return,
                Ok(Signal::Seeked { .. }) => {}
                Err(_) => {
                    // El hilo de señales se cayó: reintentar por tiempo.
                    std::thread::sleep(left);
                    return;
                }
            },
            recv(cmd_rx) -> message => {
                if message.is_err() {
                    std::thread::sleep(left);
                    return;
                }
                // Sin reproductor no hay nada que ejecutar; se descarta.
            }
            default(left) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeked(player: &str, position_us: i64) -> Signal {
        Signal::Seeked {
            player: player.to_string(),
            position_us,
        }
    }

    #[test]
    fn accepts_a_seek_from_the_current_player() {
        assert_eq!(seeked_position(seeked(":1.54", 42), ":1.54"), Some(42));
    }

    #[test]
    fn ignores_a_seek_left_over_from_another_player() {
        assert_eq!(seeked_position(seeked(":1.53", 42), ":1.54"), None);
    }

    #[test]
    fn a_bus_change_is_not_a_seek() {
        assert_eq!(seeked_position(Signal::BusChanged, ":1.54"), None);
    }
}
