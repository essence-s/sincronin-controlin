//! El hilo de señalización: el WebSocket contra el servidor.
//!
//! Es el único que sale a internet. Por el viaja la oferta, la respuesta
//! y los candidatos ICE; los mensajes del usuario van de par a par, por el
//! canal de datos de WebRTC.
//!
//! El hilo hace dos cosas a la vez: atiende los comandos de la UI y lee
//! el WebSocket. Para que no se bloquee en una lectura, el socket tiene
//! un timeout corto: cuando pasa, el hilo vuelve al principio, drena los
//! comandos y sigue.

use std::io;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde_json::Value;
use tungstenite::WebSocket;
use tungstenite::stream::MaybeTlsStream;

use protocol::{ClientMessage, Frame, ServerMessage, VERSION};

use crate::net::{self, Command, NetState, PeerCommand, PeerOut, Role, Sink};

/// Cuanto espera el socket a una lectura antes de volver al bucle.
///
/// Es corto a propósito: el hilo tiene que atender los comandos de la UI
/// aunque el servidor no mande nada.
const READ_TIMEOUT: Duration = Duration::from_millis(100);

/// Cuanto espera el hilo cuando no hay conexión con el servidor.
const IDLE_WAIT: Duration = Duration::from_millis(100);

/// La primera espera antes de volver a la sala que se cayó.
const REJOIN_FIRST: Duration = Duration::from_secs(1);

/// El tope de la espera entre reintentos.
const REJOIN_MAX: Duration = Duration::from_secs(8);

/// La espera antes del reintento número `attempts`.
///
/// Crece al doble y se queda en [`REJOIN_MAX`]: ni un servidor que acaba
/// de reiniciar tarda ocho segundos en contestar, ni un corte largo se
/// reintenta cada segundo para siempre.
fn backoff(attempts: u32) -> Duration {
    REJOIN_FIRST
        .saturating_mul(1 << attempts.min(3))
        .min(REJOIN_MAX)
}

/// Errores del hilo de señalización.
#[derive(Debug)]
pub enum Error {
    /// Fallo de red.
    Io(io::Error),
    /// Fallo del WebSocket.
    Ws(tungstenite::Error),
    /// El JSON no se pudo serializar.
    Json(serde_json::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "error de red: {err}"),
            Self::Ws(err) => write!(f, "error del websocket: {err}"),
            Self::Json(err) => write!(f, "error de json: {err}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<tungstenite::Error> for Error {
    fn from(err: tungstenite::Error) -> Self {
        Self::Ws(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

/// El hilo de señalización.
pub(crate) fn run(
    cmd_rx: Receiver<Command>,
    peer_tx: Sender<PeerCommand>,
    peer_out_rx: Receiver<PeerOut>,
    sink: Sink,
) -> Result<(), Error> {
    let mut session = Session::new();

    loop {
        // 1) Los comandos de la UI.
        while let Ok(cmd) = cmd_rx.try_recv() {
            handle_command(&mut session, cmd, &peer_tx, &sink);
        }

        // 2) Lo que el hilo de par quiere que mandemos al otro equipo.
        while let Ok(out) = peer_out_rx.try_recv() {
            send_out(&mut session, out);
        }

        // 3) Sin conexión: esperar a que llegue un comando o a que toque
        //    volver a la sala que se cayó.
        if session.ws.is_none() {
            match cmd_rx.recv_timeout(session.retry_wait()) {
                Ok(cmd) => handle_command(&mut session, cmd, &peer_tx, &sink),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            retry_room(&mut session, &peer_tx, &sink);
            continue;
        }

        // 4) Leer del WebSocket.
        //
        // La lectura va aparte para soltar el préstamo del socket antes de
        // atender el mensaje: atenderlo puede tocar la sesión.
        let read = session.ws.as_mut().map(|socket| socket.read());
        let action = match read {
            Some(Ok(tungstenite::Message::Text(text))) => {
                handle_server(&mut session, text.as_str(), &peer_tx, &sink);
                Action::Keep
            }
            Some(Ok(tungstenite::Message::Close(_))) => {
                Action::Close("el servidor cerró la conexión".to_string())
            }
            // Los pings los contesta la libreria; el resto no nos interesa.
            Some(Ok(_)) => Action::Keep,
            // El timeout del socket: no es un error, es la manera de
            // volver al principio del bucle.
            Some(Err(tungstenite::Error::Io(err)))
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Action::Keep
            }
            Some(Err(err)) => Action::Close(format!("se cortó la conexión con el servidor: {err}")),
            // Con la conexión abierta no puede pasar: se comprueba arriba.
            None => Action::Keep,
        };

        // Si la conexión se corto, se cierra y se sigue escuchando: el
        // usuario puede volver a conectarse cuando el servidor vuelva.
        if let Action::Close(reason) = action {
            // Si eramos el controlado, la sala no se disuelve: sus
            // controladores se quedan esperando y se vuelve solo.
            let mut rejoin = session.rejoin.take();
            if rejoin.is_none() {
                rejoin = session.rejoin_intent();
            }

            net::publish_notice(&sink, reason);
            session.forget();
            net::send_peer(&peer_tx, PeerCommand::Stop);

            match rejoin {
                Some(rejoin) => {
                    eprintln!(
                        "[net] se cayó la conexión: vuelvo a la sala {}",
                        rejoin.room
                    );
                    net::publish_state(&sink, NetState::Conectando);
                    net::publish_notice(
                        &sink,
                        "se cayó la conexión: vuelvo a la sala en un momento",
                    );
                    session.rejoin = Some(rejoin);
                }
                None => net::publish_state(&sink, NetState::Desconectado),
            }
        }
    }

    // Se termino la sesión: el hilo de par también tiene que parar.
    net::send_peer(&peer_tx, PeerCommand::Stop);
    net::publish_state(&sink, NetState::Desconectado);

    Ok(())
}

/// Que hay que hacer después de leer del WebSocket.
enum Action {
    /// Nada raro: se sigue.
    Keep,
    /// La conexión se corto: hay que cerrarla.
    Close(String),
}

/// Lo que el hilo sabe de la sala en la que está.
///
/// Se arma al entrar y se olvida al salir: fuera de una sala no hay nada
/// que recordar.
struct Session {
    /// La conexión con el servidor. `None` mientras no haya ninguna.
    ws: Option<WebSocket<MaybeTlsStream<TcpStream>>>,
    /// El identificador que nos dio el servidor.
    our_id: Option<u64>,
    /// La sala en la que estamos.
    room: Option<String>,
    /// Si somos el controlado de la sala.
    controlado: bool,
    /// El identificador del controlado, cuando somos controlador.
    controlled: Option<u64>,
    /// Si al entrar hay que reclamar el rol de controlado.
    ///
    /// Lo pone la vuelta a la sala que se cayó: el que vuelve sigue siendo
    /// el controlado aunque encuentre gente esperando. Una entrada normal
    /// lo deja en `false`.
    claim: bool,
    /// La sala a la que hay que volver, si se cayó la conexión.
    rejoin: Option<Rejoin>,
}

/// La sala a la que hay que volver después de una caída.
///
/// La arma la caída de la conexión del controlado: sus controladores se
/// quedan esperando en la sala, y él vuelve solo. No es polling: la espera
/// es un plazo que el bucle respeta, y hasta que vence no toca la red.
struct Rejoin {
    /// La sala a la que se vuelve.
    room: String,
    /// Cuántos reintentos van.
    attempts: u32,
    /// Cuándo toca el próximo.
    at: Instant,
}

impl Session {
    fn new() -> Self {
        Self {
            ws: None,
            our_id: None,
            room: None,
            controlado: false,
            controlled: None,
            claim: false,
            rejoin: None,
        }
    }

    /// Se olvida de la sala: lo que se sabía de ella ya no vale.
    fn forget(&mut self) {
        self.ws = None;
        self.our_id = None;
        self.room = None;
        self.controlado = false;
        self.controlled = None;
        self.claim = false;
        self.rejoin = None;
    }

    /// La sala a la que hay que volver, si la conexión se cayó.
    ///
    /// Solo vuelve el controlado: el controlador no tiene sala propia a la
    /// que regresar (su sala es la del controlado, y ese es el que se cayó).
    fn rejoin_intent(&self) -> Option<Rejoin> {
        if !self.controlado {
            return None;
        }

        Some(Rejoin {
            room: self.room.clone()?,
            attempts: 0,
            at: Instant::now() + backoff(0),
        })
    }

    /// Cuánto falta para el próximo reintento.
    ///
    /// Sin vuelta pendiente, la espera de siempre: el hilo no tiene nada
    /// que hacer hasta que la UI mande un comando.
    fn retry_wait(&self) -> Duration {
        match &self.rejoin {
            Some(rejoin) => rejoin.at.saturating_duration_since(Instant::now()),
            None => IDLE_WAIT,
        }
    }

    /// Le avisa a la sala a dónde nos vamos, antes de irnos.
    ///
    /// Solo avisa el controlado, y solo si de verdad cambia de sala: el
    /// servidor reenvía el mensaje a los que se quedan.
    fn announce(&mut self, room: &str) {
        if !self.controlado || self.room.as_deref() == Some(room) {
            return;
        }

        let Some(socket) = self.ws.as_mut() else {
            return;
        };
        send_frame(
            socket,
            &Frame::new(ClientMessage::Controlled {
                room: room.to_string(),
            }),
        );
    }

    /// Se va de la sala, avisandole al servidor con protocolo.
    fn leave(&mut self) {
        if let Some(socket) = self.ws.as_mut() {
            send_frame(socket, &Frame::new(ClientMessage::Leave));
            let _ = socket.close(None);
        }
        self.forget();
    }
}

/// Atiende un comando de la UI.
fn handle_command(session: &mut Session, cmd: Command, peer_tx: &Sender<PeerCommand>, sink: &Sink) {
    match cmd {
        Command::Connect { room } => {
            // La sala: la que escribio el usuario o, si no escribio
            // ninguna, la nuestra.
            let room = if room.trim().is_empty() {
                net::local_room_code().to_string()
            } else {
                room.trim().to_string()
            };

            let Some(room) = protocol::normalize_room(&room) else {
                net::publish_notice(sink, "el código de sala no es válido");
                net::publish_state(sink, NetState::Desconectado);
                return;
            };

            enter(session, &room, peer_tx, sink, false);
        }
        Command::Send(text) => {
            net::send_peer(peer_tx, PeerCommand::Send(text));
        }
        Command::Disconnect => {
            // Avisamos al servidor para que le diga al otro equipo.
            session.leave();
            net::send_peer(peer_tx, PeerCommand::Stop);
            net::publish_state(sink, NetState::Desconectado);
            net::publish_notice(sink, "se cortó la conexión");
        }
    }
}

/// Entra a una sala: corta lo que había y abre la conexión nueva.
///
/// `claim` dice si hay que reclamar el rol de controlado al entrar: es la
/// vuelta a la sala que se cayó, donde los controladores esperan. Una
/// entrada normal a una sala con gente entra como controlador.
fn enter(
    session: &mut Session,
    room: &str,
    peer_tx: &Sender<PeerCommand>,
    sink: &Sink,
    claim: bool,
) {
    net::publish_state(sink, NetState::Conectando);

    eprintln!("[net] pido la sala {room}");

    // Si somos el controlado de la sala, primero avisamos a dónde vamos:
    // el servidor lo reenvía a los controladores que se quedan, así saben
    // a quién seguir.
    session.announce(room);

    // Si ya había una conexión, se cierra. Con protocolo, para que el
    // servidor no vea un reset.
    session.leave();
    net::send_peer(peer_tx, PeerCommand::Stop);

    match connect(room) {
        Ok(mut socket) => {
            // El primer mensaje: en que sala estamos.
            let frame = Frame::new(ClientMessage::Join {
                room: room.to_string(),
            });
            send_frame(&mut socket, &frame);
            session.ws = Some(socket);
            // Después de `leave`, que olvida todo: la intención es de esta
            // entrada, no de la anterior.
            session.claim = claim;
        }
        Err(err) => {
            net::publish_notice(sink, format!("no se pudo conectar con el servidor: {err}"));
            net::publish_state(sink, NetState::Desconectado);
        }
    }
}

/// Vuelve solo a la sala que se cayó, si ya toca.
///
/// La espera la hace el bucle con [`Session::retry_wait`]: acá solo se
/// mira el plazo, porque entre medio puede haber llegado un comando.
fn retry_room(session: &mut Session, peer_tx: &Sender<PeerCommand>, sink: &Sink) {
    let Some(mut rejoin) = session.rejoin.take() else {
        return;
    };

    if Instant::now() < rejoin.at {
        session.rejoin = Some(rejoin);
        return;
    }

    eprintln!(
        "[net] reintento {} de la sala {}",
        rejoin.attempts + 1,
        rejoin.room
    );

    enter(session, &rejoin.room, peer_tx, sink, true);

    if session.ws.is_some() {
        // El servidor contestó: `joined` confirma la sala y desarma la
        // vuelta. Si la conexión se cae antes, se reintenta enseguida.
        rejoin.attempts += 1;
        rejoin.at = Instant::now();
        session.rejoin = Some(rejoin);
        return;
    }

    // Sin servidor: se vuelve a probar, con más espera. `enter` avisó que
    // quedó desconectado, pero para el usuario la sala sigue en pie.
    rejoin.attempts += 1;
    rejoin.at = Instant::now() + backoff(rejoin.attempts);
    session.rejoin = Some(rejoin);
    net::publish_state(sink, NetState::Conectando);
}

/// Abre el WebSocket contra el servidor y entra a la sala.
fn connect(room: &str) -> Result<WebSocket<MaybeTlsStream<TcpStream>>, Error> {
    let base = net::signal_url();
    let sep = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{sep}room={room}");

    eprintln!("[net] conectando al servidor de señalización ({url})");

    let (mut socket, _response) = tungstenite::connect(url.as_str())?;

    eprintln!("[net] servidor conectado");

    // El timeout de lectura: sin el, el hilo se queda pegado en el read
    // y no atiende los comandos de la UI.
    set_read_timeout(&mut socket, READ_TIMEOUT)?;

    Ok(socket)
}

/// Le pone un timeout de lectura al socket del WebSocket.
fn set_read_timeout(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    timeout: Duration,
) -> io::Result<()> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream.set_read_timeout(Some(timeout)),
        // Con TLS, el timeout va en el socket de adentro.
        MaybeTlsStream::NativeTls(stream) => stream.get_mut().set_read_timeout(Some(timeout)),
        _ => Ok(()),
    }
}

/// Atiende un mensaje del servidor.
fn handle_server(session: &mut Session, text: &str, peer_tx: &Sender<PeerCommand>, sink: &Sink) {
    let frame: Frame<ServerMessage> = match serde_json::from_str(text) {
        Ok(frame) => frame,
        Err(err) => {
            net::publish_notice(
                sink,
                format!("mensaje del servidor que no se entiende: {err}"),
            );
            return;
        }
    };

    if frame.version != VERSION {
        net::publish_notice(
            sink,
            format!(
                "el servidor habla la versión {} y este cliente la {VERSION}",
                frame.version
            ),
        );
        return;
    }

    match frame.body {
        ServerMessage::Joined { room, id, peers } => {
            session.our_id = Some(id);
            session.room = Some(room.clone());
            // La sala ya está confirmada: no hay más que reintentar.
            session.rejoin = None;

            // El primero de la sala es el controlado. El que llega después
            // es controlador, y espera que el controlado le ofrezca: no
            // hay que mirar quién tiene el identificador más chico.
            //
            // La excepción es la vuelta: el controlado que se cayó y vuelve
            // a su sala reclama el rol aunque haya gente esperando. Si no,
            // entraría como controlador y nadie de la sala le ofrecería.
            let claim = std::mem::take(&mut session.claim);
            session.controlado = peers.is_empty() || claim;

            net::publish_notice(sink, format!("en la sala {room}"));

            // El servidor validó el código: ya se puede entrar.
            eprintln!("[net] el servidor validó el código: sala {room} (id {id})");

            if session.controlado {
                eprintln!("[net] esperando a que otro equipo entre en la sala {room}");
                net::publish_role(sink, Role::Controlado);

                // Al volver, los que están son los que esperaban: se les
                // ofrece a cada uno, porque cada uno tiene su canal.
                for peer in peers {
                    eprintln!("[net] le ofrezco la conexión al equipo {peer}");
                    net::send_peer(peer_tx, PeerCommand::MakeOffer { peer });
                }
            } else {
                eprintln!("[net] en la sala ya estaban: {peers:?}");
                net::publish_role(sink, Role::Controlador);
                net::publish_notice(sink, "esperando la oferta del controlado");
            }
        }
        ServerMessage::PeerJoined { peer } => {
            eprintln!("[net] entró otro equipo (id {peer}) en la sala");
            net::publish_notice(sink, "llegó el otro equipo a la sala");

            // Solo el controlado ofrece: cada controlador abre su canal
            // con él, y dos controladores nunca se conectan entre sí.
            if session.controlado {
                eprintln!("[net] le ofrezco la conexión");
                net::send_peer(peer_tx, PeerCommand::MakeOffer { peer });
            }
        }
        ServerMessage::PeerLeft { peer } => {
            eprintln!("[net] se fue el otro equipo (id {peer})");

            // Si el que se fue es el controlado, en esta sala ya no hay a
            // quién controlar: se cae el par y se espera. La sala no se
            // disuelve porque el controlado puede volver solo y ofrecer de
            // nuevo; si no vuelve, el usuario entra a otra desde el panel.
            if !session.controlado && session.controlled == Some(peer) {
                net::publish_controlled_left(sink);
                net::send_peer(peer_tx, PeerCommand::Stop);
                session.controlled = None;
                net::publish_state(sink, NetState::Conectando);
                net::publish_notice(sink, "el controlado se fue: esperando que vuelva");
                return;
            }

            // Se cae solo el par de ese equipo: los demás siguen como
            // estaban. Si vuelve, se empareja de nuevo.
            net::send_peer(peer_tx, PeerCommand::Close { peer });
            net::publish_notice(sink, "el otro equipo se fue de la sala");
        }
        ServerMessage::Relay { peer, message } => {
            handle_relay(session, peer, message, peer_tx, sink);
        }
        ServerMessage::Error { code, message } => {
            eprintln!("[net] el servidor dijo: {message} ({code})");
            net::publish_notice(sink, format!("el servidor dijo: {message} ({code})"));
        }
    }
}

/// Atiende un mensaje que otro cliente mando y el servidor reenvío.
fn handle_relay(
    session: &mut Session,
    peer: u64,
    message: Value,
    peer_tx: &Sender<PeerCommand>,
    sink: &Sink,
) {
    let message: ClientMessage = match serde_json::from_value(message) {
        Ok(message) => message,
        Err(err) => {
            net::publish_notice(
                sink,
                format!("mensaje del otro equipo que no se entiende: {err}"),
            );
            return;
        }
    };

    match message {
        ClientMessage::Offer { to, sdp } => {
            if Some(to) != session.our_id {
                return;
            }

            eprintln!("[net] llegó una oferta");
            // El que ofrece es el controlado de la sala: es a él a quien
            // se controla.
            session.controlled = Some(peer);
            net::send_peer(peer_tx, PeerCommand::AcceptOffer { peer, sdp });
        }
        ClientMessage::Answer { to, sdp } => {
            if Some(to) != session.our_id {
                return;
            }

            eprintln!("[net] llegó la respuesta");
            net::send_peer(peer_tx, PeerCommand::AcceptAnswer { peer, sdp });
        }
        ClientMessage::Ice { to, candidate } => {
            if Some(to) != session.our_id {
                return;
            }

            eprintln!("[net] llegó un candidato ICE");
            net::send_peer(peer_tx, PeerCommand::AddIce { peer, candidate });
        }
        // El controlado avisa a qué sala se fue. Solo vale si viene del
        // controlado de esta sala: el resto se ignora.
        ClientMessage::Controlled { room } => {
            if session.controlled != Some(peer) {
                return;
            }
            let Some(room) = protocol::normalize_room(&room) else {
                net::publish_notice(sink, "el controlado avisó una sala que no se entiende");
                return;
            };

            eprintln!("[net] el controlado se fue a la sala {room}: lo sigo");

            net::publish_controlled_moved(sink, room.clone());
            net::publish_state(sink, NetState::Desconectado);

            // Y se lo sigue solo: el aviso ya trajo el código, así que no
            // hay que apretar nada.
            enter(session, &room, peer_tx, sink, false);
        }
        // El join y el leave los maneja el servidor, no el par.
        ClientMessage::Join { .. } | ClientMessage::Leave => {}
    }
}

/// Manda al otro equipo lo que pidió el hilo de par.
fn send_out(session: &mut Session, out: PeerOut) {
    let Some(socket) = session.ws.as_mut() else {
        return;
    };

    let body = match out {
        PeerOut::Offer { to, sdp } => ClientMessage::Offer { to, sdp },
        PeerOut::Answer { to, sdp } => ClientMessage::Answer { to, sdp },
        PeerOut::Ice { to, candidate } => ClientMessage::Ice { to, candidate },
    };

    send_frame(socket, &Frame::new(body));
}

/// Manda un mensaje del protocolo por el WebSocket.
fn send_frame(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>, frame: &Frame<ClientMessage>) {
    let text = match serde_json::to_string(frame) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("[net] no se pudo serializar un mensaje: {err}");
            return;
        }
    };

    if let Err(err) = socket.send(tungstenite::Message::text(text)) {
        eprintln!("[net] no se pudo mandar un mensaje: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use crate::net::Event;

    /// Un sink que anota los eventos, y el registro.
    fn recording_sink() -> (Sink, Arc<Mutex<Vec<Event>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&events);
        let sink: Sink = Arc::new(move |event| {
            recorded
                .lock()
                .expect("el registro no está poisoned")
                .push(event);
        });

        (sink, events)
    }

    /// El mensaje con el que el servidor confirma la entrada a una sala.
    fn joined(room: &str, id: u64, peers: &[u64]) -> String {
        let frame = Frame::new(ServerMessage::Joined {
            room: room.to_string(),
            id,
            peers: peers.to_vec(),
        });

        serde_json::to_string(&frame).expect("el sobre se serializa")
    }

    /// Las ofertas que pidió el hilo de par, ordenadas.
    fn offers(rx: &Receiver<PeerCommand>) -> Vec<u64> {
        let mut offered = Vec::new();
        while let Ok(command) = rx.try_recv() {
            match command {
                PeerCommand::MakeOffer { peer } => offered.push(peer),
                other => panic!("no era una oferta: {other:?}"),
            }
        }
        offered.sort_unstable();
        offered
    }

    /// Una entrada normal a una sala con gente: controlador, y no ofrece
    /// nada.
    #[test]
    fn a_client_that_joins_a_room_with_people_is_a_controller() {
        let (sink, events) = recording_sink();
        let (peer_tx, peer_rx) = crossbeam_channel::unbounded();

        let mut session = Session::new();
        handle_server(&mut session, &joined("ABC234", 9, &[3, 5]), &peer_tx, &sink);

        assert!(
            !session.controlado,
            "con gente en la sala se entra como controlador"
        );
        assert!(offers(&peer_rx).is_empty(), "el controlador no ofrece");

        let recorded = events.lock().expect("el registro no está poisoned").clone();
        assert!(
            recorded
                .iter()
                .any(|event| matches!(event, Event::AssignedRole(Role::Controlador))),
            "falta el rol de controlador: {recorded:?}"
        );
    }

    /// El controlado que vuelve a su sala la reclama: no entra como
    /// controlador aunque los que esperan sigan ahí.
    #[test]
    fn the_controlado_that_comes_back_claims_its_room() {
        let (sink, events) = recording_sink();
        let (peer_tx, peer_rx) = crossbeam_channel::unbounded();

        let mut session = Session::new();
        // La vuelta: la conexión se armó con la intención de reclamar.
        session.claim = true;

        handle_server(&mut session, &joined("ABC234", 9, &[3, 5]), &peer_tx, &sink);

        assert!(
            session.controlado,
            "el que vuelve sigue siendo el controlado"
        );
        assert!(!session.claim, "la intención se consume");
        assert_eq!(
            offers(&peer_rx),
            vec![3, 5],
            "le ofrece a cada uno de los que esperaban"
        );

        let recorded = events.lock().expect("el registro no está poisoned").clone();
        assert!(
            recorded
                .iter()
                .any(|event| matches!(event, Event::AssignedRole(Role::Controlado))),
            "falta el rol de controlado: {recorded:?}"
        );
    }

    /// Solo el controlado vuelve a su sala: el controlador no tiene a qué.
    #[test]
    fn only_the_controlado_has_a_room_to_come_back_to() {
        let mut session = Session::new();
        session.room = Some("ABC234".to_string());
        assert!(session.rejoin_intent().is_none());

        session.controlado = true;
        let rejoin = session.rejoin_intent().expect("el controlado vuelve");
        assert_eq!(rejoin.room, "ABC234");
    }

    /// La espera crece y se queda en el tope.
    #[test]
    fn the_rejoin_wait_grows_and_stops_at_the_top() {
        assert_eq!(backoff(0), REJOIN_FIRST);
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(3), REJOIN_MAX);
        assert_eq!(backoff(9), REJOIN_MAX);
    }
}
