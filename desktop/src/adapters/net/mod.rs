//! Adaptador de red: implementa [`crate::ports::NetPort`] sobre el módulo
//! [`crate::net`] (señalización WebSocket + par str0m).
//!
//! Traduce los eventos de `crate::net::Event` a `crate::domain::net::NetEvent`
//! y expone el puerto que consumen los servicios de aplicación.

use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::ports::NetPort;
use crate::domain::net::NetEvent;
use crate::domain::room::Role as DomainRole;

use crate::net::{self, Command, Event as NetRawEvent, NetState as RawNetState, Role as RawRole};

/// Adaptador de red.
///
/// Envuelve los hilos de señalización y par existentes, y traduce sus
/// eventos al dominio.
pub struct NetAdapter {
    cmd_tx: Sender<Command>,
    events: Arc<Mutex<Vec<NetEvent>>>,
    rx: Receiver<NetRawEvent>,
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
            cmd_tx,
            events: Arc::new(Mutex::new(Vec::new())),
            rx: sink_rx,
        }
    }

    /// Drena los eventos crudos y los traduce al dominio, invocando el
    /// callback para cada uno.
    fn pump_events(&self, callback: &dyn Fn(NetEvent)) {
        while let Ok(event) = self.rx.try_recv() {
            let domain_event = translate(event);
            self.events
                .lock()
                .expect("el registro de eventos no está poisoned")
                .push(domain_event.clone());
            callback(domain_event);
        }
    }
}

impl NetPort for NetAdapter {
    fn connect(&self, room: Option<String>) {
        let code = room.unwrap_or_else(|| net::local_room_code().to_string());
        let _ = self.cmd_tx.send(Command::Connect { room: code });
    }

    fn disconnect(&self) {
        let _ = self.cmd_tx.send(Command::Disconnect);
    }

    fn send_data(&self, text: String) {
        let _ = self.cmd_tx.send(net::Command::Send(text));
    }

    fn on_event(&self, callback: Box<dyn Fn(NetEvent) + Send + Sync>) {
        let callback = Arc::new(callback);
        let events = Arc::clone(&self.events);
        let rx = self.rx.clone();

        std::thread::Builder::new()
            .name("net-adapter".to_string())
            .spawn(move || {
                // Reenviar eventos acumulados antes de suscribirse.
                {
                    let accumulated = events
                        .lock()
                        .expect("el registro de eventos no está poisoned")
                        .clone();
                    for e in accumulated {
                        callback(e);
                    }
                }

                loop {
                    match rx.recv() {
                        Ok(event) => callback(translate(event)),
                        Err(_) => break,
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
        assert!(
            matches!(
                translate(event),
                NetEvent::State(crate::domain::net::NetState::Desconectado)
            )
        );
    }

    #[test]
    fn translate_assigned_role_controlador() {
        let event = NetRawEvent::AssignedRole(RawRole::Controlador);
        assert!(
            matches!(
                translate(event),
                NetEvent::AssignedRole(DomainRole::Controlador)
            )
        );
    }
}
