//! Línea de tiempo local de un reproductor MPRIS.
//!
//! MPRIS separa dos cosas que Windows entrega juntas:
//!
//! - `Position` es una propiedad de solo lectura y **no** emite
//!   `PropertiesChanged` cuando avanza.
//! - `Seeked` avisa los cambios discontinuos y trae la posición nueva en su
//!   payload.
//!
//! El `ProgressTracker` del crate `mpris` escucha `Seeked`, pero al recibirlo
//! vuelve a leer `Position`. Firefox devuelve `0` en `Position` mientras
//! reproduce después de un seek ([bug 2065930]) y además la trunca a segundos
//! enteros ([bug 2085676]). Esa relectura pierde justo el dato bueno.
//!
//! Por eso acá se hace lo mismo que en Windows con `GSMTC`: se guarda un
//! **ancla** (posición, instante y tasa) y la posición se interpola en local
//! hasta el próximo evento. Un `Seeked` reemplaza el ancla con su payload,
//! sin releer `Position`. Cuando la posición llega por otro evento (cambio de
//! estado, de pista o de tasa), se reancla solo si el salto es plausible.
//!
//! [bug 2065930]: https://bugzilla.mozilla.org/show_bug.cgi?id=2065930
//! [bug 2085676]: https://bugzilla.mozilla.org/show_bug.cgi?id=2085676

use std::time::{Duration, Instant};

use mpris::{Metadata, PlaybackStatus, Progress};

use crate::domain::media::PlaybackState;

/// Tolerancia para decidir que una lectura de `Position` es un salto real.
///
/// Firefox trunca `Position` a segundos enteros, así que una diferencia de
/// hasta ~1 s es normal y no debe hacer que el ancla se mueva en cada evento.
const DRIFT_TOLERANCE_TICKS: i64 = 20_000_000; // 2 s

/// Ticks de 100 ns por segundo, como `f64` para interpolar con la tasa.
const TICKS_PER_SEC: f64 = 10_000_000.0;

/// Los datos que identifican una pista.
///
/// `mpris:trackid` no siempre está y, cuando está, no siempre es único:
/// Firefox publica el mismo object path para todas las pistas. Por eso también
/// se comparan título y álbum.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TrackKey {
    id: Option<String>,
    title: String,
    album: String,
}

impl TrackKey {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            id: metadata.track_id().map(String::from),
            title: metadata.title().unwrap_or_default().to_string(),
            album: metadata.album_name().unwrap_or_default().to_string(),
        }
    }

    /// Si las dos claves describen la misma pista.
    ///
    /// Se exige que coincidan el id (si ambos lo tienen) y el texto. Firefox
    /// usa un id fijo para todas las pistas, así que mirar solo el id haría
    /// que un cambio de canción se confundiera con un cambio de metadatos.
    fn same_track(&self, other: &Self) -> bool {
        let same_id = match (&self.id, &other.id) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        same_id && self.title == other.title && self.album == other.album
    }
}

/// Una lectura completa del reproductor, ya traducida al contrato de `media`.
///
/// `position_ticks` es la lectura cruda de `Position` en el instante de la
/// observación, no la posición interpolada.
#[derive(Clone, Debug)]
pub struct Observation {
    pub state: PlaybackState,
    pub position_ticks: i64,
    pub duration_ticks: i64,
    pub rate: f64,
    track: TrackKey,
}

impl Observation {
    /// Traduce el `Progress` que acaba de refrescar el `ProgressTracker`.
    pub fn from_progress(progress: &Progress) -> Self {
        let metadata = progress.metadata();
        Self {
            state: playback_state(progress.playback_status()),
            position_ticks: to_ticks(progress.initial_position()),
            duration_ticks: metadata.length().map(to_ticks).unwrap_or(0),
            rate: sane_rate(progress.playback_rate()),
            track: TrackKey::from_metadata(metadata),
        }
    }
}

/// El estado de posición que se publica en cada `Snapshot`.
#[derive(Clone, Debug)]
pub struct Timeline {
    /// Posición conocida en `anchor_at`.
    anchor_ticks: i64,
    /// Instante monotónico en el que se conoció `anchor_ticks`.
    anchor_at: Instant,
    state: PlaybackState,
    /// Velocidad de reproducción (1.0 = normal).
    rate: f64,
    /// Duración de la pista. Se conserva ante los `0` transitorios de Firefox.
    duration_ticks: i64,
    track: Option<TrackKey>,
}

impl Timeline {
    /// Arma la línea de tiempo con la primera lectura del reproductor.
    pub fn new(progress: &Progress, now: Instant) -> Self {
        Self::from_observation(Observation::from_progress(progress), now)
    }

    fn from_observation(observation: Observation, now: Instant) -> Self {
        Self {
            anchor_ticks: observation.position_ticks.max(0),
            anchor_at: now,
            state: observation.state,
            rate: observation.rate,
            duration_ticks: observation.duration_ticks.max(0),
            track: Some(observation.track),
        }
    }

    /// Aplica una lectura nueva que llegó por un evento de D-Bus.
    ///
    /// Una lectura positiva solo reemplaza al ancla si el salto supera la
    /// tolerancia: Firefox trunca `Position` a segundos enteros, así que el
    /// valor crudo casi nunca coincide con la posición interpolada. Un cero
    /// mientras la misma pista sigue en curso se ignora (es el bug de Firefox
    /// posterior a un seek); un seek real a cero llega por `Seeked`. Las
    /// excepciones son el reproductor detenido y el reinicio de una pista
    /// cerca del final (repeat one).
    ///
    /// Cuando solo cambió el estado o la tasa, se congela la posición
    /// interpolada en vez de la truncada: así pausar o reanudar no pierde la
    /// fracción de segundo.
    pub fn observe(&mut self, observation: Observation, now: Instant) {
        let predicted = self.position(now);
        let track_changed = self
            .track
            .as_ref()
            .is_none_or(|track| !track.same_track(&observation.track));
        let state_changed = self.state != observation.state;
        let rate_changed = (self.rate - observation.rate).abs() > f64::EPSILON;
        let position = observation.position_ticks.max(0);

        let raw_wins = if track_changed {
            true
        } else if position == 0 {
            // Detenerse sí es un cero real. Reproduciendo, solo si la pista
            // vuelve a empezar cerca del final; en cualquier otro punto es el
            // cero transitorio que deja Firefox después de un seek.
            observation.state == PlaybackState::Stopped
                || (observation.state == PlaybackState::Playing && self.near_end(predicted))
        } else {
            // Respaldo para reproductores que no emiten `Seeked`: un salto
            // grande en `Position` es un seek. Un salto chico es el
            // truncamiento normal y no debe mover el ancla.
            (position - predicted).abs() > DRIFT_TOLERANCE_TICKS
        };

        if raw_wins {
            self.anchor_ticks = position;
            self.anchor_at = now;
        } else if state_changed || rate_changed {
            self.anchor_ticks = predicted;
            self.anchor_at = now;
        }

        self.state = observation.state;
        self.rate = observation.rate;

        if observation.duration_ticks > 0 {
            self.duration_ticks = observation.duration_ticks;
        } else if track_changed && position <= DRIFT_TOLERANCE_TICKS {
            // Otra pista que arranca sin duración conocida: no se arrastra la
            // duración de la anterior. Si el título cambió pero la posición
            // sigue avanzando (metadatos de una radio), la duración anterior
            // puede seguir siendo válida.
            self.duration_ticks = 0;
        }
        // Si es la misma pista y `mpris:length` desapareció, se conserva la
        // última duración buena. Firefox lo hace de forma transitoria tras
        // un seek y el slider no debe colapsar a cero.

        self.track = Some(observation.track);
    }

    /// Aplica un `Seeked`: el payload es la verdad, sin releer `Position`.
    pub fn seek_to(&mut self, position_ticks: i64, now: Instant) {
        self.anchor_ticks = self.clamp_position(position_ticks);
        self.anchor_at = now;
    }

    /// Aplica un seek relativo (botones de ±10 s).
    pub fn seek_by(&mut self, delta_ticks: i64, now: Instant) {
        let target = self.position(now).saturating_add(delta_ticks);
        self.seek_to(target, now);
    }

    /// Posición interpolada en `now`.
    pub fn position(&self, now: Instant) -> i64 {
        let anchor = self.anchor_ticks.max(0);
        let position = if self.state == PlaybackState::Playing {
            let elapsed = now.saturating_duration_since(self.anchor_at);
            let advanced = (elapsed.as_secs_f64() * self.rate * TICKS_PER_SEC).round();
            anchor.saturating_add(advanced as i64)
        } else {
            anchor
        };

        self.clamp_position(position)
    }

    pub fn state(&self) -> PlaybackState {
        self.state
    }

    pub fn duration_ticks(&self) -> i64 {
        self.duration_ticks
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// Encaja una posición en el rango válido de la pista.
    pub fn clamp_position(&self, position_ticks: i64) -> i64 {
        let position = position_ticks.max(0);
        if self.duration_ticks > 0 {
            position.min(self.duration_ticks)
        } else {
            position
        }
    }

    fn near_end(&self, position: i64) -> bool {
        self.duration_ticks > 0
            && position >= self.duration_ticks.saturating_sub(DRIFT_TOLERANCE_TICKS)
    }
}

/// Traduce el estado de MPRIS al del contrato.
fn playback_state(status: PlaybackStatus) -> PlaybackState {
    match status {
        PlaybackStatus::Playing => PlaybackState::Playing,
        PlaybackStatus::Paused => PlaybackState::Paused,
        PlaybackStatus::Stopped => PlaybackState::Stopped,
    }
}

/// Una tasa inválida no debe romper la interpolación.
fn sane_rate(rate: f64) -> f64 {
    if rate.is_finite() && rate > 0.0 {
        rate
    } else {
        1.0
    }
}

/// Convierte una duración a ticks de 100 ns, la unidad del contrato `media`.
///
/// MPRIS trabaja en microsegundos y Windows en ticks de 100 ns, así que la
/// conversión es ×10.
pub fn to_ticks(duration: Duration) -> i64 {
    let ticks = duration.as_micros().saturating_mul(10);
    i64::try_from(ticks).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(
        state: PlaybackState,
        position_secs: f64,
        duration_secs: f64,
        title: &str,
    ) -> Observation {
        Observation {
            state,
            position_ticks: (position_secs * TICKS_PER_SEC) as i64,
            duration_ticks: (duration_secs * TICKS_PER_SEC) as i64,
            rate: 1.0,
            track: TrackKey {
                id: Some(format!("/track/{title}")),
                title: title.to_string(),
                album: String::new(),
            },
        }
    }

    fn secs(ticks: i64) -> f64 {
        ticks as f64 / TICKS_PER_SEC
    }

    #[test]
    fn converts_microseconds_to_ticks() {
        assert_eq!(to_ticks(Duration::from_micros(1_400_000)), 14_000_000);
        assert_eq!(to_ticks(Duration::from_secs(10)), 100_000_000);
        assert_eq!(to_ticks(Duration::ZERO), 0);
    }

    #[test]
    fn does_not_overflow_with_absurd_durations() {
        assert_eq!(to_ticks(Duration::MAX), i64::MAX);
    }

    #[test]
    fn interpolates_while_playing() {
        let now = Instant::now();
        let timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 10.0, 100.0, "a"), now);

        let later = now + Duration::from_secs(5);
        assert!((secs(timeline.position(later)) - 15.0).abs() < 0.01);
    }

    #[test]
    fn freezes_while_paused() {
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 10.0, 100.0, "a"), now);

        let paused = now + Duration::from_secs(5);
        timeline.observe(observation(PlaybackState::Paused, 15.0, 100.0, "a"), paused);

        let later = paused + Duration::from_secs(30);
        assert!((secs(timeline.position(later)) - 15.0).abs() < 0.01);
    }

    #[test]
    fn pausing_keeps_the_fraction_of_a_second() {
        // Firefox trunca Position a segundos: al pausar en 10.9 informa 10.
        // No hay que retroceder el slider a 10.
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 10.0, 100.0, "a"), now);

        let paused = now + Duration::from_millis(900);
        timeline.observe(observation(PlaybackState::Paused, 10.0, 100.0, "a"), paused);

        assert!((secs(timeline.position(paused)) - 10.9).abs() < 0.01);
        let later = paused + Duration::from_secs(10);
        assert!((secs(timeline.position(later)) - 10.9).abs() < 0.01);
    }

    #[test]
    fn resuming_continues_from_the_frozen_position() {
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 10.0, 100.0, "a"), now);

        let paused = now + Duration::from_millis(900);
        timeline.observe(observation(PlaybackState::Paused, 10.0, 100.0, "a"), paused);

        // Al reanudar, Firefox vuelve a informar el segundo truncado.
        let resumed = paused + Duration::from_secs(2);
        timeline.observe(
            observation(PlaybackState::Playing, 10.0, 100.0, "a"),
            resumed,
        );

        let later = resumed + Duration::from_secs(1);
        assert!((secs(timeline.position(later)) - 11.9).abs() < 0.01);
    }

    #[test]
    fn a_paused_seek_without_seeked_still_reanchors() {
        // Respaldo: si el reproductor no emite Seeked, un salto grande en
        // Position mientras está pausado sigue siendo un seek.
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Paused, 10.0, 100.0, "a"), now);

        let later = now + Duration::from_secs(1);
        timeline.observe(observation(PlaybackState::Paused, 30.0, 100.0, "a"), later);

        assert!((secs(timeline.position(later)) - 30.0).abs() < 0.01);
    }

    #[test]
    fn seeked_has_priority_over_stale_position() {
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 10.0, 100.0, "a"), now);

        // Firefox emite Seeked(30 s), pero Position sigue devolviendo 0.
        let at_seek = now + Duration::from_secs(2);
        timeline.seek_to((30.0 * TICKS_PER_SEC) as i64, at_seek);
        timeline.observe(
            observation(PlaybackState::Playing, 0.0, 100.0, "a"),
            at_seek,
        );

        assert!((secs(timeline.position(at_seek)) - 30.0).abs() < 0.01);

        // Y sigue avanzando desde ahí, sin volver a cero.
        let later = at_seek + Duration::from_secs(1);
        assert!((secs(timeline.position(later)) - 31.0).abs() < 0.01);
    }

    #[test]
    fn ignores_the_zero_of_a_seek_while_playing() {
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 40.0, 100.0, "a"), now);

        let later = now + Duration::from_secs(1);
        timeline.observe(observation(PlaybackState::Playing, 0.0, 100.0, "a"), later);

        assert!((secs(timeline.position(later)) - 41.0).abs() < 0.01);
    }

    #[test]
    fn accepts_zero_when_the_track_changes() {
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 40.0, 100.0, "a"), now);

        let later = now + Duration::from_secs(1);
        timeline.observe(observation(PlaybackState::Playing, 0.0, 100.0, "b"), later);

        assert!((secs(timeline.position(later)) - 0.0).abs() < 0.01);
    }

    #[test]
    fn accepts_zero_when_only_the_title_changes() {
        // Firefox usa el mismo mpris:trackid para todas las pistas. El cambio
        // de canción se detecta por el título.
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 40.0, 100.0, "a"), now);

        let later = now + Duration::from_secs(1);
        let mut next = observation(PlaybackState::Playing, 0.0, 100.0, "b");
        next.track.id = Some("/track/a".to_string());
        timeline.observe(next, later);

        assert!((secs(timeline.position(later)) - 0.0).abs() < 0.01);
    }

    #[test]
    fn a_title_change_mid_track_uses_the_real_position() {
        // Metadatos de una radio: cambia el título pero la posición sigue.
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 40.0, 100.0, "a"), now);

        let later = now + Duration::from_secs(1);
        let mut next = observation(PlaybackState::Playing, 41.0, 100.0, "b");
        next.track.id = Some("/track/a".to_string());
        timeline.observe(next, later);

        assert!((secs(timeline.position(later)) - 41.0).abs() < 0.01);
    }

    #[test]
    fn reconciles_a_seek_from_a_player_without_seeked() {
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 10.0, 100.0, "a"), now);

        // Sin señal Seeked, pero un PropertiesChanged trae Position=30 s.
        let later = now + Duration::from_secs(1);
        timeline.observe(observation(PlaybackState::Playing, 30.0, 100.0, "a"), later);

        assert!((secs(timeline.position(later)) - 30.0).abs() < 0.01);
    }

    #[test]
    fn a_truncated_position_does_not_move_the_anchor() {
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 10.0, 100.0, "a"), now);

        // Firefox trunca 10.9 s a 10 s. No es un salto.
        let later = now + Duration::from_millis(900);
        timeline.observe(observation(PlaybackState::Playing, 10.0, 100.0, "a"), later);

        assert!((secs(timeline.position(later)) - 10.9).abs() < 0.01);
    }

    #[test]
    fn keeps_the_duration_when_metadata_drops_it() {
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 30.0, 100.0, "a"), now);

        let later = now + Duration::from_secs(1);
        timeline.observe(observation(PlaybackState::Playing, 31.0, 0.0, "a"), later);

        assert_eq!(timeline.duration_ticks(), (100.0 * TICKS_PER_SEC) as i64);
    }

    #[test]
    fn resets_the_duration_when_the_track_changes() {
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 30.0, 100.0, "a"), now);

        let later = now + Duration::from_secs(1);
        timeline.observe(observation(PlaybackState::Playing, 0.0, 0.0, "b"), later);

        assert_eq!(timeline.duration_ticks(), 0);
    }

    #[test]
    fn respects_the_playback_rate() {
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 10.0, 100.0, "a"), now);

        let mut faster = observation(PlaybackState::Playing, 10.0, 100.0, "a");
        faster.rate = 2.0;
        timeline.observe(faster, now);

        let later = now + Duration::from_secs(5);
        assert!((secs(timeline.position(later)) - 20.0).abs() < 0.01);
    }

    #[test]
    fn clamps_to_the_track_duration() {
        let now = Instant::now();
        let timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 9.0, 10.0, "a"), now);

        let later = now + Duration::from_secs(5);
        assert!((secs(timeline.position(later)) - 10.0).abs() < 0.01);
    }

    #[test]
    fn seeks_by_a_delta() {
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 10.0, 100.0, "a"), now);

        timeline.seek_by((10.0 * TICKS_PER_SEC) as i64, now);
        assert!((secs(timeline.position(now)) - 20.0).abs() < 0.01);

        timeline.seek_by((-30.0 * TICKS_PER_SEC) as i64, now);
        assert!((secs(timeline.position(now)) - 0.0).abs() < 0.01);
    }

    #[test]
    fn accepts_a_loop_restart_near_the_end() {
        let now = Instant::now();
        let mut timeline =
            Timeline::from_observation(observation(PlaybackState::Playing, 99.0, 100.0, "a"), now);

        let later = now + Duration::from_secs(1);
        timeline.observe(observation(PlaybackState::Playing, 0.0, 100.0, "a"), later);

        assert!((secs(timeline.position(later)) - 0.0).abs() < 0.01);
    }
}
