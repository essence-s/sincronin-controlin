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

use crate::media::{Command, PlaybackState, Sink, Snapshot};

use crossbeam_channel::{Receiver, Sender, unbounded};
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
use windows::core::Result as WinResult;

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

#[derive(Default)]
struct PropsCache {
    title: String,
    artist: String,
    album: String,
    app: String,
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

    attach_current_session(&manager, &wake_tx, &mut watch, &mut props_stale)?;
    publish(&sink, watch.as_ref(), &mut cache, &mut props_stale);

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
            attach_current_session(&manager, &wake_tx, &mut watch, &mut props_stale)?;
        }

        if let Event::Cmd(cmd) = evento {
            execute(cmd, watch.as_ref());
        }

        publish(&sink, watch.as_ref(), &mut cache, &mut props_stale);
    }

    Ok(())
}

/// Comprueba cuál es la sesión actual y, si cambió, se (des)suscribe a sus eventos.
fn attach_current_session(
    manager: &SessionManager,
    wake_tx: &Sender<Wake>,
    watch: &mut Option<Watch>,
    props_stale: &mut bool,
) -> WinResult<()> {
    // Sin sesión activa, GSMTC devuelve un error (puntero nulo), no un error real.
    let new_session = manager.GetCurrentSession().ok();

    let unchanged = match (watch.as_ref(), new_session.as_ref()) {
        (Some(watched), Some(candidate)) => &watched.session == candidate,
        (None, None) => true,
        _ => false,
    };
    if unchanged {
        return Ok(());
    }

    if let Some(previous) = watch.take() {
        unsubscribe(&previous);
    }
    *props_stale = true;

    if let Some(session) = new_session {
        let app = session
            .SourceAppUserModelId()
            .map(|s| s.to_string())
            .unwrap_or_default();
        eprintln!("[media] sesión actual: {app}");
        let tokens = subscribe(&session, wake_tx)?;
        *watch = Some(Watch {
            session,
            prop_token: tokens.0,
            info_token: tokens.1,
            timeline_token: tokens.2,
        });
    } else {
        eprintln!("[media] no hay ninguna sesión multimedia activa");
    }

    Ok(())
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
fn publish(sink: &Sink, watch: Option<&Watch>, cache: &mut PropsCache, props_stale: &mut bool) {
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
        cache.app = session
            .SourceAppUserModelId()
            .map(|s| s.to_string())
            .unwrap_or_default();
        *props_stale = false;
    }

    let (state, can_toggle, can_seek) = read_controls(session);
    let (position_ticks, duration_ticks) = read_timeline(session, state);

    sink(Snapshot {
        has_session: true,
        title: cache.title.clone(),
        artist: cache.artist.clone(),
        album: cache.album.clone(),
        source_app: cache.app.clone(),
        state,
        can_toggle,
        can_seek,
        position_ticks,
        duration_ticks,
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
