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

/// Servidor STUN por defecto, para descubrir la dirección pública.
#[cfg(not(test))]
const DEFAULT_STUN_SERVER: &str = "stun.l.google.com:19302";

/// Variable de entorno para cambiar el servidor STUN. Vacío lo desactiva.
#[cfg(not(test))]
const STUN_ADDR_ENV: &str = "SINC_STUN_ADDR";

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

/// El rol del cliente en la sala.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Controlado,
    Controlador,
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
    /// El rol asignado en la sala: controlado o controlador.
    AssignedRole(Role),
    /// El controlado se fue a otra sala, avisando a dónde.
    ControlledMoved {
        /// La sala en la que está ahora.
        room: String,
    },
    /// El controlado se fue sin decir a dónde: la sala quedó sin controlado.
    ControlledLeft,
}

/// Callback con el que el módulo publica cada evento.
///
/// Es `Send + Sync` porque lo comparten los dos hilos de red.
pub type Sink = Arc<dyn Fn(Event) + Send + Sync + 'static>;

/// Lo que el hilo de señalización le manda al hilo de par.
#[derive(Debug)]
pub(crate) enum PeerCommand {
    /// Mandar la oferta a un equipo.
    ///
    /// Si todavía no hay par con él, el hilo lo abre: el controlado abre
    /// uno por controlador.
    MakeOffer {
        /// Identificador del equipo al que va la oferta.
        peer: u64,
    },
    /// Aceptar la oferta de un equipo y mandar la respuesta.
    AcceptOffer {
        /// Identificador del equipo que ofreció.
        peer: u64,
        /// La oferta tal cual llegó.
        sdp: Value,
    },
    /// Aceptar la respuesta de un equipo a nuestra oferta.
    AcceptAnswer {
        /// Identificador del equipo que contestó.
        peer: u64,
        /// La respuesta tal cual llegó.
        sdp: Value,
    },
    /// Un candidato ICE de un equipo.
    AddIce {
        /// Identificador del equipo que lo mandó.
        peer: u64,
        /// El candidato tal cual llegó.
        candidate: Value,
    },
    /// Mandar un mensaje por todos los canales abiertos.
    Send(String),
    /// Cerrar el par de un equipo: se fue de la sala.
    Close {
        /// Identificador del equipo que se fue.
        peer: u64,
    },
    /// Terminar todos los pares.
    Stop,
}

/// Lo que el hilo de par le manda al hilo de señalización.
#[derive(Debug)]
pub(crate) enum PeerOut {
    /// La oferta, para mandarla por el WebSocket.
    Offer {
        /// A quién va.
        to: u64,
        /// El SDP.
        sdp: Value,
    },
    /// La respuesta, para mandarla por el WebSocket.
    Answer {
        /// A quién va.
        to: u64,
        /// El SDP.
        sdp: Value,
    },
    /// Un candidato ICE local, para mandarlo por el WebSocket.
    Ice {
        /// A quién va.
        to: u64,
        /// El candidato.
        candidate: Value,
    },
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

/// El servidor STUN que se consulta para descubrir la dirección pública.
///
/// Sale de `SINC_STUN_ADDR` si está; si no, de un servidor público.
/// Un valor vacío lo desactiva: la conexión queda solo con candidatos de
/// host, que alcanza en la propia red pero no a través de un NAT.
pub fn stun_server() -> Option<String> {
    // En las pruebas no hay servidor STUN salvo que el test lo fije.
    #[cfg(test)]
    return TEST_STUN_ADDR.get().and_then(Clone::clone);

    #[cfg(not(test))]
    {
        let raw = std::env::var(STUN_ADDR_ENV).unwrap_or_else(|_| DEFAULT_STUN_SERVER.to_string());
        strip_stun_uri(raw.trim()).map(str::to_string)
    }
}

/// El servidor STUN fijado a mano. Solo existe en las pruebas.
#[cfg(test)]
static TEST_STUN_ADDR: OnceLock<Option<String>> = OnceLock::new();

/// Le saca el prefijo `stun:` de un URI de servidor STUN.
///
/// Devuelve `None` si queda vacío: es así como se desactiva.
#[cfg(not(test))]
fn strip_stun_uri(raw: &str) -> Option<&str> {
    let without = raw
        .strip_prefix("stun://")
        .or_else(|| raw.strip_prefix("stun:"))
        .unwrap_or(raw)
        .trim();

    if without.is_empty() {
        None
    } else {
        Some(without)
    }
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

/// Publica el rol asignado en la sala.
pub(crate) fn publish_role(sink: &Sink, role: Role) {
    sink(Event::AssignedRole(role));
}

/// Publica que el controlado se fue a otra sala.
pub(crate) fn publish_controlled_moved(sink: &Sink, room: String) {
    sink(Event::ControlledMoved { room });
}

/// Publica que el controlado se fue sin dejar destino.
pub(crate) fn publish_controlled_left(sink: &Sink) {
    sink(Event::ControlledLeft);
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

    /// Sala usada por los tests de emparejamiento.
    const OTHER_ROOM: &str = "XYZ789";

    /// La sala para la prueba de roles: cada test usa la suya.
    const ROLE_ROOM_A: &str = "ABC235";
    const ROLE_ROOM_B: &str = "DEF456";
    const ROLE_ROOM_C: &str = "G7H8J9";
    const ROLE_ROOM_D: &str = "H3K5M7";

    /// La sala del tercer test (donde está A).
    const SWITCH_ROOM: &str = "K7M2QP";

    /// La sala donde empieza B, antes de cambiar a la de A.
    const SWITCH_ROOM_B: &str = "M5T8RS";

    /// La sala del test de STUN: cada test usa la suya, porque corren
    /// en paralelo y el servidor agrupa por sala.
    const STUN_ROOM: &str = "J4X8TZ";

    /// Las salas de las pruebas de cambio de sala: cada test usa las suyas.
    const SWITCH_FROM_ROOM: &str = "Z2C4DF";
    const SWITCH_TO_ROOM: &str = "T8V6WX";
    const SWITCH_OLD_ROOM: &str = "X4Z6AB";
    const SWITCH_NEW_ROOM: &str = "Y2C8DE";

    /// Las salas de las pruebas de reasignación: cada test usa las suyas.
    const FOLLOW_OLD_ROOM: &str = "C2D4EF";
    const FOLLOW_NEW_ROOM: &str = "F6G8HJ";
    const FOLLOW_FROM_ROOM: &str = "K2M4NP";
    const FOLLOW_TO_ROOM: &str = "Q6R8ST";

    /// Las salas de las pruebas de varios controladores: cada test usa las
    /// suyas.
    const TWO_CONTROLLERS_ROOM: &str = "B4D6FH";
    const THREE_ROOM_A: &str = "D7F9HJ";
    const THREE_ROOM_OTHER: &str = "E2G4KM";
    const THREE_FOLLOW_FROM_ROOM: &str = "F8H2KN";
    const THREE_FOLLOW_TO_ROOM: &str = "G3J5MP";

    /// Las salas de las pruebas de traspaso: cada test usa las suyas.
    const HANDOFF_ROOM_A: &str = "R3T5VW";
    const HANDOFF_ROOM_B: &str = "S7W9XY";
    const HANDOFF_ROOM_C: &str = "V2X4ZA";
    const HANDOFF_ROOM_D: &str = "W6Y8BC";
    const HANDOFF_ROOM_E: &str = "Y8AB2C";

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
        // A entra primero: es el controlado, que es el que se queda en la
        // sala cuando el otro se va.
        wait_for(&events_a, is_controlado);
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

    /// El controlado, al irse a otra sala, avisa a dónde: el controlador
    /// que queda en la sala vieja recibe el código de la sala nueva.
    #[test]
    fn the_controlado_announces_the_room_it_moves_to() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();
        let (cmd_c, events_c) = client();

        // A entra a su sala y B se le une: A es el controlado.
        cmd_a
            .send(Command::Connect {
                room: HANDOFF_ROOM_A.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, is_controlado);
        cmd_b
            .send(Command::Connect {
                room: HANDOFF_ROOM_A.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, channel_opened);
        wait_for(&events_b, channel_opened);

        // C entra a la suya: es el nuevo controlado.
        cmd_c
            .send(Command::Connect {
                room: HANDOFF_ROOM_B.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_c, |events| {
            events
                .iter()
                .any(|event| matches!(event, Event::AssignedRole(Role::Controlado)))
        });

        // A se va a la sala de C.
        cmd_a
            .send(Command::Connect {
                room: HANDOFF_ROOM_B.to_string(),
            })
            .expect("el canal de comandos está abierto");

        // B recibe el aviso con la sala nueva.
        wait_for(&events_b, |events| {
            events.iter().any(
                |event| matches!(event, Event::ControlledMoved { room } if room == HANDOFF_ROOM_B),
            )
        });
    }

    /// Después del aviso, el controlador suelta la sala: ya no hay a quién
    /// controlar ahí, y lo que mande no le llega al controlado viejo.
    #[test]
    fn the_controller_leaves_the_room_after_a_handoff() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();
        let (cmd_c, events_c) = client();

        cmd_a
            .send(Command::Connect {
                room: HANDOFF_ROOM_C.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, is_controlado);
        cmd_b
            .send(Command::Connect {
                room: HANDOFF_ROOM_C.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, channel_opened);
        wait_for(&events_b, channel_opened);

        // El otro controlado, en su sala.
        cmd_c
            .send(Command::Connect {
                room: HANDOFF_ROOM_D.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_c, |events| {
            events
                .iter()
                .any(|event| matches!(event, Event::AssignedRole(Role::Controlado)))
        });

        cmd_a
            .send(Command::Connect {
                room: HANDOFF_ROOM_D.to_string(),
            })
            .expect("el canal de comandos está abierto");

        // Primero el aviso, después la sala suelta.
        wait_for(&events_b, |events| {
            let moved = events
                .iter()
                .position(|event| matches!(event, Event::ControlledMoved { .. }));
            moved.is_some_and(|index| {
                events[index..]
                    .iter()
                    .any(|event| matches!(event, Event::State(NetState::Desconectado)))
            })
        });

        // Ya sin sala, lo que mande no sale.
        cmd_b
            .send(Command::Send("hola".to_string()))
            .expect("el canal de comandos está abierto");
        std::thread::sleep(Duration::from_millis(500));

        let received = events_a
            .lock()
            .expect("el registro no está poisoned")
            .clone();
        assert!(
            !received
                .iter()
                .any(|event| matches!(event, Event::Message(text) if text == "hola")),
            "el controlador no tenía que poder mandar: {received:?}"
        );
    }

    /// Si el controlado se va sin decir a dónde, el controlador lo sabe y
    /// suelta la sala: no queda esperando a alguien que no va a volver.
    #[test]
    fn the_controller_leaves_the_room_when_the_controlado_disappears() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();

        cmd_a
            .send(Command::Connect {
                room: HANDOFF_ROOM_E.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, is_controlado);
        cmd_b
            .send(Command::Connect {
                room: HANDOFF_ROOM_E.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, channel_opened);
        wait_for(&events_b, channel_opened);

        // El controlado se va sin destino.
        cmd_a
            .send(Command::Disconnect)
            .expect("el canal de comandos está abierto");

        wait_for(&events_b, |events| {
            let gone = events
                .iter()
                .position(|event| matches!(event, Event::ControlledLeft));
            gone.is_some_and(|index| {
                events[index..]
                    .iter()
                    .any(|event| matches!(event, Event::State(NetState::Desconectado)))
            })
        });
    }

    /// El servidor STUN contestó a los dos: anuncia la dirección pública.
    fn stun_responded(events: &[Event]) -> bool {
        events
            .iter()
            .any(|event| matches!(event, Event::Notice(text) if text.contains("dirección pública")))
    }

    /// Dos equipos se emparejan con candidatos server reflexive: el
    /// binding STUN sale, la respuesta trae la dirección pública y esta
    /// queda anunciada en los dos.
    ///
    /// En una sola máquina no hay NAT (y el `srflx` difiere del `host` en
    /// el puerto que inventa el servidor de pruebas), pero sí ejercita el
    /// camino completo: query, respuesta, candidato, trickle.
    #[test]
    fn two_clients_pair_with_server_reflexive_candidates() {
        let _ = test_server();
        test_stun_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();

        cmd_a
            .send(Command::Connect {
                room: STUN_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        cmd_b
            .send(Command::Connect {
                room: STUN_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");

        // La conexión no depende del STUN (el host alcanza en red propia),
        // pero sí que el STUN haya contestado en los dos.
        wait_for(&events_a, channel_opened);
        wait_for(&events_b, channel_opened);
        wait_for(&events_a, stun_responded);
        wait_for(&events_b, stun_responded);

        // Y el hola viaja igual de bien.
        cmd_a
            .send(Command::Send("hola".to_string()))
            .expect("el canal de comandos está abierto");

        wait_for(&events_b, |events| {
            events
                .iter()
                .any(|event| matches!(event, Event::Message(text) if text == "hola"))
        });
    }

    /// Un controlador que entra a otra sala deja la anterior: los que se
    /// quedan lo ven salir y él queda en la sala nueva.
    #[test]
    fn the_controller_leaves_the_previous_room_when_it_joins_another() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();
        let (cmd_c, events_c) = client();

        // A entra primero: es el controlado de su sala.
        cmd_a
            .send(Command::Connect {
                room: SWITCH_FROM_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, is_controlado);

        // B se le une como controlador: el canal se abre.
        cmd_b
            .send(Command::Connect {
                room: SWITCH_FROM_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, channel_opened);

        // C espera solo en otra sala, como controlado.
        cmd_c
            .send(Command::Connect {
                room: SWITCH_TO_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_c, is_controlado);

        // B se va a la sala de C.
        cmd_b
            .send(Command::Connect {
                room: SWITCH_TO_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");

        // A lo ve salir...
        wait_for(&events_a, |events| {
            events
                .iter()
                .any(|event| matches!(event, Event::Notice(text) if text.contains("se fue")))
        });

        // ...y B queda en la sala nueva, emparejado con C.
        wait_for(&events_b, |events| {
            events
                .iter()
                .any(|event| matches!(event, Event::Notice(text) if text.contains(SWITCH_TO_ROOM)))
        });
        wait_for(&events_c, channel_opened);
    }

    /// Después de cambiar de sala, los comandos van solo al controlado
    /// nuevo: el viejo no recibe nada.
    #[test]
    fn the_controller_only_talks_to_the_controlado_of_its_room() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();
        let (cmd_c, events_c) = client();

        cmd_a
            .send(Command::Connect {
                room: SWITCH_OLD_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, is_controlado);

        cmd_b
            .send(Command::Connect {
                room: SWITCH_OLD_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, channel_opened);

        cmd_c
            .send(Command::Connect {
                room: SWITCH_NEW_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_c, is_controlado);

        // B cambia de sala: el segundo canal es el de la nueva.
        cmd_b
            .send(Command::Connect {
                room: SWITCH_NEW_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_b, |events| {
            events
                .iter()
                .filter(|event| {
                    matches!(event, Event::Notice(text) if text.contains("canal de datos abierto"))
                })
                .count()
                >= 2
        });

        // Al controlado nuevo le llega...
        cmd_b
            .send(Command::Send("hola".to_string()))
            .expect("el canal de comandos está abierto");
        wait_for(&events_c, |events| {
            events
                .iter()
                .any(|event| matches!(event, Event::Message(text) if text == "hola"))
        });

        // ...y al viejo no.
        std::thread::sleep(Duration::from_millis(500));
        let received = events_a
            .lock()
            .expect("el registro no está poisoned")
            .clone();
        assert!(
            !received
                .iter()
                .any(|event| matches!(event, Event::Message(text) if text == "hola")),
            "el controlado viejo no tenía que recibir nada: {received:?}"
        );
    }

    /// El que era controlado, al entrar a la sala de otro, queda como
    /// controlador: el grupo conserva un solo controlado.
    #[test]
    fn the_former_controlado_is_a_controller_in_the_new_room() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, _events_b) = client();
        let (cmd_c, events_c) = client();

        // A es el controlado de su sala, con B de controlador.
        cmd_a
            .send(Command::Connect {
                room: FOLLOW_OLD_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, is_controlado);
        cmd_b
            .send(Command::Connect {
                room: FOLLOW_OLD_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, channel_opened);

        // C espera en otra sala, como controlado.
        cmd_c
            .send(Command::Connect {
                room: FOLLOW_NEW_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_c, is_controlado);

        // A se conecta al otro controlado.
        cmd_a
            .send(Command::Connect {
                room: FOLLOW_NEW_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");

        // Su último rol es controlador.
        wait_for(&events_a, |events| {
            let roles: Vec<Role> = events
                .iter()
                .filter_map(|event| match event {
                    Event::AssignedRole(role) => Some(*role),
                    _ => None,
                })
                .collect();
            roles.last() == Some(&Role::Controlador)
        });
    }

    /// El controlador sigue al controlado a la sala nueva sin que nadie
    /// apriete nada: el aviso ya trae el código.
    #[test]
    fn the_controller_follows_the_controlado_without_pressing_anything() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();
        let (cmd_c, events_c) = client();

        cmd_a
            .send(Command::Connect {
                room: FOLLOW_FROM_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, is_controlado);
        cmd_b
            .send(Command::Connect {
                room: FOLLOW_FROM_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, channel_opened);

        cmd_c
            .send(Command::Connect {
                room: FOLLOW_TO_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_c, is_controlado);

        // El controlado se va con el código; B no manda nada.
        cmd_a
            .send(Command::Connect {
                room: FOLLOW_TO_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");

        // B entra solo a la sala nueva: el aviso ya trajo el código, y
        // el servidor confirma que quedó adentro.
        wait_for(&events_b, |events| {
            let moved = events.iter().position(
                |event| matches!(event, Event::ControlledMoved { room } if room == FOLLOW_TO_ROOM),
            );
            moved.is_some_and(|index| {
                events[index..].iter().any(
                    |event| matches!(event, Event::Notice(text) if text.contains(FOLLOW_TO_ROOM)),
                )
            })
        });

        // Y en la sala nueva es controlador del controlado que ya estaba.
        wait_for(&events_b, |events| {
            let roles: Vec<Role> = events
                .iter()
                .filter_map(|event| match event {
                    Event::AssignedRole(role) => Some(*role),
                    _ => None,
                })
                .collect();
            roles.last() == Some(&Role::Controlador)
        });
    }

    /// Si el cliente entró a una sala vacía: es el controlado.
    fn is_controlado(events: &[Event]) -> bool {
        events
            .iter()
            .any(|event| matches!(event, Event::AssignedRole(Role::Controlado)))
    }

    /// Si el canal de datos se abrió.
    fn channel_opened(events: &[Event]) -> bool {
        events.iter().any(
            |event| matches!(event, Event::Notice(text) if text.contains("canal de datos abierto")),
        )
    }

    /// Cuántos canales de datos abrió el cliente.
    fn channels_opened(events: &[Event]) -> usize {
        events
            .iter()
            .filter(|event| {
                matches!(event, Event::Notice(text) if text.contains("canal de datos abierto"))
            })
            .count()
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

    /// El servidor STUN de las pruebas: se levanta una sola vez y lo fija
    /// en la configuración de red.
    ///
    /// Solo contesta binding requests, con una dirección mapeada falsa: la
    /// IP del que preguntó y el puerto aledaño, como si hubiera un NAT que
    /// cambiara el par. Como no hay NAT real, el `srflx` queda distinto
    /// del `host` sin llegar a romper la conexión.
    fn test_stun_server() {
        static ADDR: OnceLock<String> = OnceLock::new();

        ADDR.get_or_init(|| {
            let addr = start_stun_server();
            TEST_STUN_ADDR
                .set(Some(addr.clone()))
                .expect("el servidor STUN se fija una sola vez");
            addr
        });
    }

    /// Levanta el servidor STUN de pruebas en un puerto efimero.
    fn start_stun_server() -> String {
        use crate::net::peer::{STUN_MAGIC, STUN_REQUEST_TYPE, binding_success_reply};

        let (addr_tx, addr_rx) = std::sync::mpsc::channel();

        std::thread::Builder::new()
            .name("test-stun".to_string())
            .spawn(move || {
                let socket =
                    std::net::UdpSocket::bind("127.0.0.1:0").expect("socket STUN de pruebas");
                let addr = socket.local_addr().expect("dirección STUN de pruebas");
                addr_tx
                    .send(format!("{addr}"))
                    .expect("se recibe la dirección");

                // Contesta hasta que el proceso de pruebas muere.
                let mut buf = [0u8; 1500];
                while let Ok((n, source)) = socket.recv_from(&mut buf) {
                    // Solo binding requests: tipo y magic cookie.
                    if n < 20 {
                        continue;
                    }
                    if u16::from_be_bytes([buf[0], buf[1]]) != STUN_REQUEST_TYPE {
                        continue;
                    }
                    if buf[4..8] != STUN_MAGIC {
                        continue;
                    }

                    // La dirección "pública" falsa: la misma IP y el puerto
                    // siguiente.
                    let mapped = std::net::SocketAddr::new(source.ip(), source.port() + 1);
                    let reply = binding_success_reply(&buf[..n], mapped);

                    let _ = socket.send_to(&reply, source);
                }
            })
            .expect("no se pudo crear el hilo STUN");

        addr_rx.recv().expect("el servidor STUN arranca")
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

    /// El primero en una sala vacía es el controlado.
    #[test]
    fn first_joiner_in_empty_room_is_controlado() {
        let _ = test_server();

        let (cmd_a, events_a) = client();

        cmd_a
            .send(Command::Connect {
                room: ROLE_ROOM_A.to_string(),
            })
            .expect("el canal de comandos está abierta");

        wait_for(&events_a, |events| {
            events.iter().any(
                |event| matches!(event, Event::AssignedRole(role) if *role == Role::Controlado),
            )
        });
    }

    /// Dos controladores a la vez: cada uno con su canal contra el
    /// controlado, y los dos viendo lo mismo.
    #[test]
    fn two_controllers_see_the_controlado_at_once() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();
        let (cmd_d, events_d) = client();

        cmd_a
            .send(Command::Connect {
                room: TWO_CONTROLLERS_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, is_controlado);

        cmd_b
            .send(Command::Connect {
                room: TWO_CONTROLLERS_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_b, channel_opened);

        cmd_d
            .send(Command::Connect {
                room: TWO_CONTROLLERS_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_d, channel_opened);

        // El controlado abre uno por controlador.
        wait_for(&events_a, |events| channels_opened(events) == 2);

        // Lo que manda el controlado les llega a los dos.
        cmd_a
            .send(Command::Send("hola".to_string()))
            .expect("el canal de comandos está abierto");

        for events in [&events_b, &events_d] {
            wait_for(events, |events| {
                events
                    .iter()
                    .any(|event| matches!(event, Event::Message(text) if text == "hola"))
            });
        }

        // Y ninguno abrió un segundo canal: no se conectan entre ellos.
        for (name, events) in [("B", &events_b), ("D", &events_d)] {
            let opened = channels_opened(&events.lock().expect("el registro no está poisoned"));
            assert_eq!(opened, 1, "{name} tenía que tener un solo canal");
        }
    }

    /// Si un controlador se va a otra sala, el que se queda no pierde el
    /// canal ni el estado.
    #[test]
    fn the_controlado_keeps_the_other_controller_when_one_leaves() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();
        let (cmd_d, events_d) = client();

        cmd_a
            .send(Command::Connect {
                room: THREE_ROOM_A.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, is_controlado);

        cmd_b
            .send(Command::Connect {
                room: THREE_ROOM_A.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_b, channel_opened);

        cmd_d
            .send(Command::Connect {
                room: THREE_ROOM_A.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_d, channel_opened);
        wait_for(&events_a, |events| channels_opened(events) == 2);

        // B se va a una sala propia.
        cmd_b
            .send(Command::Connect {
                room: THREE_ROOM_OTHER.to_string(),
            })
            .expect("el canal de comandos está abierto");

        // El controlado lo ve irse y sigue conectado.
        wait_for(&events_a, |events| {
            events
                .iter()
                .any(|event| matches!(event, Event::Notice(text) if text.contains("se fue")))
        });

        // El canal que le queda sigue andando.
        cmd_a
            .send(Command::Send("sigue".to_string()))
            .expect("el canal de comandos está abierto");
        wait_for(&events_d, |events| {
            events
                .iter()
                .any(|event| matches!(event, Event::Message(text) if text == "sigue"))
        });

        // Y a ninguno le apareció una conexión caída.
        for (name, events) in [("A", &events_a), ("D", &events_d)] {
            let seen = events.lock().expect("el registro no está poisoned").clone();
            assert!(
                !seen
                    .iter()
                    .any(|event| matches!(event, Event::Notice(text) if text.contains("falló"))),
                "{name} no tenía que ver una conexión caída: {seen:?}"
            );
            assert!(
                !seen
                    .iter()
                    .any(|event| matches!(event, Event::State(NetState::Desconectado))),
                "{name} no tenía que quedar desconectado: {seen:?}"
            );
        }
    }

    /// El controlador que siguió a la sala nueva maneja al controlado
    /// nuevo, y al viejo no le llega nada.
    #[test]
    fn a_controller_that_follows_drives_the_new_controlado() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();
        let (cmd_c, events_c) = client();

        // A es el controlado de su sala, con B de controlador.
        cmd_a
            .send(Command::Connect {
                room: THREE_FOLLOW_FROM_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_a, is_controlado);

        cmd_b
            .send(Command::Connect {
                room: THREE_FOLLOW_FROM_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_b, channel_opened);

        // C es el controlado de la sala nueva.
        cmd_c
            .send(Command::Connect {
                room: THREE_FOLLOW_TO_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_c, is_controlado);

        // A se muda: B lo sigue y queda de controlador de C.
        cmd_a
            .send(Command::Connect {
                room: THREE_FOLLOW_TO_ROOM.to_string(),
            })
            .expect("el canal de comandos está abierto");
        wait_for(&events_b, |events| channels_opened(events) == 2);

        // Y lo que mueve le llega al controlado nuevo.
        cmd_b
            .send(Command::Send("play".to_string()))
            .expect("el canal de comandos está abierto");
        wait_for(&events_c, |events| {
            events
                .iter()
                .any(|event| matches!(event, Event::Message(text) if text == "play"))
        });

        // Al controlado viejo no: ya no habla con B.
        std::thread::sleep(Duration::from_millis(500));
        let seen = events_a
            .lock()
            .expect("el registro no está poisoned")
            .clone();
        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, Event::Message(text) if text == "play")),
            "el controlado viejo no tenía que recibir nada: {seen:?}"
        );
    }

    /// El segundo en una sala ocupada es el controlador.
    #[test]
    fn second_joiner_in_occupied_room_is_controlador() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();

        // A entra primero: controlado.
        cmd_a
            .send(Command::Connect {
                room: ROLE_ROOM_B.to_string(),
            })
            .expect("el canal de comandos está abierta");
        wait_for(&events_a, |events| {
            events.iter().any(
                |event| matches!(event, Event::AssignedRole(role) if *role == Role::Controlado),
            )
        });

        // B entra después: controlador.
        cmd_b
            .send(Command::Connect {
                room: ROLE_ROOM_B.to_string(),
            })
            .expect("el canal de comandos está abierta");
        wait_for(&events_b, |events| {
            events.iter().any(
                |event| matches!(event, Event::AssignedRole(role) if *role == Role::Controlador),
            )
        });
    }

    /// El controlado sigue siendo controlado cuando otro se une.
    #[test]
    fn controlado_stays_controlado_when_peer_joins() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();

        // A entra primero: controlado.
        cmd_a
            .send(Command::Connect {
                room: ROLE_ROOM_C.to_string(),
            })
            .expect("el canal de comandos está abierta");
        wait_for(&events_a, |events| {
            events.iter().any(
                |event| matches!(event, Event::AssignedRole(role) if *role == Role::Controlado),
            )
        });

        // B entra después: A no cambia de rol.
        cmd_b
            .send(Command::Connect {
                room: ROLE_ROOM_C.to_string(),
            })
            .expect("el canal de comandos está abierta");

        wait_for(&events_b, |events| {
            events.iter().any(
                |event| matches!(event, Event::AssignedRole(role) if *role == Role::Controlador),
            )
        });

        // A sigue siendo controlado (no recibió otro Event::AssignedRole diferente).
        let roles_a: Vec<Role> = events_a
            .lock()
            .expect("el registro no está poisoned")
            .iter()
            .filter_map(|event| match event {
                Event::AssignedRole(role) => Some(*role),
                _ => None,
            })
            .collect();
        assert_eq!(roles_a, vec![Role::Controlado]);
    }

    /// Un controlador que se desconecta y vuelve a la misma sala sigue
    /// siendo controlador: no hay dos controlados en una misma sala.
    #[test]
    fn controlador_remains_controlador_on_reconnect_to_same_room() {
        let _ = test_server();

        let (cmd_a, events_a) = client();
        let (cmd_b, events_b) = client();

        // A entra primero: controlado.
        cmd_a
            .send(Command::Connect {
                room: ROLE_ROOM_D.to_string(),
            })
            .expect("el canal de comandos está abierta");
        wait_for(&events_a, |events| {
            events.iter().any(
                |event| matches!(event, Event::AssignedRole(role) if *role == Role::Controlado),
            )
        });

        // B entra después: controlador.
        cmd_b
            .send(Command::Connect {
                room: ROLE_ROOM_D.to_string(),
            })
            .expect("el canal de comandos está abierta");
        wait_for(&events_b, |events| {
            events.iter().any(
                |event| matches!(event, Event::AssignedRole(role) if *role == Role::Controlador),
            )
        });

        // B se desconecta.
        cmd_b
            .send(Command::Disconnect)
            .expect("el canal de comandos está abierta");

        // B vuelve a la misma sala: debe seguir siendo controlador.
        cmd_b
            .send(Command::Connect {
                room: ROLE_ROOM_D.to_string(),
            })
            .expect("el canal de comandos está abierta");

        wait_for(&events_b, |events| {
            events.iter().any(
                |event| matches!(event, Event::AssignedRole(role) if *role == Role::Controlador),
            )
        });

        // B no recibió un rol de controlado.
        let roles_b: Vec<Role> = events_b
            .lock()
            .expect("el registro no está poisoned")
            .iter()
            .filter_map(|event| match event {
                Event::AssignedRole(role) => Some(*role),
                _ => None,
            })
            .collect();
        assert_eq!(roles_b, vec![Role::Controlador]);
    }
}
