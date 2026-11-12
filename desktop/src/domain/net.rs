//! Eventos y estados de red del dominio.
//!
//! Tipos puros que describen lo que ocurre en la red sin depender de
//! WebSocket, str0m ni ICE.

use std::net::{IpAddr, SocketAddr};

use crate::domain::room::Role;

/// Cómo mapea el NAT las direcciones locales.
///
/// Sale de comparar lo que ven varios STUN: si coinciden, el mapeo no
/// cambia con el destino; si no, el NAT es más restrictivo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum NatMapping {
    /// Todavía no hay dos respuestas para comparar.
    #[default]
    Unknown,
    /// Dos o más STUN vieron la misma dirección.
    Stable,
    /// Cada STUN vio una dirección distinta.
    ChangesWithDestination,
}

impl NatMapping {
    /// El texto corto para el diagnóstico del panel.
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "NAT ?",
            Self::Stable => "NAT ok",
            Self::ChangesWithDestination => "NAT var",
        }
    }
}

/// Lo que la red sabe de sí misma, para el diagnóstico del panel.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct NetworkDiagnostic {
    /// Nombre de la interfaz que se está usando.
    pub interface: Option<String>,
    /// La dirección local del socket.
    pub address: Option<IpAddr>,
    /// El tipo de mapeo del NAT.
    pub nat: NatMapping,
    /// Los candidatos locales ya anunciados.
    pub candidates: Vec<SocketAddr>,
}

impl NetworkDiagnostic {
    /// El texto compacto del panel: interfaz, NAT y candidatos.
    pub fn label(&self) -> String {
        let place = self
            .interface
            .clone()
            .or_else(|| self.address.map(|ip| ip.to_string()))
            .unwrap_or_else(|| "sin red".to_string());

        format!(
            "{place} · {} · {} cand.",
            self.nat.label(),
            self.candidates.len()
        )
    }
}

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
    /// Cuántas personas hay en la sala, contando a esta.
    ///
    /// Va por la señalización: el servidor manda a quién ve cada uno al
    /// entrar (`joined`), y a los que entran y salen después (`PeerJoined`,
    /// `PeerLeft`). Por eso el anfitrión y cada invitado llegan al mismo
    /// número.
    RoomCount(usize),
    /// El controlado se fue a otra sala, avisando a dónde.
    ControlledMoved {
        /// La sala en la que está ahora.
        room: String,
    },
    /// El controlado se fue sin decir a dónde: la sala quedó sin controlado.
    ControlledLeft,
    /// Diagnóstico de la red: interfaz, NAT y candidatos.
    Diagnostic(NetworkDiagnostic),
}
