//! Puerto de la UI.
//!
//! La aplicación no sabe si la UI es Slint, una consola o tests. Solo sabe
//! que puede actualizar propiedades y abrir el panel de emparejamiento.

/// Puerto de la interfaz de usuario.
///
/// Implementado por [`crate::adapters::ui::slint`] (Slint).
pub trait UiPort: Send + Sync + 'static {
    /// Título de la pista actual.
    fn set_track_title(&self, title: String);
    /// Artista de la pista actual.
    fn set_track_artist(&self, artist: String);
    /// Si se está reproduciendo.
    fn set_playing(&self, playing: bool);
    /// Posición actual en segundos.
    fn set_position_secs(&self, secs: i32);
    /// Duración total en segundos.
    fn set_duration_secs(&self, secs: i32);
    /// Texto de estado de la conexión.
    fn set_state_text(&self, text: String);
    /// Texto del rol asignado (controlado / controlador).
    fn set_role_text(&self, text: String);
    /// Código de sala de este cliente.
    fn set_room_code(&self, code: String);
    /// Código de sala del otro equipo.
    fn set_peer_room(&self, room: String);
    /// Si el canal de datos está abierto (se puede enviar).
    fn set_can_send(&self, can: bool);
    /// Agrega una línea al registro del panel de red.
    fn append_log(&self, line: String);
    /// Abre el panel de emparejamiento.
    fn open_panel(&self);
}
