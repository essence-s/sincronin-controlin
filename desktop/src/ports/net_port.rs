//! Puerto de red.
//!
//! La aplicación no sabe si hay WebSocket, str0m, WebRTC o ICE. Solo sabe
//! que puede entrar a una sala, enviar mensajes y recibir eventos de red.

use crate::domain::net::NetEvent;

/// Puerto de red.
///
/// Implementado por [`crate::adapters::net`] (señalización WebSocket +
/// par str0m).
pub trait NetPort: Send + Sync + 'static {
    /// Entrar a una sala.
    ///
    /// `None` significa entrar a la sala propia del cliente (código local).
    fn connect(&self, room: Option<String>);

    /// Salir de la sala actual.
    fn disconnect(&self);

    /// Código de la sala propia de este cliente.
    fn local_room_code(&self) -> String;

    /// Enviar un mensaje por el canal de datos WebRTC.
    fn send_data(&self, text: String);

    /// Suscribirse a eventos de red.
    ///
    /// El callback se invoca desde el hilo de señalización/par; debe ser
    /// thread-safe.
    fn on_event(&self, callback: Box<dyn Fn(NetEvent) + Send + Sync>);
}
