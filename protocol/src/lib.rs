//! Sobre de la señalización WebRTC.
//!
//! Lo único que se define aca es como viaja un mensaje por el WebSocket:
//! el escritorio y el servidor comparten este crate, y cada lado serializa
//! sus propios tipos (el SDP y los candidatos ICE).
//!
//! Reglas del sobre:
//!
//! - Todo mensaje lleva `versión`. El servidor rechaza con un error claro
//!   lo que no entiende.
//! - El cuerpo va *plano*, con la etiqueta `type` al mismo nivel que
//!   `versión`. Así el servidor puede distinguir un `join` de un `offer`
//!   sin entender de WebRTC.
//! - Los payloads de SDP y de candidatos ICE son `serde_json::Value`
//!   opacos: el que los produce los serializa y el que los recibe los
//!   deserializa con su propia libreria. Así el escritorio (str0m) y el
//!   cliente Android (el SDK de Google) pueden hablar sin traduccion.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Versión del protocolo de señalización.
///
/// Se incrementa cuando cambia la forma de los mensajes. Los dos lados
/// tienen que hablar la misma.
pub const VERSION: u32 = 1;

/// Alfabeto de los códigos de sala.
///
/// Sin `I`, `L`, `O` ni `U`, y sin `0` ni `1`: el código se dicta por voz
/// o se copia a mano, y no hay dos letras que suenen igual.
pub const ROOM_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";

/// Largo del código de sala.
pub const ROOM_LEN: usize = 6;

/// Normaliza un código de sala escrito a mano.
///
/// Devuelve `None` si no es un código valido: no tiene el largo correcto
/// o trae caracteres que no son del alfabeto.
pub fn normalize_room(room: &str) -> Option<String> {
    let room = room.trim().to_ascii_uppercase();

    if room.len() != ROOM_LEN {
        return None;
    }
    if !room.bytes().all(|b| ROOM_ALPHABET.contains(&b)) {
        return None;
    }

    Some(room)
}

/// Lo que un cliente le manda al servidor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientMessage {
    /// Primera mensagem: el cliente se suma a una sala.
    Join {
        /// Código de la sala.
        room: String,
    },
    /// Oferta SDP, en el JSON estandar de los navegadores.
    Offer {
        /// `{"type":"offer","sdp":"v=0..."}`.
        sdp: Value,
    },
    /// Respuesta SDP, en el JSON estandar de los navegadores.
    Answer {
        /// `{"type":"answer","sdp":"v=0..."}`.
        sdp: Value,
    },
    /// Candidato ICE, en el JSON estandar de los navegadores.
    Ice {
        /// `{"candidate":"candidate:...","sdpMid":null,...}`.
        candidate: Value,
    },
    /// El cliente se va de la sala.
    Leave,
}

/// Lo que el servidor le manda a un cliente.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    /// El servidor acepto el ingreso a la sala.
    Joined {
        /// Código de la sala.
        room: String,
        /// Identificador del cliente dentro del servidor.
        ///
        /// El que tiene el identificador menor es el que ofrece: así los
        /// dos lados saben quien manda la oferta sin que el servidor
        /// entienda de WebRTC.
        id: u64,
        /// Identificadores de los que ya estaban en la sala.
        peers: Vec<u64>,
    },
    /// Entró otro cliente a la sala.
    PeerJoined {
        /// Identificador del que entró.
        peer: u64,
    },
    /// Se fue un cliente de la sala.
    PeerLeft {
        /// Identificador del que se fue.
        peer: u64,
    },
    /// Mensaje de otro cliente, reenviado sin interpretarlo.
    Relay {
        /// Identificador del que lo mando.
        peer: u64,
        /// El mensaje tal cual lo mando el otro cliente.
        message: Value,
    },
    /// El servidor no entiende el mensaje o no pudo atenderlo.
    Error {
        /// Código del error, para programadores.
        code: String,
        /// Texto del error, para humanos.
        message: String,
    },
}

/// Un mensaje con la versión del protocolo.
///
/// El cuerpo va *plano*: el JSON final es `{"version":1,"type":"offer",...}`
/// y no `{"version":1,"body":{"type":"offer",...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame<T> {
    /// Versión del protocolo.
    pub version: u32,
    /// El cuerpo del mensaje.
    #[serde(flatten)]
    pub body: T,
}

impl<T> Frame<T> {
    /// Arma un mensaje con la versión actual del protocolo.
    pub fn new(body: T) -> Self {
        Self {
            version: VERSION,
            body,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_message_goes_flat() {
        let frame = Frame::new(ClientMessage::Join {
            room: "ABC234".to_string(),
        });
        let json = serde_json::to_string(&frame).unwrap();

        assert_eq!(json, r#"{"version":1,"type":"join","room":"ABC234"}"#);
    }

    #[test]
    fn the_client_message_round_trips() {
        let frame = Frame::new(ClientMessage::Ice {
            candidate: serde_json::json!({
                "candidate": "candidate:1 1 udp 2130706431 192.168.1.5 5000 typ host",
                "sdpMid": null,
                "sdpMLineIndex": 0,
                "usernameFragment": "abcd"
            }),
        });
        let json = serde_json::to_string(&frame).unwrap();
        let back: Frame<ClientMessage> = serde_json::from_str(&json).unwrap();

        assert_eq!(back, frame);
    }

    #[test]
    fn the_server_message_round_trips() {
        let frame = Frame::new(ServerMessage::Joined {
            room: "ABC234".to_string(),
            id: 7,
            peers: vec![3, 5],
        });
        let json = serde_json::to_string(&frame).unwrap();

        assert_eq!(
            json,
            r#"{"version":1,"type":"joined","room":"ABC234","id":7,"peers":[3,5]}"#
        );

        let back: Frame<ServerMessage> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn a_relayed_message_keeps_its_payload() {
        // El servidor reenvía el mensaje del otro cliente sin tocarlo: el
        // cliente lo vuelve a leer como ClientMessage.
        let inner = Frame::new(ClientMessage::Offer {
            sdp: serde_json::json!({"type": "offer", "sdp": "v=0\r\n"}),
        });
        let relayed = Frame::new(ServerMessage::Relay {
            peer: 2,
            message: serde_json::to_value(&inner).unwrap(),
        });
        let json = serde_json::to_string(&relayed).unwrap();

        let back: Frame<ServerMessage> = serde_json::from_str(&json).unwrap();
        let ServerMessage::Relay { peer, message } = back.body else {
            panic!("no era un relay");
        };
        assert_eq!(peer, 2);

        let inner: Frame<ClientMessage> = serde_json::from_value(message).unwrap();
        let ClientMessage::Offer { sdp } = inner.body else {
            panic!("no era una oferta");
        };
        assert_eq!(sdp["type"], "offer");
    }

    #[test]
    fn an_unknown_version_can_be_read() {
        // El versión lo valida cada lado por su cuenta: el sobre lo
        // transporta sin filtrarlo.
        let json = r#"{"version":99,"type":"leave"}"#;
        let frame: Frame<ClientMessage> = serde_json::from_str(json).unwrap();

        assert_eq!(frame.version, 99);
        assert_eq!(frame.body, ClientMessage::Leave);
    }

    #[test]
    fn a_missing_type_is_rejected() {
        let json = r#"{"version":1,"room":"ABC123"}"#;
        let frame: Result<Frame<ClientMessage>, _> = serde_json::from_str(json);

        assert!(frame.is_err());
    }

    #[test]
    fn a_room_code_is_normalized() {
        assert_eq!(normalize_room("abc234").as_deref(), Some("ABC234"));
        assert_eq!(normalize_room("  abc234 ").as_deref(), Some("ABC234"));
    }

    #[test]
    fn a_bad_room_code_is_rejected() {
        // Largo incorrecto.
        assert_eq!(normalize_room("ABC23"), None);
        assert_eq!(normalize_room("ABC2345"), None);
        // Caracteres fuera del alfabeto.
        assert_eq!(normalize_room("ABC120"), None);
        assert_eq!(normalize_room("ABC1O3"), None);
        assert_eq!(normalize_room("ABC1I3"), None);
        assert_eq!(normalize_room("ABC1L3"), None);
        assert_eq!(normalize_room("ABC1U3"), None);
        assert_eq!(normalize_room("ABC1 3"), None);
    }
}
