//! Conceptos de sala y rol del dominio.

/// Rol del cliente en una sala.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Controlado,
    Controlador,
}

impl Role {
    /// El texto para la UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Controlado => "controlado",
            Self::Controlador => "controlador",
        }
    }
}
