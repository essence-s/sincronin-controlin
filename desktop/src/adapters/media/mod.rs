//! Adaptador de medios: implementa [`crate::ports::MediaPort`] sobre el módulo
//! [`crate::media`] y su backend de plataforma (GSMTC, MPRIS, stub).
//!
//! Traduce los comandos de la aplicación al canal de comandos del backend y
//! los snapshots del backend al callback del puerto.

use std::sync::{Arc, Mutex};

use crossbeam_channel::{Sender, unbounded};

use crate::ports::MediaPort;
use crate::domain::media::{Command, Snapshot};

use crate::media::Sink as MediaSink;
use crate::platform;

/// Adaptador de medios.
///
/// Envuelve el hilo de medios existente (GSMTC, MPRIS o stub) y traduce
/// sus snapshots al dominio.
pub struct MediaAdapter {
    cmd_tx: Sender<Command>,
    snapshot_cb: Arc<Mutex<Option<Box<dyn Fn(Snapshot) + Send + Sync>>>>,
}

impl MediaAdapter {
    /// Levanta el hilo de medios y devuelve el adaptador listo para usar.
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = unbounded();
        let snapshot_cb: Arc<Mutex<Option<Box<dyn Fn(Snapshot) + Send + Sync>>>> =
            Arc::new(Mutex::new(None));

        let cb = Arc::clone(&snapshot_cb);
        let sink: MediaSink = Arc::new(move |snapshot: Snapshot| {
            if let Some(callback) = cb.lock().expect("el callback no está poisoned").as_ref() {
                callback(snapshot.clone());
            }
        });

        platform::spawn(cmd_rx, sink);

        Self {
            cmd_tx,
            snapshot_cb,
        }
    }
}

impl MediaPort for MediaAdapter {
    fn send_command(&self, command: Command) {
        let _ = self.cmd_tx.send(command);
    }

    fn on_snapshot(&self, callback: Box<dyn Fn(Snapshot) + Send + Sync>) {
        *self
            .snapshot_cb
            .lock()
            .expect("el callback no está poisoned") = Some(callback);
    }
}
