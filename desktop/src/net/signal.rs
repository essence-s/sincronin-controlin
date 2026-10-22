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
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde_json::Value;
use tungstenite::WebSocket;
use tungstenite::stream::MaybeTlsStream;

use protocol::{ClientMessage, Frame, ServerMessage, VERSION};

use crate::net::{self, Command, NetState, PeerCommand, PeerOut, Sink};

/// Cuanto espera el socket a una lectura antes de volver al bucle.
///
/// Es corto a propósito: el hilo tiene que atender los comandos de la UI
/// aunque el servidor no mande nada.
const READ_TIMEOUT: Duration = Duration::from_millis(100);

/// Cuanto espera el hilo cuando no hay conexión con el servidor.
const IDLE_WAIT: Duration = Duration::from_millis(100);

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
    // La conexión con el servidor. `None` mientras no haya ninguna.
    let mut ws: Option<WebSocket<MaybeTlsStream<TcpStream>>> = None;
    // El identificador que nos dio el servidor.
    let mut our_id: Option<u64> = None;

    loop {
        // 1) Los comandos de la UI.
        while let Ok(cmd) = cmd_rx.try_recv() {
            handle_command(cmd, &mut ws, &peer_tx, &sink);
        }

        // 2) Lo que el hilo de par quiere que mandemos al otro equipo.
        while let Ok(out) = peer_out_rx.try_recv() {
            send_out(out, &mut ws);
        }

        // 3) Sin conexión: esperar a que llegue un comando.
        if ws.is_none() {
            match cmd_rx.recv_timeout(IDLE_WAIT) {
                Ok(cmd) => handle_command(cmd, &mut ws, &peer_tx, &sink),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            continue;
        }

        // 4) Leer del WebSocket.
        let action = match ws.as_mut() {
            Some(socket) => match socket.read() {
                Ok(tungstenite::Message::Text(text)) => {
                    handle_server(text.as_str(), &mut our_id, &peer_tx, &sink);
                    Action::Keep
                }
                Ok(tungstenite::Message::Close(_)) => {
                    Action::Close("el servidor cerró la conexión".to_string())
                }
                // Los pings los contesta la libreria; el resto no nos interesa.
                Ok(_) => Action::Keep,
                // El timeout del socket: no es un error, es la manera de
                // volver al principio del bucle.
                Err(tungstenite::Error::Io(err))
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    Action::Keep
                }
                Err(err) => Action::Close(format!("se cortó la conexión con el servidor: {err}")),
            },
            // Con la conexión abierta no puede pasar: se compruebo arriba.
            None => Action::Keep,
        };

        // Si la conexión se corto, se cierra y se sigue escuchando: el
        // usuario puede volver a conectarse cuando el servidor vuelva.
        if let Action::Close(reason) = action {
            net::publish_notice(&sink, reason);
            ws = None;
            net::send_peer(&peer_tx, PeerCommand::Stop);
            net::publish_state(&sink, NetState::Desconectado);
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

/// Atiende un comando de la UI.
fn handle_command(
    cmd: Command,
    ws: &mut Option<WebSocket<MaybeTlsStream<TcpStream>>>,
    peer_tx: &Sender<PeerCommand>,
    sink: &Sink,
) {
    match cmd {
        Command::Connect { room } => {
            // Si ya había una conexión, se cierra. Con protocolo, para que
            // el servidor no vea un reset.
            if let Some(socket) = ws.as_mut() {
                let _ = socket.close(None);
            }
            if ws.is_some() {
                net::send_peer(peer_tx, PeerCommand::Stop);
                *ws = None;
            }

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

            net::publish_state(sink, NetState::Conectando);

            eprintln!("[net] pido la sala {room}");

            match connect(&room) {
                Ok(mut socket) => {
                    // El primer mensaje: en que sala estamos.
                    let frame = Frame::new(ClientMessage::Join { room: room.clone() });
                    send_frame(&mut socket, &frame);
                    *ws = Some(socket);
                }
                Err(err) => {
                    net::publish_notice(
                        sink,
                        format!("no se pudo conectar con el servidor: {err}"),
                    );
                    net::publish_state(sink, NetState::Desconectado);
                }
            }
        }
        Command::Send(text) => {
            net::send_peer(peer_tx, PeerCommand::Send(text));
        }
        Command::Disconnect => {
            // Avisamos al servidor para que le diga al otro equipo.
            if let Some(socket) = ws.as_mut() {
                let frame = Frame::new(ClientMessage::Leave);
                send_frame(socket, &frame);
            }
            *ws = None;
            net::send_peer(peer_tx, PeerCommand::Stop);
            net::publish_state(sink, NetState::Desconectado);
            net::publish_notice(sink, "se cortó la conexión");
        }
    }
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
fn handle_server(text: &str, our_id: &mut Option<u64>, peer_tx: &Sender<PeerCommand>, sink: &Sink) {
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
            *our_id = Some(id);
            // El hilo de par ya puede preparar la sesión.
            net::send_peer(peer_tx, PeerCommand::Start { id });
            net::publish_notice(sink, format!("en la sala {room}"));

            // El servidor validó el código: ya se puede entrar.
            eprintln!("[net] el servidor validó el código: sala {room} (id {id})");

            // Los que ya estaban. El de identificador menor es el que
            // ofrece, así que si el suyo es mayor que el nuestro, nos
            // toca ofrecer.
            for peer in &peers {
                if id < *peer {
                    eprintln!("[net] hay {peer} en la sala: le ofrezco la conexión");
                    net::send_peer(peer_tx, PeerCommand::MakeOffer);
                }
            }

            if peers.is_empty() {
                eprintln!("[net] esperando a que otro equipo entre en la sala {room}");
            }
        }
        ServerMessage::PeerJoined { peer } => {
            let Some(id) = *our_id else {
                return;
            };

            eprintln!("[net] entró otro equipo (id {peer}) en la sala");

            // Que haya sesión: si el otro equipo se había ido, esta vez
            // se abre de cero.
            net::send_peer(peer_tx, PeerCommand::Start { id });

            if id < peer {
                eprintln!("[net] le ofrezco la conexión");
                net::send_peer(peer_tx, PeerCommand::MakeOffer);
            } else {
                eprintln!("[net] espero que ofrezca");
                net::publish_notice(sink, "llegó el otro equipo a la sala");
            }
        }
        ServerMessage::PeerLeft { peer } => {
            eprintln!("[net] se fue el otro equipo (id {peer})");
            // Se cae la sesión: si el otro equipo vuelve, se empareja de
            // nuevo.
            net::send_peer(peer_tx, PeerCommand::Stop);
            net::publish_state(sink, NetState::Conectando);
            net::publish_notice(sink, "el otro equipo se fue de la sala");
        }
        ServerMessage::Relay { message, .. } => {
            handle_relay(message, peer_tx, sink);
        }
        ServerMessage::Error { code, message } => {
            eprintln!("[net] el servidor dijo: {message} ({code})");
            net::publish_notice(sink, format!("el servidor dijo: {message} ({code})"));
        }
    }
}

/// Atiende un mensaje que otro cliente mando y el servidor reenvío.
fn handle_relay(message: Value, peer_tx: &Sender<PeerCommand>, sink: &Sink) {
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
        ClientMessage::Offer { sdp } => {
            eprintln!("[net] llegó una oferta");
            net::send_peer(peer_tx, PeerCommand::AcceptOffer(sdp));
        }
        ClientMessage::Answer { sdp } => {
            eprintln!("[net] llegó la respuesta");
            net::send_peer(peer_tx, PeerCommand::AcceptAnswer(sdp));
        }
        ClientMessage::Ice { candidate } => {
            eprintln!("[net] llegó un candidato ICE");
            net::send_peer(peer_tx, PeerCommand::AddIce(candidate));
        }
        // El join y el leave los maneja el servidor, no el par.
        ClientMessage::Join { .. } | ClientMessage::Leave => {}
    }
}

/// Manda al otro equipo lo que pidió el hilo de par.
fn send_out(out: PeerOut, ws: &mut Option<WebSocket<MaybeTlsStream<TcpStream>>>) {
    let Some(socket) = ws.as_mut() else {
        return;
    };

    let body = match out {
        PeerOut::Offer(sdp) => ClientMessage::Offer { sdp },
        PeerOut::Answer(sdp) => ClientMessage::Answer { sdp },
        PeerOut::Ice(candidate) => ClientMessage::Ice { candidate },
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
