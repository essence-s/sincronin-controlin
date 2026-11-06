//! Conceptos de sala y rol del dominio.

/// Rol del cliente en una sala.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Controlado,
    Controlador,
}

impl Role {
    /// El texto para la UI.
    ///
    /// El controlado es el anfitrión: comparte su reproducción y su sala. El
    /// controlador es el invitado, que se une a esa sala para manejarla.
    pub fn label(self) -> &'static str {
        match self {
            Self::Controlado => "Anfitrión",
            Self::Controlador => "Invitado",
        }
    }
}
