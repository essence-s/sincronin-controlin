//! Interpolación de posición remota para la barra de progreso del controlador.

use std::time::Instant;

use crate::domain::media::PlaybackState;

/// Posición interpolada del controlado, persistente entre mensajes de red.
pub struct RemotePosition {
    position_ticks: i64,
    playback_rate: f64,
    state: PlaybackState,
    anchor_at: Instant,
}

impl RemotePosition {
    pub fn new(
        position_ticks: i64,
        playback_rate: f64,
        state: PlaybackState,
        now: Instant,
    ) -> Self {
        Self {
            position_ticks,
            playback_rate,
            state,
            anchor_at: now,
        }
    }

    pub fn update(
        &mut self,
        position_ticks: i64,
        playback_rate: f64,
        state: PlaybackState,
        now: Instant,
    ) {
        self.position_ticks = position_ticks;
        self.playback_rate = playback_rate;
        self.state = state;
        self.anchor_at = now;
    }

    /// Posición interpolada en `now`, en ticks.
    pub fn position(&self, now: Instant) -> i64 {
        let anchor = self.position_ticks.max(0);
        if self.state == PlaybackState::Playing {
            let elapsed = now.saturating_duration_since(self.anchor_at);
            let advanced =
                (elapsed.as_secs_f64() * self.playback_rate * 10_000_000.0).round() as i64;
            anchor.saturating_add(advanced)
        } else {
            anchor
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn remote_position_advances_with_the_playback_rate() {
        let start = Instant::now();
        let position = RemotePosition::new(0, 1.5, PlaybackState::Playing, start);

        let advanced = position.position(start + Duration::from_secs(2));
        assert_eq!(advanced, 3 * 10_000_000);
    }

    #[test]
    fn remote_position_does_not_advance_when_not_playing() {
        let start = Instant::now();
        let position = RemotePosition::new(5_000_000, 1.0, PlaybackState::Paused, start);

        assert_eq!(position.position(start + Duration::from_secs(2)), 5_000_000);
    }

    #[test]
    fn a_new_remote_state_reanchors_the_position() {
        let start = Instant::now();
        let mut position = RemotePosition::new(0, 1.0, PlaybackState::Playing, start);
        position.update(
            30_000_000,
            1.0,
            PlaybackState::Playing,
            start + Duration::from_millis(100),
        );

        // Justo después del reanclaje, la posición es el valor anclado.
        assert_eq!(
            position.position(start + Duration::from_millis(100)),
            30_000_000
        );
        // Y sigue interpolando desde ese ancla con la tasa nueva.
        assert_eq!(
            position.position(start + Duration::from_millis(200)),
            31_000_000
        );
    }
}
