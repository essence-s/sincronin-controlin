//! Puerto de entrada de la aplicación.
//!
//! Es la API que consumen los adaptadores de entrada (hoy, Slint). El
//! controlador de [`crate::services::controller`] la implementa y decide qué
//! caso de uso ejecutar. Así el adaptador de UI no conoce los servicios.

/// Acciones que la UI puede disparar.
pub trait AppPort: Send + Sync + 'static {
    /// Alterna reproducción y pausa.
    ///
    /// `playing` es el estado que la UI está mostrando: si está
    /// reproduciendo, la acción es pausar.
    fn toggle_play_pause(&self, playing: bool);

    /// Retrocede diez segundos.
    fn seek_backward(&self);

    /// Avanza diez segundos.
    fn seek_forward(&self);

    /// Busca una posición absoluta, en segundos.
    fn seek_to(&self, seconds: f32);

    /// Elige qué sesión de medios controlar.
    fn select_session(&self, id: String);

    /// Entra a una sala. Un texto vacío significa la sala propia.
    fn connect(&self, room: String);

    /// Sale de la sala actual.
    fn disconnect(&self);

    /// Manda un mensaje de prueba por el canal de datos.
    fn send_message(&self, text: String);

    /// El panel de emparejamiento se abrió (`true`) o se cerró (`false`).
    fn panel_toggled(&self, open: bool);
}
