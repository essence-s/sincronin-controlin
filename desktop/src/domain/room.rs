//! Conceptos de sala y rol del dominio.

/// Rol del cliente en una sala.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Controlado,
    Controlador,
}

/// Identificador de par (equipo remoto en la señalización WebRTC).
pub type PeerId = u64;
