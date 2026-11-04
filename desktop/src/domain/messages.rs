//! Mensajes que viajan por el canal de datos WebRTC.
//!
//! Son tipos puros: comandos del controlador y estados del controlado. No
//! dependen de Slint, str0m ni de la plataforma.

use crate::domain::media::{Command, PlaybackState, Snapshot};

/// Estado que el controlado publica por el canal de datos.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteState {
    pub has_session: bool,
    pub playback: PlaybackState,
    pub position_ticks: i64,
    pub duration_ticks: i64,
    pub playback_rate: f64,
    pub title: String,
    pub artist: String,
    /// El reproductor del controlado permite play/pausa/stop.
    pub can_toggle: bool,
    /// El reproductor del controlado permite buscar.
    pub can_seek: bool,
}

/// Lo que puede llegar por el canal de datos.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    State(RemoteState),
    Command(Command),
    Ignored,
}

/// Arma el mensaje de estado que viaja por el canal de datos.
pub fn state_message(snapshot: &Snapshot) -> String {
    let playback = match snapshot.state {
        PlaybackState::Playing => "playing",
        PlaybackState::Paused => "paused",
        PlaybackState::Stopped => "stopped",
        PlaybackState::Unknown => "unknown",
    };

    serde_json::json!({
        "type": "state",
        "has_session": snapshot.has_session,
        "playback_status": playback,
        "position_ms": snapshot.position_ticks / 10_000,
        "duration_ms": snapshot.duration_ticks / 10_000,
        "playback_rate": snapshot.playback_rate,
        "title": &snapshot.title,
        "artist": &snapshot.artist,
        "can_toggle": snapshot.can_toggle,
        "can_seek": snapshot.can_seek,
    })
    .to_string()
}

/// Lee un mensaje de estado. Devuelve `None` si no es un estado válido.
pub fn parse_state_message(text: &str) -> Option<RemoteState> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("type")?.as_str()? != "state" {
        return None;
    }

    let playback = match value.get("playback_status")?.as_str()? {
        "playing" => PlaybackState::Playing,
        "paused" => PlaybackState::Paused,
        "stopped" => PlaybackState::Stopped,
        _ => PlaybackState::Unknown,
    };

    let has_session = value.get("has_session")?.as_bool()?;

    // Un controlado de una versión anterior no manda las capacidades: si
    // hay sesión, se asume que se puede controlar.
    let can_toggle = value
        .get("can_toggle")
        .and_then(|v| v.as_bool())
        .unwrap_or(has_session);
    let can_seek = value
        .get("can_seek")
        .and_then(|v| v.as_bool())
        .unwrap_or(has_session);

    Some(RemoteState {
        has_session,
        playback,
        position_ticks: value.get("position_ms")?.as_i64()?.saturating_mul(10_000),
        duration_ticks: value.get("duration_ms")?.as_i64()?.saturating_mul(10_000),
        playback_rate: value
            .get("playback_rate")
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0),
        title: value.get("title")?.as_str()?.to_string(),
        artist: value.get("artist")?.as_str()?.to_string(),
        can_toggle,
        can_seek,
    })
}

/// Traduce un mensaje de texto del canal de datos a un comando de medios.
///
/// Solo reconoce comandos simples de un solo comando. Un mensaje que no
/// sea un comando válido devuelve `None` y se ignora.
pub fn parse_media_command(text: &str) -> Option<Command> {
    let text = text.trim();
    match text {
        "play" => Some(Command::Play),
        "pause" => Some(Command::Pause),
        "stop" => Some(Command::Stop),
        "seek_backward" => Some(Command::SeekBackward),
        "seek_forward" => Some(Command::SeekForward),
        _ if text.starts_with("seek_to ") => {
            let rest = text["seek_to ".len()..].trim();
            rest.parse::<i64>().ok().map(Command::SeekTo)
        }
        _ => None,
    }
}

/// Serializa un comando para mandarlo por el canal de datos.
///
/// `SelectSession` es local: no viaja por la red.
pub fn command_message(command: &Command) -> Option<String> {
    match command {
        Command::Play => Some("play".to_string()),
        Command::Pause => Some("pause".to_string()),
        Command::Stop => Some("stop".to_string()),
        Command::SeekBackward => Some("seek_backward".to_string()),
        Command::SeekForward => Some("seek_forward".to_string()),
        Command::SeekTo(ticks) => Some(format!("seek_to {ticks}")),
        Command::SelectSession(_) => None,
    }
}

/// Clasifica un mensaje del canal de datos.
pub fn route_message(text: &str) -> Incoming {
    if let Some(state) = parse_state_message(text) {
        Incoming::State(state)
    } else if let Some(command) = parse_media_command(text) {
        Incoming::Command(command)
    } else {
        Incoming::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_media_command_play() {
        assert_eq!(parse_media_command("play"), Some(Command::Play));
    }

    #[test]
    fn parse_media_command_ignores_invalid() {
        assert_eq!(parse_media_command("hola"), None);
        assert_eq!(parse_media_command(""), None);
    }

    #[test]
    fn parse_media_command_pause() {
        assert_eq!(parse_media_command("pause"), Some(Command::Pause));
    }

    #[test]
    fn parse_media_command_stop() {
        assert_eq!(parse_media_command("stop"), Some(Command::Stop));
    }

    #[test]
    fn parse_media_command_seek_to() {
        assert_eq!(
            parse_media_command("seek_to 125000"),
            Some(Command::SeekTo(125000))
        );
    }

    #[test]
    fn command_message_round_trips() {
        for command in [
            Command::Play,
            Command::Pause,
            Command::Stop,
            Command::SeekBackward,
            Command::SeekForward,
            Command::SeekTo(125_000),
        ] {
            let text = command_message(&command).expect("el comando viaja por la red");
            assert_eq!(parse_media_command(&text), Some(command));
        }
    }

    #[test]
    fn select_session_does_not_travel_over_the_network() {
        assert_eq!(
            command_message(&Command::SelectSession("spotify".to_string())),
            None
        );
    }

    #[test]
    fn state_message_carries_the_track_and_playback() {
        let snapshot = Snapshot {
            has_session: true,
            title: "Canción".to_string(),
            artist: "Artista".to_string(),
            state: PlaybackState::Playing,
            position_ticks: 1_250_000_000,
            duration_ticks: 36_000_000_000,
            ..Snapshot::no_session()
        };

        let text = state_message(&snapshot);
        let remote = parse_state_message(&text).expect("el estado se entiende");

        assert!(remote.has_session);
        assert_eq!(remote.title, "Canción");
        assert_eq!(remote.artist, "Artista");
        assert_eq!(remote.playback, PlaybackState::Playing);
    }

    #[test]
    fn state_message_carries_the_position_and_duration() {
        let snapshot = Snapshot {
            has_session: true,
            position_ticks: 1_250_000_000,
            duration_ticks: 36_000_000_000,
            ..Snapshot::no_session()
        };

        let text = state_message(&snapshot);
        let json: serde_json::Value = serde_json::from_str(&text).expect("es JSON");
        assert_eq!(json["position_ms"].as_i64(), Some(125_000));
        assert_eq!(json["duration_ms"].as_i64(), Some(3_600_000));

        let remote = parse_state_message(&text).expect("el estado se entiende");
        assert_eq!(remote.position_ticks, 1_250_000_000);
        assert_eq!(remote.duration_ticks, 36_000_000_000);
    }

    #[test]
    fn state_message_carries_the_control_capabilities() {
        let snapshot = Snapshot {
            can_toggle: true,
            can_seek: false,
            ..playing(0)
        };

        let text = state_message(&snapshot);
        let remote = parse_state_message(&text).expect("el estado se entiende");

        assert!(remote.can_toggle);
        assert!(!remote.can_seek);
    }

    #[test]
    fn a_state_without_capabilities_defaults_to_the_session() {
        let text = serde_json::json!({
            "type": "state",
            "has_session": true,
            "playback_status": "playing",
            "position_ms": 0,
            "duration_ms": 0,
            "playback_rate": 1.0,
            "title": "Canción",
            "artist": "Artista",
        })
        .to_string();

        let remote = parse_state_message(&text).expect("el estado se entiende");

        assert!(remote.can_toggle);
        assert!(remote.can_seek);
    }

    #[test]
    fn state_message_carries_the_playback_rate() {
        let snapshot = Snapshot {
            playback_rate: 1.25,
            ..playing(0)
        };

        let text = state_message(&snapshot);
        let remote = parse_state_message(&text).expect("el estado se entiende");

        assert_eq!(remote.playback_rate, 1.25);
    }

    #[test]
    fn state_message_is_not_parsed_as_a_command() {
        let snapshot = Snapshot {
            has_session: true,
            title: "Canción".to_string(),
            ..Snapshot::no_session()
        };
        let text = state_message(&snapshot);

        match route_message(&text) {
            Incoming::State(state) => assert_eq!(state.title, "Canción"),
            otro => panic!("el estado no se tiene que tratar como comando: {otro:?}"),
        }

        match route_message("play") {
            Incoming::Command(Command::Play) => {}
            otro => panic!("un comando se sigue tratando como comando: {otro:?}"),
        }

        assert!(matches!(route_message("hola"), Incoming::Ignored));
    }

    #[test]
    fn state_message_reports_when_there_is_no_session() {
        let text = state_message(&Snapshot::no_session());
        let remote = parse_state_message(&text).expect("el estado se entiende");

        assert!(!remote.has_session);
        assert_eq!(remote.title, "");
        assert_eq!(remote.artist, "");
        assert_eq!(remote.position_ticks, 0);
        assert_eq!(remote.duration_ticks, 0);
        assert!(!remote.can_toggle);
        assert!(!remote.can_seek);
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
}
