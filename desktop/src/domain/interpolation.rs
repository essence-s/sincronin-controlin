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
