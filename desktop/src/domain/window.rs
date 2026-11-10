//! Modo de la ventana según la inactividad del mouse.

use std::time::{Duration, Instant};

/// Lo que tiene que pasar sin que el mouse pase por la ventana para que se
/// achique al círculo.
pub const IDLE_COLLAPSE: Duration = Duration::from_secs(2);

/// Margen con el que se arma el temporizador de colapso.
///
/// [`WindowIdle::mode`] compara instantes: si el temporizador vence justo en
/// el umbral puede adelantarse unas décimas de milisegundo, la cuenta todavía
/// no llega y no se achica. Como el temporizador es de un solo tiro, esa
/// carrera dejaba la ventana abierta para siempre (pasaba a veces). Armarlo
/// un poco después del umbral la elimina, y si el mouse vuelve a entrar en el
/// medio la regla sigue diciendo "normal".
pub const COLLAPSE_SLACK: Duration = Duration::from_millis(10);

/// En qué forma está la ventana.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowMode {
    /// La interfaz de siempre.
    Normal,
    /// El círculo semitranslúcido.
    Circle,
}

/// El colapso por inactividad: cuenta desde el último aviso del mouse.
///
/// No sabe de píxeles ni de temporizadores: la UI avisa cuándo pasó el mouse
/// y pregunta en qué forma tiene que estar la ventana.
pub struct WindowIdle {
    last_pointer: Instant,
    inside: bool,
}

impl WindowIdle {
    /// Arranca la cuenta en `now`, con el mouse fuera de la ventana.
    pub fn new(now: Instant) -> Self {
        Self {
            last_pointer: now,
            inside: false,
        }
    }

    /// El mouse entró en la ventana.
    pub fn pointer_entered(&mut self, now: Instant) {
        self.last_pointer = now;
        self.inside = true;
    }

    /// El mouse salió de la ventana: desde acá corre la cuenta.
    pub fn pointer_left(&mut self, now: Instant) {
        self.last_pointer = now;
        self.inside = false;
    }

    /// En qué forma tiene que estar la ventana en `now`.
    pub fn mode(&self, now: Instant) -> WindowMode {
        if self.inside || now.duration_since(self.last_pointer) < IDLE_COLLAPSE {
            WindowMode::Normal
        } else {
            WindowMode::Circle
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn sin_mouse_durante_dos_segundos_la_ventana_se_achica() {
        let start = Instant::now();
        let idle = WindowIdle::new(start);

        assert_eq!(
            idle.mode(start + Duration::from_secs(2)),
            WindowMode::Circle
        );
    }

    #[test]
    fn el_mouse_rearma_los_dos_segundos() {
        let start = Instant::now();
        let mut idle = WindowIdle::new(start);

        // Mientras el mouse está encima, no se achica aunque pasen 5 s.
        idle.pointer_entered(start);
        assert_eq!(
            idle.mode(start + Duration::from_secs(5)),
            WindowMode::Normal
        );

        // Al salir, la cuenta arranca de nuevo.
        let left = start + Duration::from_secs(5);
        idle.pointer_left(left);
        assert_eq!(
            idle.mode(left + Duration::from_millis(1_900)),
            WindowMode::Normal
        );
        assert_eq!(idle.mode(left + Duration::from_secs(2)), WindowMode::Circle);
    }

    #[test]
    fn en_circulo_y_sin_mouse_sigue_en_circulo() {
        let start = Instant::now();
        let idle = WindowIdle::new(start);

        assert_eq!(
            idle.mode(start + Duration::from_secs(2)),
            WindowMode::Circle
        );
        assert_eq!(
            idle.mode(start + Duration::from_secs(60)),
            WindowMode::Circle
        );
    }
}
