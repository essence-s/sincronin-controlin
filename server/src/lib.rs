//! El servidor de señalización.
//!
//! Empareja dos clientes y reenvía la señalización entre ellos. Nada
//! más: los mensajes de control del usuario van de un cliente al otro por
//! el datachannel de WebRTC, nunca por aca.
//!
//! El servidor no sabe nada de WebRTC. Solo mira la `versión` y el `type`
//! del mensaje para saber que hacer, y reenvía el resto en crudo.
//!
//! **TLS**: en esta versión el servidor escucha en HTTP plano. En la VPS
//! va detras de Caddy (o nginx), que termina el TLS y le pasa el
//! WebSocket por HTTP plano.
//!
//! Esta es la biblioteca; el binario (`main.rs`) solo la levanta.

pub mod rooms;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::Query;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use protocol::{Frame, ServerMessage, VERSION};

use crate::rooms::{Client, Rooms};

/// El estado compartido por todos los clientes.
pub struct State {
    /// Las salas.
    pub rooms: Rooms,
}

impl State {
    /// Un estado nuevo, sin salas.
    pub fn new() -> Self {
        Self {
            rooms: Rooms::new(),
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

/// La aplicación: un único endpoint.
pub fn app(state: Arc<State>) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .with_state(state)
}

/// Levanta el servidor en un listener.
///
/// El binario lo usa con el puerto por defecto; las pruebas lo usan con
/// un puerto efimero.
pub async fn serve(listener: TcpListener, state: Arc<State>) -> std::io::Result<()> {
    axum::serve(listener, app(state)).await
}

/// Recibe el `GET /ws?room=CODIGO` y sube la conexión a WebSocket.
async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    axum::extract::State(state): axum::extract::State<Arc<State>>,
) -> Response {
    let Some(room) = params.get("room") else {
        eprintln!("[señal] Conexión rechazada: falta el parámetro room");
        return (StatusCode::BAD_REQUEST, "falta el parametro room").into_response();
    };
    let Some(room) = protocol::normalize_room(room) else {
        eprintln!("[señal] Conexión rechazada: el código de sala no es válido");
        return (StatusCode::BAD_REQUEST, "código de sala invalido").into_response();
    };

    ws.on_upgrade(move |socket| handle(socket, room, state))
}

/// Atiende a un cliente desde que entra hasta que se va.
async fn handle(mut socket: WebSocket, room: String, state: Arc<State>) {
    // El canal por donde se le mandan los mensajes.
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    let id = state.rooms.next_id();
    let peers = state.rooms.join(&room, Client { id, tx: tx.clone() });
    eprintln!(
        "[señal] sala {room}: el cliente {id} validó el código → dentro (había {} en la sala)",
        peers.len()
    );

    // Le decimos al que entra que está en la sala, y quien más hay.
    let joined = Frame::new(ServerMessage::Joined {
        room: room.clone(),
        id,
        peers: peers.clone(),
    });
    if !send(&tx, &joined) {
        return;
    }

    // Y avisamos a los que ya estaban.
    state
        .rooms
        .broadcast(&room, id, message(&ServerMessage::PeerJoined { peer: id }));

    loop {
        tokio::select! {
            // Del cliente.
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(msg)) => {
                        if !handle_incoming(msg, &room, id, &state, &tx) {
                            break;
                        }
                    }
                    Some(Err(err)) => {
                        eprintln!("[señal] sala {room}: error del cliente {id}: {err}");
                        break;
                    }
                    None => break,
                }
            }
            // Hacia el cliente.
            Some(outgoing) = rx.recv() => {
                if socket.send(outgoing).await.is_err() {
                    break;
                }
            }
        }
    }

    // Se fue: lo sacamos de la sala y avisamos a los demas.
    let left = state.rooms.leave(&room, id);
    eprintln!(
        "[señal] sala {room}: sale {id} (quedan {} en la sala)",
        left.len()
    );
    state
        .rooms
        .broadcast(&room, id, message(&ServerMessage::PeerLeft { peer: id }));
}

/// Atiende un mensaje que viene del cliente.
///
/// Devuelve `false` si la conexión se tiene que cerrar.
fn handle_incoming(
    msg: Message,
    room: &str,
    id: u64,
    state: &Arc<State>,
    tx: &mpsc::UnboundedSender<Message>,
) -> bool {
    // Solo nos interesan los mensajes de texto.
    let Message::Text(text) = msg else {
        return true;
    };

    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        eprintln!("[señal] sala {room}: el cliente {id} mando algo que no es JSON");
        let _ = tx.send(message(&ServerMessage::Error {
            code: "bad_json".to_string(),
            message: "el mensaje no es JSON valido".to_string(),
        }));
        return true;
    };

    // La versión es lo primero: si no la entendemos, no entendemos nada.
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(versión) if versión == u64::from(VERSION) => {}
        Some(versión) => {
            eprintln!("[señal] sala {room}: el cliente {id} habla la versión {versión}");
            let _ = tx.send(message(&ServerMessage::Error {
                code: "unsupported_version".to_string(),
                message: format!("este servidor habla la versión {VERSION}"),
            }));
            return false;
        }
        None => {
            let _ = tx.send(message(&ServerMessage::Error {
                code: "missing_version".to_string(),
                message: "falta el campo versión".to_string(),
            }));
            return false;
        }
    }

    let kind = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");

    match kind {
        "join" => {
            // El join tiene que coincidir con la sala de la URL.
            let joined = value
                .get("room")
                .and_then(serde_json::Value::as_str)
                .and_then(protocol::normalize_room);

            if joined.as_deref() != Some(room) {
                eprintln!("[señal] sala {room}: el cliente {id} pide otra sala");
                let _ = tx.send(message(&ServerMessage::Error {
                    code: "room_mismatch".to_string(),
                    message: "el código de sala no coincide con el de la URL".to_string(),
                }));
                return false;
            }
            true
        }
        "leave" => {
            eprintln!("[señal] sala {room}: el cliente {id} se va");
            false
        }
        // Cualquier otra cosa se reenvía en crudo a los demas de la sala.
        // El servidor no interpreta el contenido: no necesita saber que
        // es un SDP para reenviarlo.
        _ => {
            state.rooms.broadcast(
                room,
                id,
                message(&ServerMessage::Relay {
                    peer: id,
                    message: value,
                }),
            );
            true
        }
    }
}

/// Arma el mensaje de WebSocket con un mensaje del protocolo.
fn message(body: &ServerMessage) -> Message {
    let frame = Frame::new(body.clone());

    match serde_json::to_string(&frame) {
        Ok(text) => Message::text(text),
        Err(err) => {
            // No debería pasar: los mensajes del protocolo son serializables.
            eprintln!("[señal] no se pudo serializar un mensaje: {err}");
            Message::text("{}")
        }
    }
}

/// Manda un mensaje del protocolo a un cliente.
///
/// Devuelve `false` si el cliente ya no está.
fn send(tx: &mpsc::UnboundedSender<Message>, frame: &Frame<ServerMessage>) -> bool {
    match serde_json::to_string(frame) {
        Ok(text) => tx.send(Message::text(text)).is_ok(),
        Err(err) => {
            eprintln!("[señal] no se pudo serializar un mensaje: {err}");
            false
        }
    }
}
