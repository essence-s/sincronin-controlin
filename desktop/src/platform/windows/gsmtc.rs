//! Backend real para Windows usando `Windows.Media.Control` (GSMTC).
//!
//! Diseño 100 % por eventos: nos suscribimos a los eventos de `WinRT` y solo
//! hacemos trabajo cuando alguno se dispara. No hay temporizadores consultando
//! al sistema.
//!
//! Hilos:
//! - Hilo "gsmtc" (este módulo): tiene COM en modo MTA, obtiene el gestor de
//!   sesiones, se suscribe a los eventos y atiende un bucle que se queda
//!   bloqueado esperando eventos o comandos de la UI.
//! - Los manejadores de eventos de `WinRT` solo envían un aviso por canal; el
//!   trabajo (leer metadatos, pedir acciones) ocurre siempre en el hilo "gsmtc".

use crate::media::{Command, PlaybackState, Session, Sink, Snapshot};

use super::aumid::Names;

use crossbeam_channel::{Receiver, Sender, unbounded};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use windows::Foundation::{DateTime, TypedEventHandler};
use windows::Media::Control::{
    CurrentSessionChangedEventArgs, GlobalSystemMediaTransportControlsSession,
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
    MediaPropertiesChangedEventArgs, PlaybackInfoChangedEventArgs, SessionsChangedEventArgs,
    TimelinePropertiesChangedEventArgs,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::{Interface, Result as WinResult};

const TICKS_PER_SEC: i64 = 10_000_000;
/// Diferencia entre el epoch de FILETIME (1601) y el de Unix (1970), en ticks de 100 ns.
const FILETIME_TO_UNIX_EPOCH: i64 = 116_444_736_000_000_000;
/// Salto máximo que aceptamos al interpolar la posición (protege ante relojes raros).
const MAX_TIME_JUMP_TICKS: i64 = 60 * TICKS_PER_SEC;
/// Cuánto adelantamos/atrasamos con los botones ±10 s.
const SEEK_STEP: i64 = 10 * TICKS_PER_SEC;

/// Avisos que dejan los manejadores de eventos de `WinRT`.
#[derive(Clone, Copy, Debug)]
enum Wake {
    /// Cambió la sesión actual o la lista de sesiones.
    Sessions,
    /// Cambió el estado de reproducción o la línea temporal.
    Session,
    /// Cambiaron los metadatos (título/artista/álbum/miniatura).
    Props,
}

enum Event {
    Wake(Wake),
    Cmd(Command),
}

/// Sesión a la que estamos suscritos, con los tokens para poder desuscribirnos.
struct Watch {
    session: GlobalSystemMediaTransportControlsSession,
    prop_token: i64,
    info_token: i64,
    timeline_token: i64,
}

/// Una sesión que el sistema publica, con lo que la UI necesita de ella.
struct Entry {
    /// El id con el que la UI la elige.
    id: String,
    /// El nombre que se muestra, numerado si hay varias apps iguales.
    label: String,
    /// El nombre de la app, sin numerar.
    name: String,
    /// El AUMID de la app.
    aumid: String,
    /// La sesión en sí, para poder suscribirse a ella.
    session: GlobalSystemMediaTransportControlsSession,
}

#[derive(Default)]
struct PropsCache {
    title: String,
    artist: String,
    album: String,
}

pub fn spawn(cmd_rx: Receiver<Command>, sink: Sink) {
    std::thread::Builder::new()
        .name("gsmtc".to_string())
        .spawn(move || {
            #[allow(unused_must_use)]
            unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            if let Err(err) = run(cmd_rx, sink) {
                eprintln!("[media] el backend GSMTC se detuvo: {err}");
            }
        })
        .expect("no se pudo crear el hilo de medios");
}

fn run(cmd_rx: Receiver<Command>, sink: Sink) -> WinResult<()> {
    let (wake_tx, wake_rx) = unbounded::<Wake>();

    let manager = SessionManager::RequestAsync()?.join()?;
    eprintln!("[media] gestor GSMTC obtenido");

    // Eventos del gestor: sesión actual y lista de sesiones.
    let t = wake_tx.clone();
    let _current_token = manager.CurrentSessionChanged(&TypedEventHandler::<
        SessionManager,
        CurrentSessionChangedEventArgs,
    >::new(move |_sender, _args| {
        let _ = t.try_send(Wake::Sessions);
        Ok(())
    }))?;
    let t = wake_tx.clone();
    let _sessions_token = manager.SessionsChanged(&TypedEventHandler::<
        SessionManager,
        SessionsChangedEventArgs,
    >::new(move |_sender, _args| {
        let _ = t.try_send(Wake::Sessions);
        Ok(())
    }))?;

    let mut watch: Option<Watch> = None;
    let mut cache = PropsCache::default();
    let mut props_stale = true;

    // La lista de sesiones y la que controlamos.
    let mut names = Names::default();
    let mut entries = read_sessions(&manager, &mut names);
    let mut controlled_id: Option<String> = None;
    // La sesión que eligió el usuario. Mientras haya una, no se sigue a la
    // que el sistema considera actual.
    let mut chosen: Option<String> = None;

    attach_current(&manager, &entries, &mut watch, &mut controlled_id, &wake_tx)?;

    loop {
        // Aquí el hilo se queda bloqueado: no hay sondeo, solo espera de eventos.
        let evento = crossbeam_channel::select! {
            recv(wake_rx) -> msg => match msg {
                Ok(w) => Event::Wake(w),
                Err(_) => break,
            },
            recv(cmd_rx) -> msg => match msg {
                Ok(c) => Event::Cmd(c),
                Err(_) => break,
            },
        };

        // Coalescencia: si han llegado varios eventos mientras trabajábamos,
        // los atendemos de una sola vez.
        let mut sesion_cambio = false;
        let mut props_cambiaron = matches!(evento, Event::Wake(Wake::Props));
        if matches!(evento, Event::Wake(Wake::Sessions)) {
            sesion_cambio = true;
            props_cambiaron = true;
        }
        while let Ok(w) = wake_rx.try_recv() {
            match w {
                Wake::Sessions => {
                    sesion_cambio = true;
                    props_cambiaron = true;
                }
                Wake::Props => props_cambiaron = true,
                Wake::Session => {}
            }
        }
        if props_cambiaron {
            props_stale = true;
        }

        if sesion_cambio {
            entries = read_sessions(&manager, &mut names);

            // Si la app que eligió el usuario ya no está, se vuelve a seguir
            // la que el sistema considera actual.
            if let Some(id) = chosen.as_deref() {
                if !entries.iter().any(|e| e.id == id) {
                    eprintln!("[media] la sesión chosen ya no está: {id}");
                    chosen = None;
                }
            }

            let target = match chosen.as_deref() {
                Some(id) => entries.iter().find(|e| e.id == id).map(|e| e.id.clone()),
                None => current_id(&manager, &entries),
            };
            match target {
                Some(id) => {
                    attach(&entries, &id, &mut watch, &mut controlled_id, &wake_tx)?;
                }
                None => detach(&mut watch, &mut controlled_id),
            }
        }

        if let Event::Cmd(cmd) = evento {
            match cmd {
                Command::SelectSession(id) => {
                    if entries.iter().any(|e| e.id == id) {
                        chosen = Some(id.clone());
                        attach(&entries, &id, &mut watch, &mut controlled_id, &wake_tx)?;
                        // Los metadatos son de otra sesión: hay que releerlos.
                        props_stale = true;
                    } else {
                        eprintln!("[media] no hay ninguna sesión con ese id: {id}");
                    }
                }
                otra => execute(otra, watch.as_ref()),
            }
        }

        publish(
            &sink,
            watch.as_ref(),
            &mut cache,
            &mut props_stale,
            &entries,
            controlled_id.as_deref(),
        );
    }

    Ok(())
}

/// Relee la lista de sesiones que publica el sistema.
///
/// Windows no da un id de sesión: lo único que hay es el AUMID de la app. Si
/// dos sesiones lo comparten —dos instancias del mismo ejecutable— se
/// numeran, como en Linux con el nombre de bus.
fn read_sessions(manager: &SessionManager, names: &mut Names) -> Vec<Entry> {
    let Ok(all) = manager.GetSessions() else {
        return Vec::new();
    };

    let mut raw: Vec<(String, String, GlobalSystemMediaTransportControlsSession)> = Vec::new();
    for session in all {
        let aumid = session
            .SourceAppUserModelId()
            .map(|a| a.to_string())
            .unwrap_or_default();
        let name = names.name(&aumid);
        raw.push((aumid, name, session));
    }

    // Orden estable: por nombre y después por AUMID.
    raw.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

    let pairs: Vec<(String, String)> = raw
        .iter()
        .map(|(aumid, name, _)| (aumid.clone(), name.clone()))
        .collect();

    raw.into_iter()
        .zip(identities(&pairs))
        .map(|((aumid, name, session), (id, label))| Entry {
            id,
            label,
            name,
            aumid,
            session,
        })
        .collect()
}

/// El id y la etiqueta de cada sesión, con las repetidas numeradas.
///
/// El id es el AUMID de la app. Si dos sesiones lo comparten —dos instancias
/// del mismo ejecutable— hay que distinguirlas, y lo mismo con las etiquetas:
/// "Chrome (1)", "Chrome (2)".
fn identities(entries: &[(String, String)]) -> Vec<(String, String)> {
    let mut by_aumid: HashMap<&str, usize> = HashMap::new();
    let mut by_name: HashMap<&str, usize> = HashMap::new();
    for (aumid, name) in entries {
        *by_aumid.entry(aumid.as_str()).or_default() += 1;
        *by_name.entry(name.as_str()).or_default() += 1;
    }

    let mut seen_aumid: HashMap<&str, usize> = HashMap::new();
    let mut seen_name: HashMap<&str, usize> = HashMap::new();
    let mut out = Vec::with_capacity(entries.len());
    for (aumid, name) in entries {
        let mut id = aumid.clone();
        if by_aumid[aumid.as_str()] > 1 {
            let n = seen_aumid.entry(aumid.as_str()).or_default();
            *n += 1;
            id = format!("{aumid}#{n}");
        }

        let mut label = name.clone();
        if by_name[name.as_str()] > 1 {
            let n = seen_name.entry(name.as_str()).or_default();
            *n += 1;
            label = format!("{name} ({n})");
        }

        out.push((id, label));
    }
    out
}

/// Se suscribe a una sesión y deja de escuchar la anterior.
fn attach(
    entries: &[Entry],
    id: &str,
    watch: &mut Option<Watch>,
    controlled_id: &mut Option<String>,
    wake_tx: &Sender<Wake>,
) -> WinResult<()> {
    let Some(entry) = entries.iter().find(|e| e.id == id) else {
        return Ok(());
    };

    // Si ya estamos en esa sesión, no hay que volver a suscribirse: dos
    // referencias al mismo objeto COM tienen el mismo puntero.
    if let Some(actual) = watch.as_ref() {
        if std::ptr::eq(actual.session.as_raw(), entry.session.as_raw()) {
            *controlled_id = Some(id.to_string());
            return Ok(());
        }
    }

    if let Some(anterior) = watch.take() {
        unsubscribe(&anterior);
    }
    let tokens = subscribe(&entry.session, wake_tx)?;
    *watch = Some(Watch {
        session: entry.session.clone(),
        prop_token: tokens.0,
        info_token: tokens.1,
        timeline_token: tokens.2,
    });
    *controlled_id = Some(id.to_string());
    eprintln!("[media] controlando: {} ({id})", entry.label);
    Ok(())
}

/// Deja de escuchar la sesión actual.
fn detach(watch: &mut Option<Watch>, controlled_id: &mut Option<String>) {
    if let Some(anterior) = watch.take() {
        unsubscribe(&anterior);
    }
    *controlled_id = None;
}

/// El id de la sesión que el sistema considera actual.
fn current_id(manager: &SessionManager, entries: &[Entry]) -> Option<String> {
    // Sin sesión activa, GSMTC devuelve un error (puntero nulo), no un error real.
    let actual = manager.GetCurrentSession().ok()?;
    Some(entry_of(entries, &actual)?.id.clone())
}

/// La entrada que corresponde a una sesión del sistema.
fn entry_of<'a>(
    entries: &'a [Entry],
    session: &GlobalSystemMediaTransportControlsSession,
) -> Option<&'a Entry> {
    // Dos referencias al mismo objeto COM tienen el mismo puntero.
    if let Some(entry) = entries
        .iter()
        .find(|e| std::ptr::eq(e.session.as_raw(), session.as_raw()))
    {
        return Some(entry);
    }
    // Si no, se busca por AUMID.
    let aumid = session.SourceAppUserModelId().ok()?;
    entries.iter().find(|e| e.aumid == aumid)
}

/// Se suscribe a la sesión que el sistema considera actual.
fn attach_current(
    manager: &SessionManager,
    entries: &[Entry],
    watch: &mut Option<Watch>,
    controlled_id: &mut Option<String>,
    wake_tx: &Sender<Wake>,
) -> WinResult<()> {
    match current_id(manager, entries) {
        Some(id) => attach(entries, &id, watch, controlled_id, wake_tx),
        None => {
            detach(watch, controlled_id);
            Ok(())
        }
    }
}

fn subscribe(
    session: &GlobalSystemMediaTransportControlsSession,
    wake_tx: &Sender<Wake>,
) -> WinResult<(i64, i64, i64)> {
    let t = wake_tx.clone();
    let prop_token = session.MediaPropertiesChanged(&TypedEventHandler::<
        GlobalSystemMediaTransportControlsSession,
        MediaPropertiesChangedEventArgs,
    >::new(move |_sender, _args| {
        let _ = t.try_send(Wake::Props);
        Ok(())
    }))?;

    let t = wake_tx.clone();
    let info_token = session.PlaybackInfoChanged(&TypedEventHandler::<
        GlobalSystemMediaTransportControlsSession,
        PlaybackInfoChangedEventArgs,
    >::new(move |_sender, _args| {
        let _ = t.try_send(Wake::Session);
        Ok(())
    }))?;

    let t = wake_tx.clone();
    let timeline_token =
        session.TimelinePropertiesChanged(&TypedEventHandler::<
            GlobalSystemMediaTransportControlsSession,
            TimelinePropertiesChangedEventArgs,
        >::new(move |_sender, _args| {
            let _ = t.try_send(Wake::Session);
            Ok(())
        }))?;

    Ok((prop_token, info_token, timeline_token))
}

fn unsubscribe(watch: &Watch) {
    let _ = watch.session.RemoveMediaPropertiesChanged(watch.prop_token);
    let _ = watch.session.RemovePlaybackInfoChanged(watch.info_token);
    let _ = watch
        .session
        .RemoveTimelinePropertiesChanged(watch.timeline_token);
}

/// Lee el estado actual y lo envía a la UI.
fn publish(
    sink: &Sink,
    watch: Option<&Watch>,
    cache: &mut PropsCache,
    props_stale: &mut bool,
    entries: &[Entry],
    controlled_id: Option<&str>,
) {
    let Some(w) = watch else {
        *props_stale = false;
        sink(Snapshot::no_session());
        return;
    };
    let session = &w.session;

    if *props_stale {
        if let Ok(props) = session
            .TryGetMediaPropertiesAsync()
            .and_then(|op| op.join())
        {
            cache.title = props.Title().map(|s| s.to_string()).unwrap_or_default();
            cache.artist = props.Artist().map(|s| s.to_string()).unwrap_or_default();
            cache.album = props
                .AlbumTitle()
                .map(|s| s.to_string())
                .unwrap_or_default();
        }
        *props_stale = false;
    }

    let (state, can_toggle, can_seek) = read_controls(session);
    let (position_ticks, duration_ticks) = read_timeline(session, state);

    // El nombre de la app que controlamos: sale de la lista de sesiones.
    let source_app = controlled_id
        .and_then(|id| entries.iter().find(|e| e.id == id))
        .map(|e| e.name.clone())
        .unwrap_or_default();

    sink(Snapshot {
        has_session: true,
        title: cache.title.clone(),
        artist: cache.artist.clone(),
        album: cache.album.clone(),
        source_app,
        state,
        can_toggle,
        can_seek,
        position_ticks,
        duration_ticks,
        // Todas las sesiones que publica el sistema. Con una sola, la UI no
        // muestra el selector.
        sessions: entries
            .iter()
            .map(|e| Session {
                id: e.id.clone(),
                label: e.label.clone(),
            })
            .collect(),
        session_id: controlled_id.unwrap_or_default().to_string(),
    });
}

fn read_controls(
    session: &GlobalSystemMediaTransportControlsSession,
) -> (PlaybackState, bool, bool) {
    let Ok(info) = session.GetPlaybackInfo() else {
        return (PlaybackState::Unknown, true, true);
    };

    let state = match info.PlaybackStatus() {
        Ok(PlaybackStatus::Playing) => PlaybackState::Playing,
        Ok(PlaybackStatus::Paused) => PlaybackState::Paused,
        Ok(PlaybackStatus::Stopped) => PlaybackState::Stopped,
        // `Closed` y `Opened` son estados del ciclo de vida de una sesión
        // WinRT, no del reproductor: para el usuario no hay nada sonando.
        Ok(PlaybackStatus::Closed) | Ok(PlaybackStatus::Opened) => PlaybackState::Stopped,
        Ok(PlaybackStatus::Changing) => PlaybackState::Unknown,
        // GSMTC puede ampliar los estados en el futuro; cualquier otro se trata como desconocido.
        Ok(_) | Err(_) => PlaybackState::Unknown,
    };

    let (mut can_toggle, mut can_seek) = (true, true);
    if let Ok(controls) = info.Controls() {
        can_toggle = controls.IsPlayPauseToggleEnabled().unwrap_or(true);
        can_seek = controls.IsPlaybackPositionEnabled().unwrap_or(true);
    }

    (state, can_toggle, can_seek)
}

fn read_timeline(
    session: &GlobalSystemMediaTransportControlsSession,
    state: PlaybackState,
) -> (i64, i64) {
    let Ok(timeline) = session.GetTimelineProperties() else {
        return (0, 0);
    };

    let mut position = timeline.Position().map_or(0, |t| t.Duration);
    let mut duration = timeline.MaxSeekTime().map_or(0, |t| t.Duration);
    if duration <= 0 {
        duration = timeline.EndTime().map_or(0, |t| t.Duration);
    }

    // Mientras suena, la posición real es la última reportada + lo transcurrido.
    if state == PlaybackState::Playing {
        if let Ok(last_update) = timeline.LastUpdatedTime() {
            position += ticks_since(last_update);
        }
    }
    if duration > 0 {
        position = position.min(duration);
    }

    (position.max(0), duration.max(0))
}

fn ticks_since(moment: DateTime) -> i64 {
    let last_unix = moment.UniversalTime.saturating_sub(FILETIME_TO_UNIX_EPOCH);
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(last_unix, |d| {
            i64::try_from(d.as_nanos() / 100).unwrap_or(i64::MAX)
        });
    (now_unix - last_unix).clamp(0, MAX_TIME_JUMP_TICKS)
}

fn execute(cmd: Command, watch: Option<&Watch>) {
    let Some(w) = watch else {
        return;
    };
    let session = &w.session;

    match cmd {
        Command::TogglePlayPause => {
            match session.TryTogglePlayPauseAsync().and_then(|op| op.join()) {
                Ok(false) => eprintln!("[media] la aplicación no aceptó play/pausa"),
                Err(err) => eprintln!("[media] error en play/pausa: {err}"),
                Ok(true) => {}
            }
        }
        Command::SeekBackward => seek_by(session, -SEEK_STEP),
        Command::SeekForward => seek_by(session, SEEK_STEP),
        // Elegir sesión se atiende en el bucle, que es el que sabe a cuál
        // estamos controlando.
        Command::SelectSession(id) => {
            eprintln!("[media] elegir sesión se atiende en el bucle: {id}");
        }
    }
}

fn seek_by(session: &GlobalSystemMediaTransportControlsSession, delta: i64) {
    let Ok(timeline) = session.GetTimelineProperties() else {
        return;
    };

    let start = timeline.MinSeekTime().map_or(0, |t| t.Duration);
    let end = timeline.MaxSeekTime().map_or(0, |t| t.Duration);
    let mut current = timeline.Position().map_or(0, |t| t.Duration);

    if read_controls(session).0 == PlaybackState::Playing {
        if let Ok(last_update) = timeline.LastUpdatedTime() {
            current += ticks_since(last_update);
        }
    }

    let target = if end > start {
        (current + delta).clamp(start, end)
    } else {
        (current + delta).max(0)
    };

    match session
        .TryChangePlaybackPositionAsync(target)
        .and_then(|op| op.join())
    {
        Ok(true) => eprintln!(
            "[media] posición cambiada a {:.1}s",
            target as f64 / TICKS_PER_SEC as f64
        ),
        Ok(false) => eprintln!("[media] esta aplicación no admite cambiar la posición"),
        Err(err) => eprintln!("[media] error al cambiar la posición: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(aumid: &str, name: &str) -> (String, String) {
        (aumid.to_string(), name.to_string())
    }

    #[test]
    fn numbers_the_repeated_sessions() {
        // Dos instancias del mismo ejecutable: mismo AUMID y mismo nombre.
        let entries = vec![
            session("308046B0AF4A39CB", "Firefox"),
            session("308046B0AF4A39CB", "Firefox"),
        ];

        let ids = identities(&entries);
        assert_eq!(ids[0].0, "308046B0AF4A39CB#1");
        assert_eq!(ids[0].1, "Firefox (1)");
        assert_eq!(ids[1].0, "308046B0AF4A39CB#2");
        assert_eq!(ids[1].1, "Firefox (2)");
    }

    #[test]
    fn leaves_the_unique_ones_alone() {
        let entries = vec![session("MSEdge", "Edge"), session("chrome.exe", "Chrome")];

        let ids = identities(&entries);
        assert_eq!(ids[0], ("MSEdge".to_string(), "Edge".to_string()));
        assert_eq!(ids[1], ("chrome.exe".to_string(), "Chrome".to_string()));
    }

    #[test]
    fn numbers_the_labels_with_different_ids() {
        // Dos apps distintas con el mismo nombre: los ids no se tocan.
        let entries = vec![session("MSEdge", "Edge"), session("MSEdge2", "Edge")];

        let ids = identities(&entries);
        assert_eq!(ids[0].0, "MSEdge");
        assert_eq!(ids[0].1, "Edge (1)");
        assert_eq!(ids[1].0, "MSEdge2");
        assert_eq!(ids[1].1, "Edge (2)");
    }
}
