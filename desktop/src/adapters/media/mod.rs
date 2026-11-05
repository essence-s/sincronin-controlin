//! Adaptador de medios: implementa [`crate::ports::MediaPort`] sobre el
//! backend de plataforma (GSMTC, MPRIS, stub).
//!
//! Traduce los comandos de la aplicación al canal de comandos del backend y
//! los snapshots del backend al callback del puerto.
//!
//! Guarda la última foto: el backend puede publicar antes de que la
//! aplicación registre su callback, y en Windows no vuelve a publicar hasta
//! que ocurra un evento nuevo.

use std::sync::{Arc, Mutex};

use crossbeam_channel::{Sender, unbounded};

use crate::domain::media::{Command, Snapshot};
use crate::ports::MediaPort;

use crate::platform::{self, Sink as MediaSink};

/// Callback del puerto y última foto publicada por el backend.
#[derive(Default)]
struct State {
    callback: Option<Arc<dyn Fn(Snapshot) + Send + Sync>>,
    last_snapshot: Option<Snapshot>,
}

/// Estado interno compartido para permitir clonar el adaptador.
struct Inner {
    cmd_tx: Sender<Command>,
    state: Arc<Mutex<State>>,
}

/// Adaptador de medios.
///
/// Envuelve el hilo de medios existente (GSMTC, MPRIS o stub) y traduce
/// sus snapshots al dominio.
#[derive(Clone)]
pub struct MediaAdapter {
    inner: Arc<Inner>,
}

impl MediaAdapter {
    /// Levanta el hilo de medios y devuelve el adaptador listo para usar.
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = unbounded();
        let state = Arc::new(Mutex::new(State::default()));

        let sink_state = Arc::clone(&state);
        let sink: MediaSink = Arc::new(move |snapshot: Snapshot| {
            let mut state = sink_state
                .lock()
                .expect("el estado de medios no está poisoned");
            state.last_snapshot = Some(snapshot.clone());
            let callback = state.callback.clone();
            drop(state);

            if let Some(callback) = callback {
                callback(snapshot);
            }
        });

        platform::spawn(cmd_rx, sink);

        Self {
            inner: Arc::new(Inner { cmd_tx, state }),
        }
    }
}

impl MediaPort for MediaAdapter {
    fn send_command(&self, command: Command) {
        let _ = self.inner.cmd_tx.send(command);
    }

    fn on_snapshot(&self, callback: Box<dyn Fn(Snapshot) + Send + Sync>) {
        let callback: Arc<dyn Fn(Snapshot) + Send + Sync> = Arc::from(callback);
        let mut state = self
            .inner
            .state
            .lock()
            .expect("el estado de medios no está poisoned");

        let had_callback = state.callback.is_some();
        state.callback = Some(Arc::clone(&callback));

        // Si el backend publicó antes de que hubiera suscriptor, se le
        // entrega esa foto ahora. Se llama con el mutex tomado para que el
        // hilo de medios no entregue una foto más nueva antes que esta.
        if !had_callback {
            if let Some(snapshot) = state.last_snapshot.clone() {
                callback(snapshot);
            }
        }
    }
}
