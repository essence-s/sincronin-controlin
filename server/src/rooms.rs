//! El registro de salas del servidor.
//!
//! El servidor no sabe nada de WebRTC: solo agrupa clientes por sala y
//! les reenvía los mensajes. Todo lo que hay aca es un `HashMap` de salas
//! con la lista de clientes conectados.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::ws::Message;
use tokio::sync::mpsc;

/// Un cliente conectado a una sala.
///
/// El `tx` es por donde se le mandan los mensajes. Es un canal sin limite:
/// el servidor no espera a ningún cliente lento (si se cuelga, la
/// conexión se cae sola y el canal se descarta con el cliente).
#[derive(Clone)]
pub struct Client {
    /// Identificador del cliente, único y creciente.
    pub id: u64,
    /// Por donde se le mandan los mensajes.
    pub tx: mpsc::UnboundedSender<Message>,
}

/// Las salas y quién está en cada una.
#[derive(Default)]
pub struct Rooms {
    /// Las salas, por código.
    rooms: Mutex<HashMap<String, Vec<Client>>>,
    /// El proximo identificador de cliente.
    ///
    /// Crece para siempre: el más chico es el que llegó antes.
    next_id: AtomicU64,
}

impl Rooms {
    /// Un registro vacío.
    pub fn new() -> Self {
        Self::default()
    }

    /// Un identificador de cliente nuevo.
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Mete a un cliente en una sala.
    ///
    /// Devuelve los identificadores de los que ya estaban.
    pub fn join(&self, room: &str, client: Client) -> Vec<u64> {
        let mut rooms = self.rooms.lock().unwrap_or_else(|err| err.into_inner());
        let clients = rooms.entry(room.to_string()).or_default();

        let peers = clients.iter().map(|c| c.id).collect();
        clients.push(client);

        peers
    }

    /// Saca a un cliente de la sala.
    ///
    /// Devuelve los identificadores de los que quedan. Si la sala se
    /// queda vacía, se borra: no tiene sentido acumular salas muertas.
    pub fn leave(&self, room: &str, id: u64) -> Vec<u64> {
        let mut rooms = self.rooms.lock().unwrap_or_else(|err| err.into_inner());

        let Some(clients) = rooms.get_mut(room) else {
            return Vec::new();
        };
        clients.retain(|c| c.id != id);

        let left = clients.iter().map(|c| c.id).collect();
        if clients.is_empty() {
            rooms.remove(room);
        }

        left
    }

    /// Le manda un mensaje a todos los de la sala menos a uno.
    ///
    /// El que queda excluido es el propio emisor: no tiene que recibir
    /// otra vez lo que acaba de mandar.
    pub fn broadcast(&self, room: &str, except: u64, message: Message) {
        let rooms = self.rooms.lock().unwrap_or_else(|err| err.into_inner());

        let Some(clients) = rooms.get(room) else {
            return;
        };
        for client in clients {
            if client.id == except {
                continue;
            }
            // Si el canal está cerrado, el cliente se cae: no hay a quien
            // mandarle nada.
            let _ = client.tx.send(message.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(id: u64) -> Client {
        let (tx, _rx) = mpsc::unbounded_channel();
        Client { id, tx }
    }

    #[test]
    fn the_ids_grow() {
        let rooms = Rooms::new();

        assert_eq!(rooms.next_id(), 0);
        assert_eq!(rooms.next_id(), 1);
        assert_eq!(rooms.next_id(), 2);
    }

    #[test]
    fn joining_returns_who_was_there() {
        let rooms = Rooms::new();

        assert!(rooms.join("ABC123", client(1)).is_empty());

        let peers = rooms.join("ABC123", client(2));
        assert_eq!(peers, vec![1]);
    }

    #[test]
    fn rooms_are_independent() {
        let rooms = Rooms::new();
        rooms.join("ABC123", client(1));

        assert!(rooms.join("XYZ789", client(2)).is_empty());
    }

    #[test]
    fn leaving_gives_back_the_ones_that_stay() {
        let rooms = Rooms::new();
        rooms.join("ABC123", client(1));
        rooms.join("ABC123", client(2));

        let left = rooms.leave("ABC123", 1);
        assert_eq!(left, vec![2]);

        // La sala sigue viva.
        assert_eq!(rooms.join("ABC123", client(3)), vec![2]);
    }

    #[test]
    fn an_empty_room_is_dropped() {
        let rooms = Rooms::new();
        rooms.join("ABC123", client(1));

        assert!(rooms.leave("ABC123", 1).is_empty());

        // Si la sala se borro, el siguiente entra solo.
        assert!(rooms.join("ABC123", client(2)).is_empty());
    }
}
