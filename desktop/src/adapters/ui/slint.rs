//! Adaptador de UI sobre Slint.
//!
//! Implementa [`crate::ports::UiPort`] y conecta los callbacks de [`AppWindow`]
//! a los servicios de la capa de aplicación.
//!
//! En esta fase se conectan emparejamiento y medios. La publicación de estado
//! (StateService) se conecta en la siguiente fase.

use slint::ComponentHandle;

use crate::adapters::media::MediaAdapter;
use crate::adapters::net::NetAdapter;
use crate::ports::{NetPort, UiPort};
use crate::domain::media::Command as MediaCommand;
use crate::services::media::MediaService;
use crate::services::pairing::PairingService;

/// Adaptador de UI.
///
/// Implementa `UiPort` y conecta los callbacks de Slint a los servicios.
pub struct SlintAdapter {
    ui: slint::Weak<crate::AppWindow>,
    pairing: PairingService<NetAdapter>,
    media: MediaService<MediaAdapter>,
}

impl SlintAdapter {
    /// Crea el adaptador y conecta todos los callbacks de Slint.
    pub fn new(
        ui: &crate::AppWindow,
        net: NetAdapter,
        media: MediaAdapter,
    ) -> Self {
        let ui_weak = ui.as_weak();
        let pairing = PairingService::new(net.clone());
        let media_service = MediaService::new(media.clone());

        // toggle-play-pause
        {
            let weak = ui_weak.clone();
            let media = media_service.clone();
            ui.on_toggle_play_pause(move || {
                let playing = weak.upgrade().map(|u| u.get_playing()).unwrap_or(false);
                let cmd = if playing { MediaCommand::Pause } else { MediaCommand::Play };
                media.execute(cmd);
            });
        }

        // seek-backward
        {
            let media = media_service.clone();
            ui.on_seek_backward(move || {
                media.execute(MediaCommand::SeekBackward);
            });
        }

        // seek-forward
        {
            let media = media_service.clone();
            ui.on_seek_forward(move || {
                media.execute(MediaCommand::SeekForward);
            });
        }

        // seek-to
        {
            let media = media_service.clone();
            ui.on_seek_to(move |secs: f32| {
                let ticks = (secs as f64 * 10_000_000.0) as i64;
                media.execute(MediaCommand::SeekTo(ticks));
            });
        }

        // select-session
        {
            let media = media_service.clone();
            ui.on_select_session(move |id: slint::SharedString| {
                media.select_session(id.to_string());
            });
        }

        // connect
        {
            let weak = ui_weak.clone();
            let pairing = pairing.clone();
            ui.on_connect(move |room: slint::SharedString| {
                let room = room.trim();
                let room = if room.is_empty() {
                    None
                } else {
                    Some(room.to_ascii_uppercase())
                };
                if let Some(handle) = weak.upgrade() {
                    if let Some(ref code) = room {
                        handle.set_room_code(code.as_str().into());
                    }
                }
                pairing.connect(room.map(|r| r));
            });
        }

        // disconnect
        {
            let pairing = pairing.clone();
            ui.on_disconnect(move || {
                pairing.disconnect();
            });
        }

        // send-message
        {
            let net = net.clone();
            ui.on_send_message(move || {
                let _ = net.send_data("hola".to_string());
            });
        }

        // net-toggled (abrir/cerrar panel)
        {
            let weak = ui_weak.clone();
            let net = net.clone();
            ui.on_net_toggled(move || {
                let Some(handle) = weak.upgrade() else {
                    return;
                };
                weak.upgrade_in_event_loop(move |ui| {
                    crate::platform::configure(&ui);
                });

                if handle.get_net_open() {
                    // Al abrir el panel, entrar a la sala propia.
                    net.connect(None);
                }
            });
        }

        // quit
        {
            ui.on_quit(move || {
                let _ = slint::quit_event_loop();
            });
        }

        Self {
            ui: ui_weak,
            pairing,
            media: media_service,
        }
    }
}

impl UiPort for SlintAdapter {
    fn set_track_title(&self, title: String) {
        if let Some(handle) = self.ui.upgrade() {
            handle.set_track_title(title.into());
        }
    }

    fn set_track_artist(&self, artist: String) {
        if let Some(handle) = self.ui.upgrade() {
            handle.set_track_artist(artist.into());
        }
    }

    fn set_has_session(&self, has_session: bool) {
        if let Some(handle) = self.ui.upgrade() {
            handle.set_has_session(has_session);
        }
    }

    fn set_playing(&self, playing: bool) {
        if let Some(handle) = self.ui.upgrade() {
            handle.set_playing(playing);
        }
    }

    fn set_position_secs(&self, secs: i32) {
        if let Some(handle) = self.ui.upgrade() {
            handle.set_position_secs(secs);
        }
    }

    fn set_duration_secs(&self, secs: i32) {
        if let Some(handle) = self.ui.upgrade() {
            handle.set_duration_secs(secs);
        }
    }

    fn set_state_text(&self, text: String) {
        if let Some(handle) = self.ui.upgrade() {
            handle.set_state_text(text.into());
        }
    }

    fn set_role_text(&self, text: String) {
        if let Some(handle) = self.ui.upgrade() {
            handle.set_role_text(text.into());
        }
    }

    fn set_room_code(&self, code: String) {
        if let Some(handle) = self.ui.upgrade() {
            handle.set_room_code(code.into());
        }
    }

    fn set_peer_room(&self, room: String) {
        if let Some(handle) = self.ui.upgrade() {
            handle.set_peer_room(room.into());
        }
    }

    fn set_can_send(&self, can: bool) {
        if let Some(handle) = self.ui.upgrade() {
            handle.set_can_send(can);
        }
    }

    fn append_log(&self, line: String) {
        let Some(handle) = self.ui.upgrade() else {
            return;
        };
        let mut lines: Vec<String> = handle.get_log_text().lines().map(str::to_string).collect();
        lines.push(line);
        if lines.len() > 40 {
            lines.drain(..lines.len() - 40);
        }
        handle.set_log_text(lines.join("\n").into());
    }

    fn open_panel(&self) {
        if let Some(handle) = self.ui.upgrade() {
            handle.set_net_open(true);
            self.ui.upgrade_in_event_loop(move |ui| {
                crate::platform::configure(&ui);
            });
        }
    }
}
