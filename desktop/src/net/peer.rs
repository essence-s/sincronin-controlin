//! El hilo de par: el `Rtc` de str0m y el socket UDP.
//!
//! Es el único que toca la conexión punto a punto. El bucle es el
//! canónico de `str0m`: se le pide que produzca una salida, se manda lo
//! que pida y se le pasa lo que llega del socket.
//!
//! ## El candidato local
//!
//! Se anuncia **uno solo**, y el socket se abre en esa dirección exacta.
//! No es arbitrario: el agente ICE descarta los paquetes STUN que
//! llegan a una dirección que no sea uno de sus candidatos, así que un
//! socket en `0.0.0.0` (que recibe en cualquier dirección) no sirve.
//! Anunciar varias interfaces requiere un socket por cada una y saber
//! cuál usó `str0m` para cada par: eso queda para otra versión.
//!
//! ## Lo que falta
//!
//! Solo hay candidatos de host: entre dos máquinas en la misma red anda,
//! pero atravesar un NAT no. Para eso hacen falta candidatos "server
//! reflexive" (STUN) y, si tampoco alcanza, un relay (TURN). El STUN se
//! puede agregar con el crate `is` (que ya es dependencia de `str0m`)
//! contra un servidor público tipo `stun:stun.l.google.com:19302`.

use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde_json::Value;
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcError};

use crate::net::{self, NetState, PeerCommand, PeerOut, Sink};

/// Etiqueta del canal de datos.
const CHANNEL_LABEL: &str = "control";

/// Cuanto espera el hilo cuando no hay sesión.
const IDLE_WAIT: Duration = Duration::from_millis(100);

/// El tope de la espera del socket.
///
/// Los comandos de la UI no pueden quedar esperando a que `str0m` diga
/// cuando es el proximo timeout: ese timeout puede ser de segundos.
const MAX_WAIT: Duration = Duration::from_millis(100);

/// El tamaño del buffer de recepción.
///
/// Es el tamaño máximo de un datagram de WebRTC (con el overhead de IP y
/// UDP, 1500 bytes de MTU).
const BUFFER_SIZE: usize = 2048;

/// La sesión punto a punto.
struct Session {
    /// El estado de WebRTC.
    rtc: Rtc,
    /// El socket UDP.
    socket: UdpSocket,
    /// La oferta que mandamos, esperando la respuesta.
    pending: Option<SdpPendingOffer>,
    /// El canal de datos, cuando se abre.
    channel: Option<ChannelId>,
    /// El candidato local, para mandarlo con la oferta o la respuesta.
    local: Option<Candidate>,
    /// Si ya mandamos una oferta: para no mandar dos.
    offered: bool,
}

/// El hilo de par.
pub(crate) fn run(
    cmd_rx: Receiver<PeerCommand>,
    out_tx: Sender<PeerOut>,
    sink: Sink,
) -> Result<(), RtcError> {
    let mut session: Option<Session> = None;
    let mut buf: Vec<u8> = Vec::new();

    loop {
        // 1) Los comandos que ya están esperando.
        while let Ok(cmd) = cmd_rx.try_recv() {
            if !handle_command(cmd, &mut session, &out_tx, &sink) {
                break;
            }
        }

        // 2) Sin sesión no hay nada que hacer: esperar un comando.
        let Some(current) = session.as_mut() else {
            match cmd_rx.recv_timeout(IDLE_WAIT) {
                Ok(cmd) => {
                    handle_command(cmd, &mut session, &out_tx, &sink);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            continue;
        };

        // 3) El bucle de str0m.
        let timeout = match current.rtc.poll_output() {
            Ok(Output::Timeout(v)) => v,
            Ok(Output::Transmit(v)) => {
                if let Err(err) = current.socket.send_to(&v.contents, v.destination) {
                    eprintln!("[net] no se pudo mandar el paquete: {err}");
                }
                continue;
            }
            Ok(Output::Event(v)) => {
                handle_event(v, current, &sink);
                continue;
            }
            Err(err) => {
                eprintln!("[net] error de WebRTC: {err}");
                break;
            }
        };

        let remaining = timeout.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            current.rtc.handle_input(Input::Timeout(Instant::now()))?;
            continue;
        }

        // El socket espera lo que falta para el timeout de str0m, pero no
        // más de MAX_WAIT: los comandos de la UI no pueden quedar
        // esperando segundos.
        let wait = remaining.min(MAX_WAIT);
        current.socket.set_read_timeout(Some(wait))?;

        // El buffer tiene que estar listo antes de recibir: con un buffer
        // vacío el datagram se trunca a cero bytes.
        buf.resize(BUFFER_SIZE, 0);

        let input = match current.socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                buf.truncate(n);
                Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: current.socket.local_addr()?,
                        contents: buf.as_slice().try_into()?,
                    },
                )
            }
            // Se cumplio el timeout del socket. Si el de str0m todavía no
            // vencio, solo seguimos esperando: no hay nada que avisarle.
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if remaining <= MAX_WAIT {
                    Input::Timeout(Instant::now())
                } else {
                    continue;
                }
            }
            Err(err) => return Err(err.into()),
        };

        current.rtc.handle_input(input)?;
    }

    Ok(())
}

/// Atiende un comando del hilo de señalización.
///
/// Devuelve `false` si hay que dejar de drenar comandos: después de
/// escribir en el canal de datos hay que llamar a `poll_output` antes de
/// escribir otra vez.
fn handle_command(
    cmd: PeerCommand,
    session: &mut Option<Session>,
    out_tx: &Sender<PeerOut>,
    sink: &Sink,
) -> bool {
    match cmd {
        PeerCommand::Start { id } => {
            // La sesión se crea una sola vez: si ya hay una, se deja.
            if session.is_none() {
                eprintln!("[net] se abre la sesión (id {id})");
                match start() {
                    Ok(new) => {
                        *session = Some(new);
                    }
                    Err(err) => {
                        eprintln!("[net] no se pudo abrir el socket: {err}");
                        net::publish_notice(sink, format!("no se pudo abrir el socket UDP: {err}"));
                    }
                }
            }
            true
        }
        PeerCommand::MakeOffer => {
            if let Some(current) = session.as_mut() {
                if current.offered {
                    // Ya se ofrecio: no se ofrece dos veces.
                    return true;
                }
                current.offered = true;

                if let Err(err) = make_offer(current, out_tx) {
                    eprintln!("[net] no se pudo hacer la oferta: {err}");
                    net::publish_notice(sink, format!("no se pudo hacer la oferta: {err}"));
                }
            }
            true
        }
        PeerCommand::AcceptOffer(sdp) => {
            if let Some(current) = session.as_mut() {
                if let Err(err) = accept_offer(current, &sdp, out_tx) {
                    eprintln!("[net] no se pudo aceptar la oferta: {err}");
                    net::publish_notice(sink, format!("no se pudo aceptar la oferta: {err}"));
                }
            }
            true
        }
        PeerCommand::AcceptAnswer(sdp) => {
            if let Some(current) = session.as_mut() {
                if let Err(err) = accept_answer(current, &sdp) {
                    eprintln!("[net] no se pudo aceptar la respuesta: {err}");
                    net::publish_notice(sink, format!("no se pudo aceptar la respuesta: {err}"));
                }
            }
            true
        }
        PeerCommand::AddIce(value) => {
            if let Some(current) = session.as_mut() {
                match serde_json::from_value::<Candidate>(value) {
                    Ok(candidate) => {
                        current.rtc.add_remote_candidate(candidate);
                    }
                    Err(err) => {
                        eprintln!("[net] candidato ICE que no se entiende: {err}");
                    }
                }
            }
            true
        }
        PeerCommand::Send(text) => {
            if let Some(current) = session.as_mut() {
                send_message(current, &text, sink);
            }
            // Después de escribir hay que pasar por `poll_output` antes
            // de escribir otra vez.
            false
        }
        PeerCommand::Stop => {
            // Se cae la sesión: el socket se cierra y la proxima vez se
            // abre de cero.
            *session = None;
            true
        }
    }
}

/// Abre el socket UDP y prepara el `Rtc`.
fn start() -> io::Result<Session> {
    let socket = UdpSocket::bind(local_address()?)?;
    let local_addr = socket.local_addr()?;

    let rtc = Rtc::new(Instant::now());
    let candidate = Candidate::host(local_addr, "udp")
        .map_err(|err| io::Error::other(format!("no se pudo crear el candidato local: {err}")))?;

    eprintln!("[net] socket UDP en {local_addr}");

    Ok(Session {
        rtc,
        socket,
        pending: None,
        channel: None,
        local: Some(candidate),
        offered: false,
    })
}

/// Crea el canal de datos, arma la oferta y la manda.
fn make_offer(current: &mut Session, out_tx: &Sender<PeerOut>) -> Result<(), RtcError> {
    let (offer, pending) = {
        let mut changes = current.rtc.sdp_api();
        changes.add_channel_with_config(ChannelConfig {
            label: CHANNEL_LABEL.to_string(),
            // Sin orden y con un solo reintento: un mensaje de prueba no
            // vale la pena esperar los 200 ms del RTO de SCTP, y el
            // head-of-line blocking tampoco.
            ordered: false,
            reliability: Reliability::MaxRetransmits { retransmits: 1 },
            ..Default::default()
        });

        changes.apply().expect("se agregó un canal, hay cambios")
    };

    current.pending = Some(pending);

    let value = serde_json::to_value(&offer).expect("el SDP se serializa");
    let _ = out_tx.send(PeerOut::Offer(value));
    eprintln!("[net] mandé la oferta");

    // El candidato local va después del SDP: es trickle ICE puro.
    send_local_candidate(current, out_tx);

    Ok(())
}

/// Acepta la oferta del otro y manda la respuesta.
fn accept_offer(
    current: &mut Session,
    sdp: &Value,
    out_tx: &Sender<PeerOut>,
) -> Result<(), RtcError> {
    let offer: SdpOffer = serde_json::from_value(sdp.clone())
        .map_err(|err| RtcError::RemoteSdp(format!("oferta que no se entiende: {err}")))?;

    let answer = current.rtc.sdp_api().accept_offer(offer)?;

    let value = serde_json::to_value(&answer).expect("el SDP se serializa");
    let _ = out_tx.send(PeerOut::Answer(value));
    eprintln!("[net] acepté la oferta y mandé la respuesta");

    // El candidato local va después del SDP: es trickle ICE puro.
    send_local_candidate(current, out_tx);

    Ok(())
}

/// Acepta la respuesta a nuestra oferta.
fn accept_answer(current: &mut Session, sdp: &Value) -> Result<(), RtcError> {
    let answer: SdpAnswer = serde_json::from_value(sdp.clone())
        .map_err(|err| RtcError::RemoteSdp(format!("respuesta que no se entiende: {err}")))?;

    let pending = current.pending.take().ok_or(RtcError::ChangesOutOfOrder)?;

    current.rtc.sdp_api().accept_answer(pending, answer)?;

    eprintln!("[net] acepté la respuesta");

    Ok(())
}

/// Manda el candidato local y lo agrega al `Rtc`.
///
/// Se hace después de armar el SDP a propósito: el candidato viaja como
/// mensaje `ice` (trickle), no embebido en el SDP.
fn send_local_candidate(current: &mut Session, out_tx: &Sender<PeerOut>) {
    let Some(candidate) = current.local.take() else {
        return;
    };

    // `add_local_candidate` le pone el ufrag al candidato: hay que
    // mandar el que devuelve, no el de entrada.
    let Some(stored) = current.rtc.add_local_candidate(candidate) else {
        eprintln!("[net] el candidato local fue rechazado");
        return;
    };

    match serde_json::to_value(stored) {
        Ok(value) => {
            let _ = out_tx.send(PeerOut::Ice(value));
        }
        Err(err) => eprintln!("[net] no se pudo serializar el candidato local: {err}"),
    }
}

/// Escribe un mensaje en el canal de datos.
///
/// Va como texto: es lo que se muestra en la UI del otro equipo.
fn send_message(current: &mut Session, text: &str, sink: &Sink) {
    let Some(id) = current.channel else {
        net::publish_notice(sink, "el canal de datos todavía no está abierto");
        return;
    };

    let Some(mut channel) = current.rtc.channel(id) else {
        net::publish_notice(sink, "el canal de datos no está");
        return;
    };

    match channel.write(false, text.as_bytes()) {
        Ok(true) => {}
        Ok(false) => {
            net::publish_notice(sink, "el canal de datos está lleno");
        }
        Err(err) => {
            eprintln!("[net] no se pudo escribir en el canal: {err}");
            net::publish_notice(sink, format!("no se pudo enviar el mensaje: {err}"));
        }
    }
}

/// Atiende un evento de `str0m`.
fn handle_event(event: Event, current: &mut Session, sink: &Sink) {
    match event {
        Event::Connected => {
            net::publish_state(sink, NetState::Conectado);
        }
        Event::ChannelOpen(id, label) => {
            current.channel = Some(id);
            eprintln!("[net] canal de datos abierto: {label}");
            net::publish_state(sink, NetState::Conectado);
            net::publish_notice(sink, format!("canal de datos abierto: {label}"));
        }
        Event::ChannelData(data) => {
            if data.binary {
                eprintln!("[net] llegó un mensaje binario ({} bytes)", data.data.len());
                net::publish_message(sink, format!("[binario] {} bytes", data.data.len()));
            } else {
                let text = String::from_utf8_lossy(&data.data).to_string();
                eprintln!("[net] llegó un mensaje: {text}");
                net::publish_message(sink, text);
            }
        }
        Event::IceConnectionStateChange(state) => {
            eprintln!("[net] estado del ICE: {state:?}");
        }
        _ => {}
    }
}

/// La dirección donde se abre el socket UDP.
///
/// Primero `SINC_NET_ADDR`, después la primera IPv4 que no sea de
/// loopback y, si no hay ninguna, el loopback.
fn local_address() -> io::Result<SocketAddr> {
    if let Some(ip) = net::local_address_hint() {
        return Ok(SocketAddr::new(ip, 0));
    }

    if let Some(ip) = first_usable_ipv4() {
        return Ok(SocketAddr::new(ip, 0));
    }

    Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
}

/// La primera IPv4 de una interfaz que no sea de loopback.
fn first_usable_ipv4() -> Option<IpAddr> {
    let interfaces = if_addrs::get_if_addrs().ok()?;

    for interface in interfaces {
        if let if_addrs::IfAddr::V4(v4) = interface.addr {
            let ip = v4.ip;
            if !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified() {
                return Some(IpAddr::V4(ip));
            }
        }
    }

    None
}
