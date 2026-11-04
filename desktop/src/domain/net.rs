//! Eventos y estados de red del dominio.
//!
//! Tipos puros que describen lo que ocurre en la red sin depender de
//! WebSocket, str0m ni ICE.

use crate::domain::room::Role;

/// Estado de la conexión de red.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetState {
    /// No hay conexión con el servidor de señalización.
    Desconectado,
    /// Conectado al servidor, esperando al otro equipo.
    Conectando,
    /// El canal de datos está abierto: se puede mandar.
    Conectado,
}

/// Lo que el módulo de red le publica a la aplicación.
#[derive(Clone, Debug)]
pub enum NetEvent {
    /// Cambio de estado de conexión.
    State(NetState),
    /// Un mensaje que llegó por el canal de datos WebRTC.
    Message(String),
    /// Un aviso o error, para mostrar en la UI.
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
