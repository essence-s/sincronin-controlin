//! Puerto de la UI.
//!
//! La aplicación no sabe si la UI es Slint, una consola o tests. Solo sabe
//! que puede mostrar el estado local o el remoto y actualizar el panel de
//! emparejamiento.

use crate::domain::media::Snapshot;
use crate::domain::messages::RemoteState;
use crate::domain::room::Role;

/// Puerto de la interfaz de usuario.
///
/// Implementado por [`crate::adapters::ui::slint`] (Slint).
pub trait UiPort: Send + Sync + 'static {
    /// Muestra la fotografía del reproductor local.
    ///
    /// `role` decide si los controles de medios quedan habilitados: el
    /// controlado nunca controla desde su propia barra.
    fn show_local_snapshot(&self, snapshot: &Snapshot, role: Option<Role>);

    /// Muestra el estado que publicó el controlado.
    ///
    /// `position_ticks` es la posición ya interpolada por el servicio.
    fn show_remote_state(&self, state: &RemoteState, position_ticks: i64);

    /// Texto de estado de la conexión.
    fn set_state_text(&self, text: String);

    /// Texto del rol asignado (anfitrión / invitado).
    fn set_role_text(&self, text: String);

    /// Código de sala de este cliente.
    fn set_room_code(&self, code: String);

    /// Código de sala del otro equipo.
    fn set_peer_room(&self, room: String);

    /// Si el canal de datos está abierto (se puede enviar).
    fn set_can_send(&self, can: bool);

    /// Agrega una línea al registro del panel de red.
    fn append_log(&self, line: String);

    /// Abre el panel de emparejamiento si está cerrado.
    fn open_panel(&self);
}
