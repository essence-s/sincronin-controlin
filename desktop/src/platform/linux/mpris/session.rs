//! Descubrimiento y selección de reproductores MPRIS.
//!
//! El conjunto de reproductores cambia cuando alguien entra o sale del bus.
//! `find_all()` es caro (hace una introspección por reproductor), así que solo
//! se llama cuando el bus avisa un cambio o cuando hay que elegir de nuevo.

use std::time::Duration;

use mpris::{PlaybackStatus, Player, PlayerFinder};

use crate::domain::media::Session;

/// Los nombres de bus de los proxies conocidos.
///
/// Un proxy refleja el reproductor de otro proceso: no es una sesión propia,
/// sino una copia de otra. `playerctld` es el caso habitual.
const PROXY_NAMES: &[&str] = &["playerctld"];

/// Qué se espera cuando no hay ningún reproductor en el bus antes de volver
/// a buscar uno.
pub const RETRY_WITHOUT_PLAYER: Duration = Duration::from_secs(2);

/// Los reproductores que hay en el bus y la lista que ve la UI.
pub struct Catalog {
    finder: PlayerFinder,
    sessions: Vec<Session>,
}

impl Catalog {
    pub fn new() -> Result<Self, mpris::DBusError> {
        Ok(Self {
            finder: PlayerFinder::new()?,
            sessions: Vec::new(),
        })
    }

    /// Las sesiones disponibles, para el selector de la UI.
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    /// Relee el bus y deja la lista de sesiones al día.
    pub fn refresh(&mut self) {
        let _ = self.players();
    }

    /// Elige con qué reproductor trabajar.
    ///
    /// `preferred` es lo que se busca primero: el `id` de la sesión que eligió
    /// el usuario, o —si el reproductor se fue— el nombre del programa, para
    /// seguir con la misma aplicación aunque sea otra instancia.
    ///
    /// `exclude` es el reproductor que acaba de irse: no se vuelve a elegir, o
    /// el bucle se engancharía con él.
    ///
    /// Si no hay ninguno de los dos, se prefiere el que está sonando; si hay
    /// varios pausados, el primero en orden estable.
    pub fn pick(&mut self, preferred: Option<&str>, exclude: Option<&str>) -> Option<Player> {
        let all: Vec<Player> = self
            .players()
            .into_iter()
            .filter(|p| Some(p.bus_name()) != exclude)
            .collect();
        if all.is_empty() {
            return None;
        }

        // 1. El preferido, si sigue en la lista.
        let chosen = preferred
            .and_then(|want| all.iter().position(|p| p.bus_name() == want))
            // 2. El mismo programa, si es otro el que se fue.
            .or_else(|| preferred.and_then(|want| all.iter().position(|p| p.identity() == want)))
            // 3. El que está sonando.
            .or_else(|| {
                all.iter()
                    .position(|p| matches!(p.get_playback_status(), Ok(PlaybackStatus::Playing)))
            })
            // 4. El primero que exista.
            .unwrap_or(0);

        all.into_iter().nth(chosen)
    }

    /// Relee el bus y devuelve los reproductores, sin los proxies.
    fn players(&mut self) -> Vec<Player> {
        match self.finder.find_all() {
            Ok(all) => {
                let players = without_proxies(all);
                let raw: Vec<(String, String)> = players
                    .iter()
                    .map(|p| (p.bus_name().to_string(), p.identity().to_string()))
                    .collect();
                self.sessions = sessions_from(&raw);
                players
            }
            Err(err) => {
                eprintln!("[media] no se pudieron listar los reproductores: {err}");
                Vec::new()
            }
        }
    }
}

/// Quita los proxies: nombres de bus que reflejan el reproductor de otro.
///
/// `playerctld` se anuncia con su propio nombre de bus pero con la identidad
/// del reproductor que refleja. Si no se filtra, el mismo reproductor
/// aparece dos veces en el selector.
fn without_proxies(all: Vec<Player>) -> Vec<Player> {
    all.into_iter()
        .filter(|p| !is_proxy(p.bus_name()))
        .collect()
}

/// Si el nombre de bus corresponde a un proxy conocido.
fn is_proxy(bus_name: &str) -> bool {
    let tail = bus_name.rsplit('.').next().unwrap_or(bus_name);
    PROXY_NAMES.iter().any(|p| tail.eq_ignore_ascii_case(p))
}

/// Arma la lista de sessions a partir de pares `(id, etiqueta)`.
///
/// Cada reproductor es una sesión: dos ventanas del mismo navegador o dos
/// instancias de mpv son sessions distintas, aunque compartan nombre. Por eso
/// la clave es el nombre de bus y no la identidad.
///
/// Cuando varias sessions comparten nombre se numeran, para que el usuario
/// pueda distinguirlas. Recibe datos planos para poder probarse sin un bus.
fn sessions_from(raw: &[(String, String)]) -> Vec<Session> {
    let mut sessions: Vec<Session> = raw
        .iter()
        .map(|(id, label)| Session {
            id: id.clone(),
            label: label.clone(),
        })
        .collect();

    // Orden estable: por etiqueta y después por id. Así los números de las
    // sessions repetidas no cambian entre una foto y otra.
    sessions.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.id.cmp(&b.id)));

    // Numerar las que comparten etiqueta.
    let mut i = 0;
    while i < sessions.len() {
        let mut j = i + 1;
        while j < sessions.len() && sessions[j].label == sessions[i].label {
            j += 1;
        }
        if j - i > 1 {
            for (k, s) in sessions.iter_mut().enumerate().take(j).skip(i) {
                s.label = format!("{} ({})", s.label, k - i + 1);
            }
        }
        i = j;
    }

    sessions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_the_known_proxies() {
        assert!(is_proxy("org.mpris.MediaPlayer2.playerctld"));
        assert!(is_proxy("org.mpris.MediaPlayer2.PLAYERCTLD"));
        assert!(!is_proxy("org.mpris.MediaPlayer2.mpv.instance-sLbacCqj"));
        assert!(!is_proxy("org.mpris.MediaPlayer2.firefox.instance_1_40"));
        // El nombre del proxy puede ser el prefijo de otro, pero no su cola.
        assert!(!is_proxy("org.mpris.MediaPlayer2.playerctld.helper"));
    }

    #[test]
    fn keeps_players_with_the_same_name_apart() {
        // Dos ventanas del mismo navegador: mismo nombre, sessions distintas.
        let sessions = sessions_from(&[
            (
                "org.mpris.MediaPlayer2.firefox.instance_1_40".to_string(),
                "Mozilla zen".to_string(),
            ),
            (
                "org.mpris.MediaPlayer2.firefox.instance_2_41".to_string(),
                "Mozilla zen".to_string(),
            ),
        ]);

        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions[0].id,
            "org.mpris.MediaPlayer2.firefox.instance_1_40"
        );
        assert_eq!(sessions[0].label, "Mozilla zen (1)");
        assert_eq!(
            sessions[1].id,
            "org.mpris.MediaPlayer2.firefox.instance_2_41"
        );
        assert_eq!(sessions[1].label, "Mozilla zen (2)");
    }

    #[test]
    fn leaves_unique_names_alone() {
        let sessions = sessions_from(&[
            (
                "org.mpris.MediaPlayer2.mpv.instance-sLbacCqj".to_string(),
                "mpv".to_string(),
            ),
            (
                "org.mpris.MediaPlayer2.firefox.instance_1_40".to_string(),
                "Mozilla zen".to_string(),
            ),
        ]);

        assert_eq!(sessions[0].label, "Mozilla zen");
        assert_eq!(sessions[1].label, "mpv");
    }

    #[test]
    fn the_order_does_not_depend_on_the_order_of_the_bus() {
        let a = (
            "org.mpris.MediaPlayer2.firefox.instance_2_41".to_string(),
            "Mozilla zen".to_string(),
        );
        let b = (
            "org.mpris.MediaPlayer2.firefox.instance_1_40".to_string(),
            "Mozilla zen".to_string(),
        );

        let uno = sessions_from(&[a.clone(), b.clone()]);
        let otro = sessions_from(&[b, a]);

        assert_eq!(uno, otro);
        assert_eq!(uno[0].id, "org.mpris.MediaPlayer2.firefox.instance_1_40");
    }
}
