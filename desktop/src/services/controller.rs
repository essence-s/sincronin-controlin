//! Controlador de entrada: traduce las acciones de la UI a casos de uso.
//!
//! Es el puerto de entrada [`AppPort`] visto desde el lado de la aplicación.
//! No conoce Slint ni la plataforma: recibe acciones ya traducidas y las
//! reparte entre [`StateService`] y [`PairingService`].

use std::sync::{Arc, Mutex};

use crate::domain::media::Command;
use crate::ports::{AppPort, MediaPort, NetPort, UiPort};
use crate::services::pairing::PairingService;
use crate::services::state::StateService;

/// Implementa [`AppPort`] sobre los servicios de aplicación.
pub struct AppController<M: MediaPort, N: NetPort + Clone> {
    state: Arc<Mutex<StateService<M, N>>>,
    pairing: PairingService<N>,
    ui: Arc<dyn UiPort>,
}

impl<M: MediaPort, N: NetPort + Clone> AppController<M, N> {
    /// Crea el controlador con los servicios ya construidos.
    pub fn new(
        state: Arc<Mutex<StateService<M, N>>>,
        pairing: PairingService<N>,
        ui: Arc<dyn UiPort>,
    ) -> Self {
        Self { state, pairing, ui }
    }

    /// Ejecuta un comando de reproducción en el servicio de estado.
    fn user_command(&self, command: Command) {
        self.state
            .lock()
            .expect("el servicio de estado no está poisoned")
            .on_user_command(command);
    }
}

impl<M: MediaPort, N: NetPort + Clone> AppPort for AppController<M, N> {
    fn toggle_play_pause(&self, playing: bool) {
        let command = if playing {
            Command::Pause
        } else {
            Command::Play
        };
        self.user_command(command);
    }

    fn seek_backward(&self) {
        self.user_command(Command::SeekBackward);
    }

    fn seek_forward(&self) {
        self.user_command(Command::SeekForward);
    }

    fn seek_to(&self, seconds: f32) {
        // La UI trabaja en segundos; el backend, en ticks de 100 ns.
        let ticks = (f64::from(seconds) * 10_000_000.0) as i64;
        self.user_command(Command::SeekTo(ticks));
    }

    fn select_session(&self, id: String) {
        self.user_command(Command::SelectSession(id));
    }

    fn connect(&self, room: String) {
        let room = room.trim();

        if room.is_empty() {
            // Sin código se entra a la sala propia, que es la que muestra
            // el panel.
            let code = self.pairing.local_room_code();
            self.ui.set_room_code(code);
            self.pairing.connect(None);
        } else {
            let code = room.to_ascii_uppercase();
            self.ui.set_room_code(code.clone());
            self.pairing.connect(Some(code));
        }
    }

    fn disconnect(&self) {
        self.pairing.disconnect();

        // Al salir se vuelve a la sala propia: la vista de unión queda con un
        // código vivo, que es el que se puede volver a compartir. Es el mismo
        // camino que abrir el panel.
        let code = self.pairing.local_room_code();
        self.ui.set_room_code(code);
        self.pairing.connect(None);
    }

    fn panel_toggled(&self, open: bool) {
        if !open {
            return;
        }

        // Al abrir el panel, el cliente entra solo en su propia sala: así
        // el código que muestra está vivo. Si ya estaba en una sala, no se
        // mueve de ella.
        let in_room = self
            .state
            .lock()
            .expect("el servicio de estado no está poisoned")
            .is_in_room();

        if !in_room {
            let code = self.pairing.local_room_code();
            self.ui.set_room_code(code);
            self.pairing.connect(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Instant;

    use crate::domain::media::Snapshot;
    use crate::domain::messages::RemoteState;
    use crate::domain::net::NetEvent;
    use crate::domain::notice::Notice;
    use crate::domain::room::Role;

    #[derive(Clone, Default)]
    struct FakeMedia {
        commands: Arc<Mutex<Vec<Command>>>,
    }

    impl MediaPort for FakeMedia {
        fn send_command(&self, command: Command) {
            self.commands
                .lock()
                .expect("los comandos no están poisoned")
                .push(command);
        }

        fn on_snapshot(&self, _callback: Box<dyn Fn(Snapshot) + Send + Sync>) {}
    }

    #[derive(Clone, Default)]
    struct FakeNet {
        connects: Arc<Mutex<Vec<Option<String>>>>,
        disconnects: Arc<Mutex<usize>>,
        sent: Arc<Mutex<Vec<String>>>,
    }

    impl NetPort for FakeNet {
        fn connect(&self, room: Option<String>) {
            self.connects
                .lock()
                .expect("las conexiones no están poisoned")
                .push(room);
        }

        fn disconnect(&self) {
            *self
                .disconnects
                .lock()
                .expect("las desconexiones no están poisoned") += 1;
        }

        fn local_room_code(&self) -> String {
            "TEST01".to_string()
        }

        fn send_data(&self, text: String) {
            self.sent
                .lock()
                .expect("los mensajes no están poisoned")
                .push(text);
        }

        fn on_event(&self, _callback: Box<dyn Fn(NetEvent) + Send + Sync>) {}
    }

    #[derive(Default)]
    struct FakeUi {
        room_codes: Mutex<Vec<String>>,
    }

    impl UiPort for FakeUi {
        fn show_local_snapshot(&self, _snapshot: &Snapshot, _role: Option<Role>) {}

        fn show_remote_state(&self, _state: &RemoteState, _position_ticks: i64) {}

        fn set_state_text(&self, _text: String) {}

        fn set_net_diagnostic(&self, _text: String) {}

        fn set_role_text(&self, _text: String) {}

        fn set_room_code(&self, code: String) {
            self.room_codes
                .lock()
                .expect("los códigos no están poisoned")
                .push(code);
        }

        fn set_peer_room(&self, _room: String) {}

        fn set_can_send(&self, _can: bool) {}

        fn set_room_count(&self, _count: usize) {}

        fn show_notice(&self, _notice: Notice) {}

        fn append_log(&self, _line: String) {}

        fn open_panel(&self) {}
    }

    struct Harness {
        controller: AppController<FakeMedia, FakeNet>,
        state: Arc<Mutex<StateService<FakeMedia, FakeNet>>>,
        media: FakeMedia,
        net: FakeNet,
        ui: Arc<FakeUi>,
    }

    fn harness() -> Harness {
        let media = FakeMedia::default();
        let net = FakeNet::default();
        let ui = Arc::new(FakeUi::default());
        let state = Arc::new(Mutex::new(StateService::new(
            media.clone(),
            net.clone(),
            ui.clone(),
        )));
        let pairing = PairingService::new(net.clone());
        let controller = AppController::new(Arc::clone(&state), pairing, ui.clone());

        Harness {
            controller,
            state,
            media,
            net,
            ui,
        }
    }

    #[test]
    fn connect_with_an_empty_room_uses_the_local_code() {
        let harness = harness();

        harness.controller.connect("   ".to_string());

        assert_eq!(
            *harness
                .net
                .connects
                .lock()
                .expect("las conexiones no están poisoned"),
            vec![None]
        );
        assert_eq!(
            *harness
                .ui
                .room_codes
                .lock()
                .expect("los códigos no están poisoned"),
            vec!["TEST01"]
        );
    }

    #[test]
    fn connect_normalizes_the_room_code() {
        let harness = harness();

        harness.controller.connect("  abC123 ".to_string());

        assert_eq!(
            *harness
                .net
                .connects
                .lock()
                .expect("las conexiones no están poisoned"),
            vec![Some("ABC123".to_string())]
        );
        assert_eq!(
            *harness
                .ui
                .room_codes
                .lock()
                .expect("los códigos no están poisoned"),
            vec!["ABC123"]
        );
    }

    #[test]
    fn the_panel_connects_only_when_the_client_is_not_in_a_room() {
        let harness = harness();

        harness.controller.panel_toggled(false);
        assert!(
            harness
                .net
                .connects
                .lock()
                .expect("las conexiones no están poisoned")
                .is_empty()
        );

        harness.controller.panel_toggled(true);
        assert_eq!(
            harness
                .net
                .connects
                .lock()
                .expect("las conexiones no están poisoned")
                .len(),
            1
        );

        harness
            .state
            .lock()
            .expect("el servicio de estado no está poisoned")
            .on_net_event(
                NetEvent::State(crate::domain::net::NetState::Conectando),
                Instant::now(),
            );

        harness.controller.panel_toggled(true);
        assert_eq!(
            harness
                .net
                .connects
                .lock()
                .expect("las conexiones no están poisoned")
                .len(),
            1
        );
    }

    #[test]
    fn disconnect_reenters_the_local_room() {
        let harness = harness();

        harness.controller.disconnect();

        assert_eq!(
            *harness
                .net
                .disconnects
                .lock()
                .expect("las desconexiones no están poisoned"),
            1
        );
        assert_eq!(
            *harness
                .net
                .connects
                .lock()
                .expect("las conexiones no están poisoned"),
            vec![None]
        );
        assert_eq!(
            *harness
                .ui
                .room_codes
                .lock()
                .expect("los códigos no están poisoned"),
            vec!["TEST01"]
        );
    }

    #[test]
    fn a_controller_toggles_the_remote_player() {
        let harness = harness();

        harness
            .state
            .lock()
            .expect("el servicio de estado no está poisoned")
            .on_net_event(NetEvent::AssignedRole(Role::Controlador), Instant::now());

        harness.controller.toggle_play_pause(false);
        harness.controller.toggle_play_pause(true);

        assert_eq!(
            *harness
                .net
                .sent
                .lock()
                .expect("los mensajes no están poisoned"),
            vec!["play", "pause"]
        );
        assert!(
            harness
                .media
                .commands
                .lock()
                .expect("los comandos no están poisoned")
                .is_empty()
        );
    }

    #[test]
    fn seek_to_converts_seconds_to_ticks() {
        let harness = harness();

        harness.controller.seek_to(2.5);

        assert_eq!(
            *harness
                .media
                .commands
                .lock()
                .expect("los comandos no están poisoned"),
            vec![Command::SeekTo(25_000_000)]
        );
    }

    #[test]
    fn select_session_is_always_local() {
        let harness = harness();

        harness
            .state
            .lock()
            .expect("el servicio de estado no está poisoned")
            .on_net_event(NetEvent::AssignedRole(Role::Controlador), Instant::now());

        harness.controller.select_session("session-1".to_string());

        assert_eq!(
            *harness
                .media
                .commands
                .lock()
                .expect("los comandos no están poisoned"),
            vec![Command::SelectSession("session-1".to_string())]
        );
        assert!(
            harness
                .net
                .sent
                .lock()
                .expect("los mensajes no están poisoned")
                .is_empty()
        );
    }

    #[test]
    fn disconnect_leaves_the_room() {
        let harness = harness();

        harness.controller.disconnect();

        assert_eq!(
            *harness
                .net
                .disconnects
                .lock()
                .expect("las desconexiones no están poisoned"),
            1
        );
    }
}
