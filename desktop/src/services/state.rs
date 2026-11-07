//! Caso de uso: publicación de estado, supresión de eco e interpolación.
//!
//! Es el único lugar que decide quién manda qué por el canal de datos:
//!
//! - el **controlado** publica su estado local cuando el canal está abierto;
//! - el **controlador** nunca publica su estado: solo manda comandos;
//! - el controlado aplica los comandos remotos una sola vez;
//! - el controlador pinta el estado remoto mientras está conectado y vuelve
//!   a su estado local al desconectarse.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::domain::echo::{PendingCommand, is_echo, is_expired};
use crate::domain::interpolation::RemotePosition;
use crate::domain::media::{Command, PlaybackState, Snapshot};
use crate::domain::messages::{
    Incoming, RemoteState, command_message, route_message, state_message,
};
use crate::domain::net::{NetEvent, NetState};
use crate::domain::room::Role;
use crate::ports::{MediaPort, NetPort, UiPort};

/// Servicio de estado: publica estado local, suprime ecos, interpola remoto.
pub struct StateService<M: MediaPort, N: NetPort> {
    media: M,
    net: N,
    ui: Arc<dyn UiPort>,
    role: Option<Role>,
    net_state: NetState,
    last_snapshot: Snapshot,
    /// Mientras el controlador muestra el estado del controlado, las fotos
    /// de su propio reproductor no pisan la UI.
    remote_active: bool,
    publisher: StatePublisher,
    pending: Option<PendingCommand>,
    remote_position: Option<RemotePosition>,
}

impl<M: MediaPort, N: NetPort> StateService<M, N> {
    /// Crea un nuevo servicio con los puertos inyectados.
    pub fn new(media: M, net: N, ui: Arc<dyn UiPort>) -> Self {
        Self {
            media,
            net,
            ui,
            role: None,
            net_state: NetState::Desconectado,
            last_snapshot: Snapshot::no_session(),
            remote_active: false,
            publisher: StatePublisher::new(),
            pending: None,
            remote_position: None,
        }
    }

    /// ¿El cliente ya está en una sala? Sirve para no salir de ella al
    /// abrir el panel de emparejamiento.
    pub fn is_in_room(&self) -> bool {
        self.net_state != NetState::Desconectado
    }

    /// Llegó un evento de la red.
    pub fn on_net_event(&mut self, event: NetEvent, now: Instant) {
        match event {
            NetEvent::State(state) => self.on_net_state(state, now),
            NetEvent::AssignedRole(role) => self.on_role(role, now),
            NetEvent::RoomCount(count) => self.ui.set_room_count(count),
            NetEvent::Notice(text) => self.ui.append_log(format!("· {text}")),
            NetEvent::Message(text) => self.on_message(text, now),
            NetEvent::ControlledMoved { room } => {
                self.ui
                    .append_log(format!("· el controlado se fue a la sala {room}: entrando"));
                self.ui.set_room_code(room);
                self.ui.set_peer_room(String::new());
                self.ui.open_panel();
            }
            NetEvent::ControlledLeft => {
                self.ui
                    .append_log("· el controlado se fue: esperando que vuelva".to_string());
                self.ui.set_peer_room(String::new());
                self.ui.open_panel();
            }
        }
    }

    /// El backend de medios publicó un nuevo snapshot local.
    pub fn on_local_snapshot(&mut self, snapshot: Snapshot, now: Instant) {
        self.last_snapshot = snapshot.clone();

        if self.role == Some(Role::Controlado) && self.net_state == NetState::Conectado {
            if self
                .pending
                .as_ref()
                .is_some_and(|pending| is_echo(pending, &snapshot))
            {
                // Es el eco del comando remoto: no se publica.
                self.pending = None;
            } else {
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| is_expired(pending, now))
                {
                    self.pending = None;
                }

                if self.publisher.should_publish(&snapshot, now) {
                    self.net.send_data(state_message(&snapshot));
                }
            }
        }

        if !self.remote_active {
            self.ui.show_local_snapshot(&snapshot, self.role);
        }
    }

    /// La UI pidió ejecutar un comando de reproducción.
    ///
    /// El controlador lo manda por la red; el controlado lo aplica en su
    /// reproductor local. Elegir sesión siempre es local.
    pub fn on_user_command(&mut self, command: Command) {
        if matches!(command, Command::SelectSession(_)) {
            self.media.send_command(command);
            return;
        }

        match self.role {
            Some(Role::Controlador) => {
                if let Some(text) = command_message(&command) {
                    self.net.send_data(text);
                }
            }
            Some(Role::Controlado) | None => self.media.send_command(command),
        }
    }

    fn on_net_state(&mut self, state: NetState, now: Instant) {
        self.net_state = state;
        self.ui.set_state_text(state.label().to_string());
        self.ui.set_can_send(state == NetState::Conectado);

        if state == NetState::Conectado {
            // La red avisa "conectado" más de una vez: primero cuando ICE
            // termina y después cuando se abre el canal de datos. Se manda
            // la foto en cada aviso porque el primer envío puede llegar
            // antes de que el canal esté listo.
            if self.role == Some(Role::Controlado) {
                self.send_cached_snapshot(now);
            }
        } else if state == NetState::Desconectado {
            self.publisher = StatePublisher::new();
            self.pending = None;
            // Sin conexión no hay sala: la vista vuelve a la de unión.
            self.ui.set_room_count(0);

            // Si se cierra el canal, el estado remoto deja de ser válido:
            // se vuelve a la reproducción local.
            if self.remote_active {
                self.remote_active = false;
                self.remote_position = None;
                self.ui.show_local_snapshot(&self.last_snapshot, self.role);
            }
        }
    }

    fn on_role(&mut self, role: Role, now: Instant) {
        self.role = Some(role);
        self.ui.set_role_text(role.label().to_string());

        if role == Role::Controlado {
            self.remote_active = false;
            self.remote_position = None;

            if self.net_state == NetState::Conectado {
                self.send_cached_snapshot(now);
            }
        }
    }

    fn on_message(&mut self, text: String, now: Instant) {
        match route_message(&text) {
            Incoming::State(state) => {
                // El estado remoto solo se pinta en el controlador.
                if self.role == Some(Role::Controlador) {
                    self.remote_active = true;
                    let position_ticks = self.interpolate(&state, now);
                    self.ui.show_remote_state(&state, position_ticks);
                }
            }
            Incoming::Command(command) => {
                // El comando remoto solo lo aplica el controlado, una vez.
                if self.role == Some(Role::Controlado) {
                    self.pending = Some(PendingCommand::from_command(&command, now));
                    self.media.send_command(command);
                }
            }
            Incoming::Ignored => self.ui.append_log(format!("← {text}")),
        }
    }

    /// Manda la última foto local al abrirse el canal.
    ///
    /// En Windows el backend solo publica con eventos, así que sin esto el
    /// controlador podría no ver nada hasta el próximo cambio.
    fn send_cached_snapshot(&mut self, now: Instant) {
        self.publisher.reset(&self.last_snapshot, now);
        self.net.send_data(state_message(&self.last_snapshot));
    }

    fn interpolate(&mut self, state: &RemoteState, now: Instant) -> i64 {
        match self.remote_position.as_mut() {
            Some(position) => {
                position.update(
                    state.position_ticks,
                    state.playback_rate,
                    state.playback,
                    now,
                );
                position.position(now)
            }
            None => {
                self.remote_position = Some(RemotePosition::new(
                    state.position_ticks,
                    state.playback_rate,
                    state.playback,
                    now,
                ));
                state.position_ticks
            }
        }
    }
}

/// Filtra publicaciones de estado para no mandar posición a 10 Hz.
struct StatePublisher {
    last_observed: Option<(Snapshot, Instant)>,
    last_sent: Option<Instant>,
}

const STATE_INTERVAL: Duration = Duration::from_secs(1);
const SEEK_TOLERANCE_TICKS: i64 = 10_000_000;

impl StatePublisher {
    fn new() -> Self {
        Self {
            last_observed: None,
            last_sent: None,
        }
    }

    /// Marca una foto como enviada sin pasar por el filtro.
    ///
    /// Se usa al abrirse el canal, cuando el controlado manda la última foto
    /// cacheada: el siguiente snapshot de posición no tiene que salir de
    /// inmediato.
    fn reset(&mut self, snapshot: &Snapshot, now: Instant) {
        self.last_observed = Some((snapshot.clone(), now));
        self.last_sent = Some(now);
    }

    fn should_publish(&mut self, snapshot: &Snapshot, now: Instant) -> bool {
        let previous = self.last_observed.take();
        let (discrete, seek) = match previous {
            Some((previous, observed_at)) => (
                is_discrete_change(&previous, snapshot),
                is_seek(&previous, observed_at, snapshot, now),
            ),
            None => (true, false),
        };
        self.last_observed = Some((snapshot.clone(), now));

        let due = snapshot.state == PlaybackState::Playing
            && self
                .last_sent
                .is_none_or(|last| now.saturating_duration_since(last) >= STATE_INTERVAL);
        let publish = discrete || seek || due;

        if publish {
            self.last_sent = Some(now);
        }

        publish
    }
}

/// Campos del estado que no cambian con solo avanzar la posición.
fn is_discrete_change(previous: &Snapshot, current: &Snapshot) -> bool {
    previous.has_session != current.has_session
        || previous.title != current.title
        || previous.artist != current.artist
        || previous.state != current.state
        || previous.duration_ticks != current.duration_ticks
        || (previous.playback_rate - current.playback_rate).abs() > f64::EPSILON
}

/// Detecta un salto de posición que no explica el avance normal.
fn is_seek(previous: &Snapshot, observed_at: Instant, current: &Snapshot, now: Instant) -> bool {
    if !previous.has_session || !current.has_session {
        return false;
    }

    let elapsed = now.saturating_duration_since(observed_at);
    let advanced = if previous.state == PlaybackState::Playing {
        (elapsed.as_secs_f64() * 10_000_000.0).round() as i64
    } else {
        0
    };
    let expected = previous.position_ticks.saturating_add(advanced);

    current.position_ticks.abs_diff(expected) > SEEK_TOLERANCE_TICKS as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::messages::parse_state_message;
    use std::sync::Mutex;

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
        sent: Arc<Mutex<Vec<String>>>,
    }

    impl NetPort for FakeNet {
        fn connect(&self, _room: Option<String>) {}

        fn disconnect(&self) {}

        fn send_data(&self, text: String) {
            self.sent
                .lock()
                .expect("los mensajes no están poisoned")
                .push(text);
        }

        fn on_event(&self, _callback: Box<dyn Fn(NetEvent) + Send + Sync>) {}

        fn local_room_code(&self) -> String {
            "TEST01".to_string()
        }
    }

    #[derive(Default)]
    struct FakeUi {
        local: Mutex<Vec<(Snapshot, Option<Role>)>>,
        remote: Mutex<Vec<(RemoteState, i64)>>,
        state_texts: Mutex<Vec<String>>,
        role_texts: Mutex<Vec<String>>,
        room_codes: Mutex<Vec<String>>,
        peer_rooms: Mutex<Vec<String>>,
        can_send: Mutex<Vec<bool>>,
        room_counts: Mutex<Vec<usize>>,
        log: Mutex<Vec<String>>,
        open_panel: Mutex<usize>,
    }

    impl UiPort for FakeUi {
        fn show_local_snapshot(&self, snapshot: &Snapshot, role: Option<Role>) {
            self.local
                .lock()
                .expect("la UI no está poisoned")
                .push((snapshot.clone(), role));
        }

        fn show_remote_state(&self, state: &RemoteState, position_ticks: i64) {
            self.remote
                .lock()
                .expect("la UI no está poisoned")
                .push((state.clone(), position_ticks));
        }

        fn set_state_text(&self, text: String) {
            self.state_texts
                .lock()
                .expect("la UI no está poisoned")
                .push(text);
        }

        fn set_role_text(&self, text: String) {
            self.role_texts
                .lock()
                .expect("la UI no está poisoned")
                .push(text);
        }

        fn set_room_code(&self, code: String) {
            self.room_codes
                .lock()
                .expect("la UI no está poisoned")
                .push(code);
        }

        fn set_peer_room(&self, room: String) {
            self.peer_rooms
                .lock()
                .expect("la UI no está poisoned")
                .push(room);
        }

        fn set_can_send(&self, can: bool) {
            self.can_send
                .lock()
                .expect("la UI no está poisoned")
                .push(can);
        }

        fn set_room_count(&self, count: usize) {
            self.room_counts
                .lock()
                .expect("la UI no está poisoned")
                .push(count);
        }

        fn append_log(&self, line: String) {
            self.log.lock().expect("la UI no está poisoned").push(line);
        }

        fn open_panel(&self) {
            *self.open_panel.lock().expect("la UI no está poisoned") += 1;
        }
    }

    struct Harness {
        service: StateService<FakeMedia, FakeNet>,
        media: FakeMedia,
        net: FakeNet,
        ui: Arc<FakeUi>,
    }

    fn harness() -> Harness {
        let media = FakeMedia::default();
        let net = FakeNet::default();
        let ui = Arc::new(FakeUi::default());
        let service = StateService::new(media.clone(), net.clone(), ui.clone());
        Harness {
            service,
            media,
            net,
            ui,
        }
    }

    fn playing(position_ticks: i64) -> Snapshot {
        Snapshot {
            has_session: true,
            title: "Canción".to_string(),
            artist: "Artista".to_string(),
            state: PlaybackState::Playing,
            position_ticks,
            duration_ticks: 100_000_000,
            ..Snapshot::no_session()
        }
    }

    fn sent_messages(net: &FakeNet) -> Vec<String> {
        net.sent
            .lock()
            .expect("los mensajes no están poisoned")
            .clone()
    }

    fn media_commands(media: &FakeMedia) -> Vec<Command> {
        media
            .commands
            .lock()
            .expect("los comandos no están poisoned")
            .clone()
    }

    #[test]
    fn controlled_publishes_only_when_connected() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::AssignedRole(Role::Controlado), now);
        h.service.on_local_snapshot(playing(0), now);
        assert!(sent_messages(&h.net).is_empty());

        h.service
            .on_net_event(NetEvent::State(NetState::Conectado), now);
        assert_eq!(sent_messages(&h.net).len(), 1);
    }

    #[test]
    fn every_connected_notice_resends_the_cached_snapshot() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::AssignedRole(Role::Controlado), now);
        h.service.on_local_snapshot(playing(0), now);

        // ICE avisa "conectado" y el canal de datos avisa otra vez: el
        // primer envío puede caer antes de que el canal esté abierto.
        h.service
            .on_net_event(NetEvent::State(NetState::Conectado), now);
        h.service
            .on_net_event(NetEvent::State(NetState::Conectado), now);

        assert_eq!(sent_messages(&h.net).len(), 2);
    }

    #[test]
    fn controller_does_not_publish_local_state() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::AssignedRole(Role::Controlador), now);
        h.service
            .on_net_event(NetEvent::State(NetState::Conectado), now);
        h.service.on_local_snapshot(playing(0), now);

        assert!(sent_messages(&h.net).is_empty());
    }

    #[test]
    fn controlled_ignores_remote_state() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::AssignedRole(Role::Controlado), now);
        h.service
            .on_net_event(NetEvent::State(NetState::Conectado), now);
        h.service
            .on_net_event(NetEvent::Message(state_message(&playing(0))), now);

        assert!(
            h.ui.remote
                .lock()
                .expect("la UI no está poisoned")
                .is_empty()
        );
    }

    #[test]
    fn controller_ignores_remote_command() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::AssignedRole(Role::Controlador), now);
        h.service
            .on_net_event(NetEvent::Message("play".to_string()), now);

        assert!(media_commands(&h.media).is_empty());
        assert!(sent_messages(&h.net).is_empty());
    }

    #[test]
    fn controlled_does_not_answer_a_remote_state() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::AssignedRole(Role::Controlado), now);
        h.service
            .on_net_event(NetEvent::State(NetState::Conectado), now);
        h.net
            .sent
            .lock()
            .expect("los mensajes no están poisoned")
            .clear();

        h.service
            .on_net_event(NetEvent::Message(state_message(&playing(0))), now);

        assert!(sent_messages(&h.net).is_empty());
    }

    #[test]
    fn a_remote_command_is_applied_once() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::AssignedRole(Role::Controlado), now);
        h.service
            .on_net_event(NetEvent::Message("seek_to 125000000".to_string()), now);

        assert_eq!(media_commands(&h.media), vec![Command::SeekTo(125_000_000)]);
    }

    #[test]
    fn seek_to_echo_is_not_published() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::AssignedRole(Role::Controlado), now);
        h.service.on_local_snapshot(playing(0), now);
        h.service
            .on_net_event(NetEvent::State(NetState::Conectado), now);
        h.net
            .sent
            .lock()
            .expect("los mensajes no están poisoned")
            .clear();

        h.service
            .on_net_event(NetEvent::Message("seek_to 125000000".to_string()), now);
        h.service
            .on_local_snapshot(playing(125_000_000), now + Duration::from_millis(100));

        assert!(sent_messages(&h.net).is_empty());
        assert_eq!(media_commands(&h.media), vec![Command::SeekTo(125_000_000)]);
    }

    #[test]
    fn cached_snapshot_is_sent_when_the_channel_opens() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::AssignedRole(Role::Controlado), now);
        h.service.on_local_snapshot(playing(5_000_000), now);
        h.service
            .on_net_event(NetEvent::State(NetState::Conectado), now);

        let sent = sent_messages(&h.net);
        assert_eq!(sent.len(), 1);
        let state = parse_state_message(&sent[0]).expect("el estado se entiende");
        assert_eq!(state.title, "Canción");
        assert_eq!(state.position_ticks, 5_000_000);
    }

    #[test]
    fn disconnect_returns_to_the_local_snapshot() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::AssignedRole(Role::Controlador), now);
        h.service
            .on_net_event(NetEvent::State(NetState::Conectado), now);

        let mut local = playing(0);
        local.title = "Local".to_string();
        h.service.on_local_snapshot(local, now);

        let mut remote = playing(0);
        remote.title = "Remota".to_string();
        h.service
            .on_net_event(NetEvent::Message(state_message(&remote)), now);
        assert_eq!(h.ui.remote.lock().expect("la UI no está poisoned").len(), 1);

        h.service
            .on_net_event(NetEvent::State(NetState::Desconectado), now);

        let local = h.ui.local.lock().expect("la UI no está poisoned");
        let (last, _) = local.last().expect("se vuelve a pintar lo local");
        assert_eq!(last.title, "Local");
    }

    #[test]
    fn controller_sends_user_commands_over_the_network() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::AssignedRole(Role::Controlador), now);
        h.service.on_user_command(Command::Play);
        h.service.on_user_command(Command::SeekTo(5_000_000));

        assert_eq!(sent_messages(&h.net), vec!["play", "seek_to 5000000"]);
        assert!(media_commands(&h.media).is_empty());
    }

    #[test]
    fn controlled_executes_user_commands_locally() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::AssignedRole(Role::Controlado), now);
        h.service.on_user_command(Command::Pause);

        assert_eq!(media_commands(&h.media), vec![Command::Pause]);
        assert!(sent_messages(&h.net).is_empty());
    }

    #[test]
    fn select_session_is_always_local() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::AssignedRole(Role::Controlador), now);
        h.service
            .on_user_command(Command::SelectSession("spotify".to_string()));

        assert_eq!(
            media_commands(&h.media),
            vec![Command::SelectSession("spotify".to_string())]
        );
        assert!(sent_messages(&h.net).is_empty());
    }

    #[test]
    fn controller_receives_the_remote_control_capabilities() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::AssignedRole(Role::Controlador), now);

        let remote = Snapshot {
            can_toggle: true,
            can_seek: false,
            ..playing(0)
        };
        h.service
            .on_net_event(NetEvent::Message(state_message(&remote)), now);

        let shown = h.ui.remote.lock().expect("la UI no está poisoned");
        let (state, _) = shown.first().expect("se pintó el estado remoto");
        assert!(state.can_toggle);
        assert!(!state.can_seek);
    }

    /// El número llegó a la UI.
    #[test]
    fn the_room_count_reaches_the_ui() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::State(NetState::Conectado), now);
        h.service.on_net_event(NetEvent::RoomCount(2), now);

        assert_eq!(
            h.ui.room_counts
                .lock()
                .expect("la UI no está poisoned")
                .last()
                .copied(),
            Some(2)
        );
    }

    /// En estado Conectando el número no se borra: alguien acaba de entrar.
    #[test]
    fn a_connecting_state_keeps_the_room_count() {
        let mut h = harness();
        let now = Instant::now();

        h.service.on_net_event(NetEvent::RoomCount(2), now);
        h.service
            .on_net_event(NetEvent::State(NetState::Conectando), now);

        assert_eq!(
            h.ui.room_counts
                .lock()
                .expect("la UI no está poisoned")
                .last()
                .copied(),
            Some(2)
        );
    }

    /// Al volver a Desconectado, el número se reinicia.
    #[test]
    fn room_count_resets_when_the_connection_drops() {
        let mut h = harness();
        let now = Instant::now();

        h.service
            .on_net_event(NetEvent::State(NetState::Conectado), now);
        h.service.on_net_event(NetEvent::RoomCount(2), now);
        h.service
            .on_net_event(NetEvent::State(NetState::Desconectado), now);

        assert_eq!(
            h.ui.room_counts
                .lock()
                .expect("la UI no está poisoned")
                .last()
                .copied(),
            Some(0)
        );
    }

    #[test]
    fn controlled_moved_updates_the_room_and_opens_the_panel() {
        let mut h = harness();
        let now = Instant::now();

        h.service.on_net_event(
            NetEvent::ControlledMoved {
                room: "ABC123".to_string(),
            },
            now,
        );

        assert_eq!(
            h.ui.room_codes
                .lock()
                .expect("la UI no está poisoned")
                .as_slice(),
            ["ABC123"]
        );
        assert_eq!(*h.ui.open_panel.lock().expect("la UI no está poisoned"), 1);
    }

    #[test]
    fn position_updates_are_published_at_most_once_per_second() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();

        assert!(publisher.should_publish(&playing(0), start));
        assert!(!publisher.should_publish(&playing(1_000_000), start + Duration::from_millis(100)));
        assert!(!publisher.should_publish(&playing(9_000_000), start + Duration::from_millis(900)));
        assert!(publisher.should_publish(&playing(10_000_000), start + Duration::from_secs(1)));
    }

    #[test]
    fn discrete_changes_are_published_immediately() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();
        let base = playing(0);
        assert!(publisher.should_publish(&base, start));

        let mut changed = base.clone();
        changed.title = "Otra canción".to_string();
        assert!(publisher.should_publish(&changed, start + Duration::from_millis(100)));

        let mut changed = changed.clone();
        changed.artist = "Otro artista".to_string();
        assert!(publisher.should_publish(&changed, start + Duration::from_millis(200)));

        let mut changed = changed.clone();
        changed.state = PlaybackState::Paused;
        assert!(publisher.should_publish(&changed, start + Duration::from_millis(300)));

        let mut changed = changed.clone();
        changed.duration_ticks += 10_000_000;
        assert!(publisher.should_publish(&changed, start + Duration::from_millis(400)));

        let mut changed = changed.clone();
        changed.has_session = false;
        assert!(publisher.should_publish(&changed, start + Duration::from_millis(500)));
    }

    #[test]
    fn seeks_are_published_immediately() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();
        assert!(publisher.should_publish(&playing(0), start));

        assert!(publisher.should_publish(
            &playing(30 * 10_000_000),
            start + Duration::from_millis(100)
        ));
        assert!(
            publisher.should_publish(&playing(5 * 10_000_000), start + Duration::from_millis(200))
        );
    }

    #[test]
    fn paused_playback_does_not_publish_position_updates() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();
        let paused = Snapshot {
            has_session: true,
            state: PlaybackState::Paused,
            position_ticks: 5_000_000,
            ..Snapshot::no_session()
        };

        assert!(publisher.should_publish(&paused, start));
        assert!(!publisher.should_publish(&paused, start + Duration::from_millis(100)));
        assert!(!publisher.should_publish(&paused, start + Duration::from_secs(1)));
        assert!(!publisher.should_publish(&paused, start + Duration::from_secs(2)));
    }

    #[test]
    fn cached_snapshot_on_connect_starts_the_window() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();
        let cached = playing(0);

        publisher.reset(&cached, start);

        assert!(!publisher.should_publish(&playing(1_000_000), start + Duration::from_millis(100)));
        assert!(publisher.should_publish(&playing(10_000_000), start + Duration::from_secs(1)));
    }

    #[test]
    fn a_rate_change_counts_as_a_discrete_change() {
        let mut publisher = StatePublisher::new();
        let start = Instant::now();
        let base = Snapshot {
            playback_rate: 1.0,
            ..playing(0)
        };
        assert!(publisher.should_publish(&base, start));

        let changed = Snapshot {
            playback_rate: 2.0,
            ..playing(0)
        };
        assert!(publisher.should_publish(&changed, start + Duration::from_millis(100)));
    }
}
