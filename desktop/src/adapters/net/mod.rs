//! Adaptador de red: implementa [`crate::ports::NetPort`] sobre el backend
//! de [`backend`] (señalización WebSocket + par str0m).
//!
//! Traduce los eventos de `backend::Event` a `crate::domain::net::NetEvent` y
//! expone el puerto que consumen los servicios de aplicación.

mod backend;

use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::domain::net::NetEvent;
use crate::domain::room::Role as DomainRole;
use crate::ports::NetPort;

use self::backend as net;
use self::backend::{Command, Event as NetRawEvent, NetState as RawNetState, Role as RawRole};

/// Suscriptor y estado del hilo puente.
///
/// El puerto tiene un solo suscriptor: si se registra otro, reemplaza al
/// anterior en vez de repartir los eventos entre dos hilos.
#[derive(Default)]
struct Subscriber {
    callback: Option<Arc<dyn Fn(NetEvent) + Send + Sync>>,
    bridge_started: bool,
}

/// Estado interno compartido para permitir clonar el adaptador.
struct Inner {
    cmd_tx: Sender<Command>,
    rx: Receiver<NetRawEvent>,
    subscriber: Arc<Mutex<Subscriber>>,
}

/// Adaptador de red.
///
/// Envuelve los hilos de señalización y par existentes, y traduce sus
/// eventos al dominio.
#[derive(Clone)]
pub struct NetAdapter {
    inner: Arc<Inner>,
}

impl NetAdapter {
    /// Levanta los hilos de red y devuelve el adaptador listo para usar.
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = unbounded();
        let (sink_tx, sink_rx) = unbounded();
        let sink: net::Sink = Arc::new(move |event: NetRawEvent| {
            let _ = sink_tx.send(event);
        });
        net::spawn(cmd_rx, sink);

        Self {
            inner: Arc::new(Inner {
                cmd_tx,
                rx: sink_rx,
                subscriber: Arc::new(Mutex::new(Subscriber::default())),
            }),
        }
    }
}

impl NetPort for NetAdapter {
    fn connect(&self, room: Option<String>) {
        let code = room.unwrap_or_else(|| net::local_room_code().to_string());
        let _ = self.inner.cmd_tx.send(Command::Connect { room: code });
    }

    fn disconnect(&self) {
        let _ = self.inner.cmd_tx.send(Command::Disconnect);
    }

    fn local_room_code(&self) -> String {
        net::local_room_code().to_string()
    }

    fn send_data(&self, text: String) {
        let _ = self.inner.cmd_tx.send(net::Command::Send(text));
    }

    fn on_event(&self, callback: Box<dyn Fn(NetEvent) + Send + Sync>) {
        let callback: Arc<dyn Fn(NetEvent) + Send + Sync> = Arc::from(callback);
        let mut subscriber = self
            .inner
            .subscriber
            .lock()
            .expect("el suscriptor de red no está poisoned");
        subscriber.callback = Some(callback);

        if subscriber.bridge_started {
            return;
        }
        subscriber.bridge_started = true;
        drop(subscriber);

        let rx = self.inner.rx.clone();
        let subscriber = Arc::clone(&self.inner.subscriber);

        std::thread::Builder::new()
            .name("net-adapter".to_string())
            .spawn(move || {
                while let Ok(event) = rx.recv() {
                    let callback = subscriber
                        .lock()
                        .expect("el suscriptor de red no está poisoned")
                        .callback
                        .clone();

                    if let Some(callback) = callback {
                        callback(translate(event));
                    }
                }
            })
            .expect("no se pudo crear el hilo puente de red");
    }
}

/// Traduce un evento crudo de red al evento de dominio.
fn translate(event: NetRawEvent) -> NetEvent {
    match event {
        NetRawEvent::State(state) => NetEvent::State(match state {
            RawNetState::Desconectado => crate::domain::net::NetState::Desconectado,
            RawNetState::Conectando => crate::domain::net::NetState::Conectando,
            RawNetState::Conectado => crate::domain::net::NetState::Conectado,
        }),
        NetRawEvent::Message(text) => NetEvent::Message(text),
        NetRawEvent::Notice(text) => NetEvent::Notice(text),
        NetRawEvent::AssignedRole(role) => NetEvent::AssignedRole(match role {
            RawRole::Controlado => DomainRole::Controlado,
            RawRole::Controlador => DomainRole::Controlador,
        }),
        NetRawEvent::RoomCount(count) => NetEvent::RoomCount(count),
        NetRawEvent::ControlledMoved { room } => NetEvent::ControlledMoved { room },
        NetRawEvent::ControlledLeft => NetEvent::ControlledLeft,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_state_desconectado() {
        let event = NetRawEvent::State(RawNetState::Desconectado);
        assert!(matches!(
            translate(event),
            NetEvent::State(crate::domain::net::NetState::Desconectado)
        ));
    }

    #[test]
    fn room_count_is_translated() {
        assert!(matches!(
            translate(NetRawEvent::RoomCount(2)),
            NetEvent::RoomCount(2)
        ));
    }

    #[test]
    fn translate_assigned_role_controlador() {
        let event = NetRawEvent::AssignedRole(RawRole::Controlador);
        assert!(matches!(
            translate(event),
            NetEvent::AssignedRole(DomainRole::Controlador)
        ));
    }
}
