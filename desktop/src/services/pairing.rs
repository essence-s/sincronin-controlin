//! Caso de uso: emparejamiento de salas y roles.

use crate::ports::NetPort;

/// Servicio de emparejamiento: entrar/salir de sala y gestionar el rol.
pub struct PairingService<N: NetPort> {
    net: N,
}

impl<N: NetPort> PairingService<N> {
    /// Crea un nuevo servicio con el puerto de red inyectado.
    pub fn new(net: N) -> Self {
        Self { net }
    }

    /// Entrar a una sala. `None` = sala propia del cliente.
    pub fn connect(&self, room: Option<String>) {
        self.net.connect(room);
    }

    /// Salir de la sala actual.
    pub fn disconnect(&self) {
        self.net.disconnect();
    }
}
