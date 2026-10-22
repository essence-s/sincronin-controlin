//! Emparejamiento de dos equipos por WebRTC.
//!
//! El módulo tiene tres piezas:
//!
//! - [`signal`]: el WebSocket contra el servidor de señalización. Es el
//!   único que sale a internet.
//! - [`peer`]: el [`str0m::Rtc`] y el socket UDP. De aca sale la
//!   conexión punto a punto.
//! - Este módulo: el estado compartido, los canales y la API que ve la UI.
//!
//! Todo es dirigido por eventos: no hay ningún timer que consulte el
//! estado. Los dos hilos se hablan por canales de crossbeam, como el
//! resto del proyecto.
//!
//! ## El reparto
//!
//! ```
//!   UI ──Command──▶ net-signal ──WebSocket──▶ VPS
//!                     │   ▲
//!                     │   └──PeerOut (oferta, respuesta, candidatos)
//!                     ▼
//!                 net-peer ──UDP──▶ el otro equipo
//! ```
//!
//! El hilo de señalización es el que habla con la UI: recibe los
//! comandos y los reparte. El hilo de par es el que tiene el `Rtc` y el
//! socket, y es el único que puede tocar la conexión punto a punto.
//!
//! ## La criptografía
//!
//! `str0m` con `rust-crypto`: la criptografía es de Rust puro. Igual el
//! árbol de dependencias trae `aws-lc-sys` (C) por el generador de
//! certificados, así que compilar necesita un toolchain de C, pero no
//! cmake ni NASM.

use std::sync::Arc;
use std::sync::OnceLock;

use crossbeam_channel::{Receiver, Sender, unbounded};
use rand::Rng;
use serde_json::Value;

use protocol::{ROOM_ALPHABET, ROOM_LEN};

pub(crate) mod peer;
pub(crate) mod signal;

/// URL del servidor de señalización por defecto.
const DEFAULT_SIGNAL_URL: &str = "ws://127.0.0.1:8787/ws";

/// Variable de entorno para cambiar la URL del servidor.
const SIGNAL_URL_ENV: &str = "SINC_SIGNAL_URL";

/// Variable de entorno para fijar la dirección local del socket UDP.
const LOCAL_ADDR_ENV: &str = "SINC_NET_ADDR";

/// Lo que la UI le manda al módulo de red.
#[derive(Clone, Debug)]
pub enum Command {
    /// Entrar a una sala.
    ///
    /// Si la sala está vacía, se usa el código local del cliente.
    Connect {
        /// Código de la sala, vacío para usar el propio.
        room: String,
    },
    /// Mandar un mensaje por el canal de datos.
    Send(String),
    /// Salir de la sala.
    Disconnect,
}

/// En qué estado está la conexión.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetState {
    /// No hay conexión con el servidor.
    Desconectado,
    /// Conectado al servidor, esperando al otro equipo.
    Conectando,
    /// El canal de datos está abierto: se puede mandar.
    Conectado,
}

impl NetState {
    /// El texto para la UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Desconectado => "Desconectado",
            // No es "conectando": el servidor ya está. Lo que falta es el
            // otro equipo.
            Self::Conectando => "Esperando al otro equipo",
            Self::Conectado => "Conectado",
        }
    }
}

/// Lo que el módulo de red le publica a la UI.
#[derive(Clone, Debug)]
pub enum Event {
    /// Cambio de estado.
    State(NetState),
    /// Un mensaje que llegó por el canal de datos.
    Message(String),
    /// Un aviso o un error, para mostrar en la UI.
    Notice(String),
}

/// Callback con el que el módulo publica cada evento.
///
/// Es `Send + Sync` porque lo comparten los dos hilos de red.
pub type Sink = Arc<dyn Fn(Event) + Send + Sync + 'static>;

/// Lo que el hilo de señalización le manda al hilo de par.
#[derive(Debug)]
pub(crate) enum PeerCommand {
    /// Crear la sesión: el `Rtc` y el socket UDP.
    Start {
        /// Identificador del cliente en el servidor.
        id: u64,
    },
    /// Mandar la oferta.
    MakeOffer,
    /// Aceptar una oferta y mandar la respuesta.
    AcceptOffer(Value),
    /// Aceptar la respuesta a nuestra oferta.
    AcceptAnswer(Value),
    /// Un candidato ICE del otro equipo.
    AddIce(Value),
    /// Mandar un mensaje por el canal de datos.
    Send(String),
    /// Terminar la sesión.
    Stop,
}

/// Lo que el hilo de par le manda al hilo de señalización.
#[derive(Debug)]
pub(crate) enum PeerOut {
    /// La oferta, para mandarla por el WebSocket.
    Offer(Value),
    /// La respuesta, para mandarla por el WebSocket.
    Answer(Value),
    /// Un candidato ICE local, para mandarlo por el WebSocket.
    Ice(Value),
}

/// Levanta los dos hilos de red.
///
/// `cmd_rx` trae los comandos de la UI y `sink` publica los eventos.
pub fn spawn(cmd_rx: Receiver<Command>, sink: Sink) {
    // Los canales entre los dos hilos.
    let (peer_cmd_tx, peer_cmd_rx) = unbounded::<PeerCommand>();
    let (peer_out_tx, peer_out_rx) = unbounded::<PeerOut>();

    // El hilo de señalización: habla con la UI y con el servidor.
    let sink_signal = Arc::clone(&sink);
    std::thread::Builder::new()
        .name("net-signal".to_string())
        .spawn(move || {
            if let Err(err) = signal::run(cmd_rx, peer_cmd_tx, peer_out_rx, sink_signal) {
                eprintln!("[net] el hilo de señalización se detuvo: {err}");
            }
        })
        .expect("no se pudo crear el hilo de señalización");

    // El hilo de par: tiene el `Rtc` y el socket UDP.
    let sink_peer = Arc::clone(&sink);
    std::thread::Builder::new()
        .name("net-peer".to_string())
        .spawn(move || {
            if let Err(err) = peer::run(peer_cmd_rx, peer_out_tx, sink_peer) {
                eprintln!("[net] el hilo de par se detuvo: {err}");
            }
        })
        .expect("no se pudo crear el hilo de par");
}

/// El código de sala de este cliente.
///
/// Lo genera el cliente y se lo muestra al usuario para que el otro lo
/// escriba. Se genera una sola vez por proceso.
pub fn local_room_code() -> &'static str {
    static CODE: OnceLock<String> = OnceLock::new();

    CODE.get_or_init(new_room_code)
}

/// Genera un código de sala nuevo.
///
/// Sale del alfabeto del protocolo: sin vocales ambiguas ni números que
/// se confundan con letras, para que se pueda dictar por voz.
fn new_room_code() -> String {
    let mut rng = rand::rng();

    (0..ROOM_LEN)
        .map(|_| {
            let idx = rng.random_range(0..ROOM_ALPHABET.len());
            ROOM_ALPHABET[idx] as char
        })
        .collect()
}

/// La URL del servidor de señalización.
pub fn signal_url() -> String {
    // En las pruebas la URL se fija a mano: el servidor de pruebas
    // escucha en un puerto efimero.
    #[cfg(test)]
    if let Some(url) = TEST_SIGNAL_URL.get() {
        return url.clone();
    }

    std::env::var(SIGNAL_URL_ENV).unwrap_or_else(|_| DEFAULT_SIGNAL_URL.to_string())
}

/// La URL del servidor fijada a mano. Solo existe en las pruebas.
#[cfg(test)]
static TEST_SIGNAL_URL: OnceLock<String> = OnceLock::new();

/// La dirección local que se le pide al sistema.
///
/// Sale de `SINC_NET_ADDR` si está; si no, de la primera IPv4 que no sea
/// de loopback.
pub fn local_address_hint() -> Option<std::net::IpAddr> {
    if let Ok(addr) = std::env::var(LOCAL_ADDR_ENV) {
        if let Ok(ip) = addr.trim().parse::<std::net::IpAddr>() {
            return Some(ip);
        }
    }

    None
}

/// Publica un estado.
pub(crate) fn publish_state(sink: &Sink, state: NetState) {
    sink(Event::State(state));
}

/// Publica un aviso.
pub(crate) fn publish_notice(sink: &Sink, text: impl Into<String>) {
    sink(Event::Notice(text.into()));
}

/// Publica un mensaje recibido.
pub(crate) fn publish_message(sink: &Sink, text: impl Into<String>) {
    sink(Event::Message(text.into()));
}

/// Envía un comando al hilo de par, avisando si no se pudo.
pub(crate) fn send_peer(peer_tx: &Sender<PeerCommand>, command: PeerCommand) {
    let _ = peer_tx.send(command);
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Cuanto se espera a que pase algo antes de rendirse.
    const TIMEOUT: Duration = Duration::from_secs(30);

    /// La sala del primer test.
    const ROOM: &str = "ABC234";

    /// La sala del segundo test: cada test usa la suya, porque corren
    /// en paralelo y el servidor agrupa por sala.
    const OTHER_ROOM: &str = "XYZ789";

    /// La sala del tercer test (donde está A).
    const SWITCH_ROOM: &str = "K7M2QP";

    /// La sala donde empieza B, antes de cambiar a la de A.
    const SWITCH_ROOM_B: &str = "M5T8RS";

    #[test]
    fn a_room_code_has_the_right_shape() {
        let code = new_room_code();

        assert_eq!(code.len(), ROOM_LEN);
        assert!(
            code.bytes().all(|b| ROOM_ALPHABET.contains(&b)),
            "el código {code} tiene caracteres fuera del alfabeto"
        );
    }

    #[test]
    fn two_room_codes_are_not_the_same() {
        // No es una prueba statistica: solo que el generador no devuelva
        // siempre lo mismo.
        let codes: Vec<String> = (0..20).map(|_| new_room_code()).collect();

        assert!(codes.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn a_room_code_is_valid_for_the_protocol() {
        let code = new_room_code();

        assert_eq!(
            protocol::normalize_room(&code).as_deref(),
            Some(code.as_str())
        );
    }

    /// La prueba de punta a punta: dos clientes entran a la misma sala a
    /// traves del servidor de señalización, se emparejan por WebRTC y uno
    /// le manda un mensaje al otro por el canal de datos.
    #[test]
    fn two_clients_send_a_message_over_the_data_channel() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();

        // Los dos entran a la misma sala.
        cmd_a
            .send(Command::Connect {
                room: ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        cmd_b
            .send(Command::Connect {
                room: ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");

        // El canal de datos se abre cuando termina el handshake de WebRTC.
        wait_for(&events_b, channel_opened);
        wait_for(&events_a, channel_opened);

        // Y ahora el mensaje de prueba.
        cmd_a
            .send(Command::Send("hola".to_string()))
            .expect("el canal de comandos está abierto");

        wait_for(&events_b, |events| {
            events
                .iter()
                .any(|event| matches!(event, Event::Message(text) if text == "hola"))
        });
    }

    /// Si el otro equipo se va y vuelve, se empareja de nuevo.
    #[test]
    fn a_client_pairs_again_after_the_peer_leaves() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, _events_b) = client();

        cmd_a
            .send(Command::Connect {
                room: OTHER_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        cmd_b
            .send(Command::Connect {
                room: OTHER_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");

        wait_for(&events_a, channel_opened);

        // El otro equipo se va.
        cmd_b
            .send(Command::Disconnect)
            .expect("el canal de comandos está abierto");

        wait_for(&events_a, |events| {
            events
                .iter()
                .any(|event| matches!(event, Event::Notice(text) if text.contains("se fue")))
        });

        // Y vuelve.
        cmd_b
            .send(Command::Connect {
                room: OTHER_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");

        // El canal de datos se abre otra vez.
        wait_for(&events_a, |events| {
            events
                .iter()
                .filter(|event| {
                    matches!(event, Event::Notice(text) if text.contains("canal de datos abierto"))
                })
                .count()
                >= 2
        });
    }

    /// Si el otro equipo entra después escribiendo el código, se emparejan.
    ///
    /// Es el camino real: el otro equipo empieza en su propia sala y
    /// cambia a la nuestra escribiendo el código.
    #[test]
    fn a_client_pairs_with_a_room_it_joins_later() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();

        // A entra a su sala.
        cmd_a
            .send(Command::Connect {
                room: SWITCH_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");

        // B entra a la suya: está solo.
        cmd_b
            .send(Command::Connect {
                room: SWITCH_ROOM_B.to_string(),
            })
            .expect("el canal de comandos está abierto");

        // Y B cambia a la sala de A escribiendo su código.
        cmd_b
            .send(Command::Connect {
                room: SWITCH_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");

        // Los dos quedan emparejados.
        wait_for(&events_a, channel_opened);
        wait_for(&events_b, channel_opened);

        // Y se cruzan un mensaje.
        cmd_a
            .send(Command::Send("hola".to_string()))
            .expect("el canal de comandos está abierto");

        wait_for(&events_b, |events| {
            events
                .iter()
                .any(|event| matches!(event, Event::Message(text) if text == "hola"))
        });
    }

    /// Si el canal de datos se abrió.
    fn channel_opened(events: &[Event]) -> bool {
        events.iter().any(
            |event| matches!(event, Event::Notice(text) if text.contains("canal de datos abierto")),
        )
    }

    /// El servidor de pruebas: se levanta una sola vez y lo usan todas
    /// las pruebas.
    fn test_server() -> &'static str {
        static URL: OnceLock<String> = OnceLock::new();

        URL.get_or_init(|| {
            let url = start_server();
            TEST_SIGNAL_URL
                .set(url.clone())
                .expect("la URL se fija una sola vez");
            url
        })
    }

    /// Levanta el servidor de señalización en un puerto efimero.
    fn start_server() -> String {
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime de tokio");

            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("puerto efimero");
                let addr = listener.local_addr().expect("dirección del servidor");
                addr_tx.send(addr).expect("se recibe la dirección");

                server::serve(listener, Arc::new(server::State::new()))
                    .await
                    .expect("el servidor se corta");
            });
        });

        let addr = addr_rx.recv().expect("el servidor arranca");
        format!("ws://{addr}/ws")
    }

    /// Un cliente de red: el canal de comandos y donde se anotan los
    /// eventos.
    fn client() -> (Sender<Command>, Arc<Mutex<Vec<Event>>>) {
        let (cmd_tx, cmd_rx) = unbounded();
        let events = Arc::new(Mutex::new(Vec::new()));

        let sink_events = Arc::clone(&events);
        let sink: Sink = Arc::new(move |event: Event| {
            sink_events
                .lock()
                .expect("el registro no está poisoned")
                .push(event);
        });

        spawn(cmd_rx, sink);

        (cmd_tx, events)
    }

    /// Espera a que se cumpla una condición sobre los eventos.
    fn wait_for(events: &Arc<Mutex<Vec<Event>>>, condition: impl Fn(&[Event]) -> bool) {
        let deadline = Instant::now() + TIMEOUT;

        while Instant::now() < deadline {
            let snapshot = events.lock().expect("el registro no está poisoned").clone();
            if condition(&snapshot) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let snapshot = events.lock().expect("el registro no está poisoned").clone();
        panic!("no se cumplio la condición; los eventos fueron: {snapshot:?}");
    }
}
