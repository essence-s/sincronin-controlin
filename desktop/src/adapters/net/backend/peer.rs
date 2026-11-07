//! El hilo de par: el `Rtc` de str0m y el socket UDP.
//!
//! Es el único que toca la conexión punto a punto. El bucle es el
//! canónico de `str0m`: se le pide que produzca una salida, se manda lo
//! que pida y se le pasa lo que llega del socket.
//!
//! ## El candidato local
//!
//! Se anuncian **dos, a partir de un mismo socket**: host (la dirección
//! del socket) y server reflexive (la dirección pública que el STUN
//! revela, para cruzar un NAT). El socket se abre en la dirección de red
//! que toca —el agente ICE descarta los paquetes STUN que llegan a una
//! dirección que no sea uno de sus candidatos, y un socket en `0.0.0.0`
//! no sirve— y el paquete que llega a la dirección pública lo reenvía el
//! NAT hacia acá. Anunciar varias interfaces requiere un socket por cada
//! una: eso queda para otra versión.
//!
//! ## El STUN
//!
//! Al abrir la sesión, sale un binding request por el mismo socket hacia
//! el servidor STUN (`SINC_STUN_ADDR`). La respuesta trae la dirección
//! mapeada, de la que sale el candidato server reflexive: se agrega al
//! `Rtc` y se anuncia por trickle, con la oferta o la respuesta si ya
//! llegaron, o en el momento si no. Sin servidor (`SINC_STUN_ADDR` vacío)
//! o sin respuesta, queda solo el host: en la propia red alcanza, y a
//! través de un NAT simétrico no habría camino (para eso haría falta
//! TURN, que no se usa). Sin respuesta igual queda el aviso en el
//! registro del panel.
//!
//! ## Un par por equipo
//!
//! El controlado sostiene un par por cada controlador de su sala, cada uno
//! con su socket y su STUN. El controlador tiene uno solo, contra el
//! controlado: solo el controlado ofrece, así que dos controladores nunca
//! abren un par entre sí.
//!
//! ## Lo que cuenta el ICE
//!
//! El estado del ICE también desembolsa un aviso del panel: si la
//! conexión se interrumpe, sale escrito, aunque la etiqueta global
//! ("Esperando al otro equipo" / "Conectado") no cambie, porque el
//! rehacer suele ser pasajero. Si la conexión vuelve, sale el aviso de
//! la vuelta.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use rand::Rng;
use serde_json::Value;
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcError};

use crate::adapters::net::backend::{self as net, NetState, PeerCommand, PeerOut, Sink};

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

/// El puerto STUN cuando la dirección no lo trae.
const STUN_PORT: u16 = 19302;

/// Cuánto pasa entre reenvíos del binding STUN.
const STUN_RETRY: Duration = Duration::from_millis(500);

/// Cuántos reenvíos del binding antes de rendirse con solo el host.
const STUN_ATTEMPTS: usize = 6;

/// El tipo del binding request del RFC 5389.
pub(crate) const STUN_REQUEST_TYPE: u16 = 0x0001;

/// El tipo del binding de éxito del RFC 5389.
const STUN_SUCCESS_TYPE: u16 = 0x0101;

/// El atributo que trae la dirección mapeada, con XOR (RFC 5389 15.2).
const STUN_MAPPED_ATTR: u16 = 0x0020;

/// La magic cookie del RFC 5389.
pub(crate) const STUN_MAGIC: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];

/// El par con un equipo: el controlado sostiene uno por controlador.
struct Session {
    /// El equipo con el que es este par.
    peer: u64,
    /// El estado de WebRTC.
    rtc: Rtc,
    /// El socket UDP.
    socket: UdpSocket,
    /// La oferta que mandamos, esperando la respuesta.
    pending: Option<SdpPendingOffer>,
    /// El canal de datos, cuando se abre.
    channel: Option<ChannelId>,
    /// El binding STUN en curso, si hay servidor configurado.
    stun: Option<StunQuery>,
    /// Los candidatos locales que faltan anunciar.
    ///
    /// El host entra al abrir la sesión; el server reflexive si el STUN
    /// contesta.
    local: Vec<Candidate>,
    /// Si ya se anunció lo que había tras el SDP: los candidatos que
    /// llegan después (un srflx a destiempo) se anuncian al momento.
    trickled: bool,
    /// Si ya mandamos una oferta: para no mandar dos.
    offered: bool,
    /// Si ya avisó que la conexión se interrumpió.
    ///
    /// La interrupción del ICE puede ser pasajera: el aviso marca para
    /// que la vuelta, si llega, también avise, y así el panel no queda
    /// con una falla que ya no vale.
    dropped: bool,
}

/// El binding STUN en curso contra el servidor.
struct StunQuery {
    /// La dirección del servidor.
    server: SocketAddr,
    /// El request ya serializado: cada reenvío lo manda igual, con el
    /// mismo trans id.
    request: Vec<u8>,
    /// Cuándo se mandó el último intento.
    sent_at: Instant,
    /// Cuántos intentos van.
    attempts: usize,
}

/// El hilo de par.
pub(crate) fn run(
    cmd_rx: Receiver<PeerCommand>,
    out_tx: Sender<PeerOut>,
    sink: Sink,
) -> Result<(), RtcError> {
    let mut sessions: HashMap<u64, Session> = HashMap::new();
    let mut buf: Vec<u8> = Vec::new();

    loop {
        // 1) Los comandos que ya están esperando.
        while let Ok(cmd) = cmd_rx.try_recv() {
            if !handle_command(cmd, &mut sessions, &out_tx, &sink) {
                break;
            }
        }

        // 2) Sin pares no hay nada que hacer: esperar un comando.
        if sessions.is_empty() {
            match cmd_rx.recv_timeout(IDLE_WAIT) {
                Ok(cmd) => {
                    handle_command(cmd, &mut sessions, &out_tx, &sink);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            continue;
        }

        // 3) La salida de cada par: el binding STUN y el bucle de str0m.
        let mut earliest = Instant::now() + MAX_WAIT;
        let mut broken: Vec<u64> = Vec::new();

        for current in sessions.values_mut() {
            // El binding STUN se reenvía si le toca: no depende de str0m.
            maintain_stun(current, &sink);

            match pump(current, &sink) {
                Ok(at) => earliest = earliest.min(at),
                Err(err) => {
                    eprintln!(
                        "[net] error de WebRTC con el equipo {}: {err}",
                        current.peer
                    );
                    broken.push(current.peer);
                }
            }
        }

        for peer in broken {
            sessions.remove(&peer);
        }
        if sessions.is_empty() {
            continue;
        }

        // 4) Un datagrama de cada socket, con un plazo corto.
        //
        // Con varios pares no se puede esperar en uno solo: el plazo se
        // reparte entre todos, y así ninguno se queda con el hilo mientras
        // los demás tienen algo para leer.
        let wait = earliest.saturating_duration_since(Instant::now());
        let slice = (wait / sessions.len() as u32)
            .min(MAX_WAIT)
            .max(Duration::from_millis(1));

        let mut failed: Vec<u64> = Vec::new();
        for current in sessions.values_mut() {
            if let Err(err) = read_once(current, slice, &mut buf, &out_tx, &sink) {
                eprintln!(
                    "[net] error en el socket del equipo {}: {err}",
                    current.peer
                );
                failed.push(current.peer);
            }
        }
        for peer in failed {
            sessions.remove(&peer);
        }
    }

    Ok(())
}

/// Drena la salida de str0m y devuelve cuándo hay que despertarlo.
///
/// Si el instante que pide ya pasó, le pasa el timeout y vuelve a
/// preguntar: cada par avanza con su propio reloj.
fn pump(current: &mut Session, sink: &Sink) -> Result<Instant, RtcError> {
    loop {
        match current.rtc.poll_output()? {
            Output::Timeout(at) => {
                let now = Instant::now();
                if at > now {
                    return Ok(at);
                }
                current.rtc.handle_input(Input::Timeout(now))?;
            }
            Output::Transmit(transmit) => {
                if let Err(err) = current
                    .socket
                    .send_to(&transmit.contents, transmit.destination)
                {
                    eprintln!("[net] no se pudo mandar el paquete: {err}");
                }
            }
            Output::Event(event) => handle_event(event, current, sink),
        }
    }
}

/// Espera un datagrama en el socket del par y se lo pasa a str0m.
///
/// Si el plazo se cumple sin nada, no hay nada que avisarle: el `pump` de
/// la vuelta siguiente ve si ya le tocaba despertarse.
fn read_once(
    current: &mut Session,
    wait: Duration,
    buf: &mut Vec<u8>,
    out_tx: &Sender<PeerOut>,
    sink: &Sink,
) -> Result<(), RtcError> {
    current.socket.set_read_timeout(Some(wait))?;

    // El buffer tiene que estar listo antes de recibir: con un buffer
    // vacío el datagram se trunca a cero bytes.
    buf.resize(BUFFER_SIZE, 0);

    let input = match current.socket.recv_from(buf) {
        Ok((n, source)) => {
            buf.truncate(n);

            // Si es la respuesta a nuestro binding STUN, la maneja el
            // hilo de par: no llega al `Rtc`.
            if handle_stun_reply(current, source, buf, out_tx, sink) {
                return Ok(());
            }

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
        // Se cumplio el timeout del socket: no hay nada para str0m.
        Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };

    current.rtc.handle_input(input)?;

    Ok(())
}

/// Reenvía o rinde el binding STUN si le toca.
///
/// No es polling que consulta nada: solo un reenvío del mismo request
/// cuando venció su intervalo, como retransmite ICE.
fn maintain_stun(current: &mut Session, sink: &Sink) {
    let Some(mut query) = current.stun.take() else {
        return;
    };

    if query.sent_at.elapsed() < STUN_RETRY {
        current.stun = Some(query);
        return;
    }

    if query.attempts >= STUN_ATTEMPTS {
        eprintln!("[net] el servidor STUN no contesta: queda solo el candidato host");
        net::publish_notice(
            sink,
            "el servidor STUN no contesta: sin dirección pública no hay camino a través del NAT",
        );
        // `query` queda consumido: el binding ya no corre más.
        return;
    }

    query.attempts += 1;
    query.sent_at = Instant::now();

    if let Err(err) = current.socket.send_to(&query.request, query.server) {
        eprintln!("[net] no se pudo reenviar el binding STUN: {err}");
    }

    current.stun = Some(query);
}

/// Atiende un paquete que puede ser la respuesta a nuestro binding STUN.
///
/// Devuelve `true` si lo era: entonces lo maneja acá y no se le pasa a
/// str0m. Si no, sigue de largo por el camino de siempre.
fn handle_stun_reply(
    current: &mut Session,
    source: SocketAddr,
    buf: &[u8],
    out_tx: &Sender<PeerOut>,
    sink: &Sink,
) -> bool {
    let Some(query) = current.stun.as_ref() else {
        return false;
    };
    if source != query.server {
        return false;
    }

    let Some(reply) = parse_stun_reply(buf, &query.request) else {
        return false;
    };

    // Es la respuesta que esperábamos: cierra la transacción, para no
    // volver a retransmitir ni tratar de leer una segunda contestación.
    current.stun = None;

    match reply {
        StunReply::Mapped(mapped) => {
            eprintln!("[net] STUN: el NAT nos mapea a {mapped}");
            net::publish_notice(
                sink,
                format!("dirección pública detectada por STUN: {mapped}"),
            );

            let base = match current.socket.local_addr() {
                Ok(base) => base,
                Err(err) => {
                    eprintln!("[net] no se pudo leer la dirección del socket: {err}");
                    return true;
                }
            };

            let candidate = match Candidate::server_reflexive(mapped, base, "udp") {
                Ok(candidate) => candidate,
                Err(err) => {
                    eprintln!("[net] no se pudo armar el candidato srflx: {err}");
                    return true;
                }
            };

            if current.trickled {
                // El SDP ya va por señalización: se anuncia solo.
                add_and_send_local_candidate(current, candidate, out_tx);
            } else {
                current.local.push(candidate);
            }
        }
        StunReply::Broken => eprintln!("[net] la respuesta del servidor STUN no sirvió"),
    }

    true
}

/// Lo que contestó el binding STUN.
pub(crate) enum StunReply {
    /// El binding anduvo: esta es la dirección mapeada.
    Mapped(SocketAddr),
    /// El binding no sirvió, o no trajo dirección mapeada.
    Broken,
}

/// Parsea la respuesta del servidor STUN.
///
/// El codec de `str0m::ice` es el de ICE: exige MESSAGE-INTEGRITY en
/// cualquier binding, y los servidores STUN públicos contestan sin ella.
/// Se parsea a mano lo mínimo del RFC 5389: el encabezado y el atributo
/// de la dirección mapeada.
///
/// Devuelve `None` si el paquete no es la respuesta a nuestro binding:
/// entonces sigue de largo hacia el `Rtc`.
pub(crate) fn parse_stun_reply(buf: &[u8], request: &[u8]) -> Option<StunReply> {
    // Encabezado: tipo (2), largo (2), magic (4), trans id (12).
    if buf.len() < 20 {
        return None;
    }
    if buf[4..8] != STUN_MAGIC {
        return None;
    }
    if request.len() < 20 || buf[8..20] != request[8..20] {
        return None;
    }

    let typ = ((buf[0] as u16 & 0b0011_1111) << 8) | buf[1] as u16;
    if typ != STUN_SUCCESS_TYPE {
        return Some(StunReply::Broken);
    }

    // Los atributos: se recorren hasta el largo declarado del paquete.
    let declared = 20 + u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let end = declared.min(buf.len());

    let mut i = 20;
    while i + 4 <= end {
        let attr = u16::from_be_bytes([buf[i], buf[i + 1]]);
        let attr_len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;

        if i + 4 + attr_len > end {
            break;
        }
        if attr == STUN_MAPPED_ATTR && (attr_len == 8 || attr_len == 20) {
            return Some(mapped_from_attr(&buf[i + 4..i + 4 + attr_len]));
        }

        i += 4 + attr_len;
    }

    Some(StunReply::Broken)
}

/// Decodifica el atributo XOR-MAPPED-ADDRESS.
///
/// El puerto se desxorea con los dos primeros bytes de la magic, y la IP
/// byte a byte con los cuatro.
fn mapped_from_attr(attr: &[u8]) -> StunReply {
    let port_xor = u16::from_be_bytes([STUN_MAGIC[0], STUN_MAGIC[1]]);
    let port = u16::from_be_bytes([attr[2], attr[3]]) ^ port_xor;

    match attr[1] {
        // IPv4.
        0x01 => {
            let a = attr[4] ^ STUN_MAGIC[0];
            let b = attr[5] ^ STUN_MAGIC[1];
            let c = attr[6] ^ STUN_MAGIC[2];
            let d = attr[7] ^ STUN_MAGIC[3];

            let ip = IpAddr::V4(Ipv4Addr::new(a, b, c, d));
            StunReply::Mapped(SocketAddr::new(ip, port))
        }
        // IPv6: la magic se repite cada 4 bytes.
        0x02 if attr.len() >= 20 => {
            let octets: [u8; 16] = std::array::from_fn(|i| attr[4 + i] ^ STUN_MAGIC[i % 4]);

            let ip = IpAddr::V6(octets.into());
            StunReply::Mapped(SocketAddr::new(ip, port))
        }
        _ => StunReply::Broken,
    }
}

/// Atiende un comando del hilo de señalización.
///
/// Devuelve `false` si hay que dejar de drenar comandos: después de
/// escribir en el canal de datos hay que llamar a `poll_output` antes de
/// escribir otra vez.
fn handle_command(
    cmd: PeerCommand,
    sessions: &mut HashMap<u64, Session>,
    out_tx: &Sender<PeerOut>,
    sink: &Sink,
) -> bool {
    match cmd {
        PeerCommand::MakeOffer { peer } => {
            let Some(current) = session_for(sessions, peer, sink) else {
                return true;
            };

            if current.offered {
                // Ya se ofrecio: no se ofrece dos veces.
                return true;
            }
            current.offered = true;

            if let Err(err) = make_offer(current, out_tx) {
                eprintln!("[net] no se pudo hacer la oferta: {err}");
                net::publish_notice(sink, format!("no se pudo hacer la oferta: {err}"));
            }
            true
        }
        PeerCommand::AcceptOffer { peer, sdp } => {
            let Some(current) = session_for(sessions, peer, sink) else {
                return true;
            };

            if let Err(err) = accept_offer(current, &sdp, out_tx) {
                eprintln!("[net] no se pudo aceptar la oferta: {err}");
                net::publish_notice(sink, format!("no se pudo aceptar la oferta: {err}"));
            }
            true
        }
        PeerCommand::AcceptAnswer { peer, sdp } => {
            let Some(current) = sessions.get_mut(&peer) else {
                return true;
            };

            if let Err(err) = accept_answer(current, &sdp) {
                eprintln!("[net] no se pudo aceptar la respuesta: {err}");
                net::publish_notice(sink, format!("no se pudo aceptar la respuesta: {err}"));
            }
            true
        }
        PeerCommand::AddIce { peer, candidate } => {
            let Some(current) = sessions.get_mut(&peer) else {
                return true;
            };

            match serde_json::from_value::<Candidate>(candidate) {
                Ok(candidate) => {
                    current.rtc.add_remote_candidate(candidate);
                }
                Err(err) => {
                    eprintln!("[net] candidato ICE que no se entiende: {err}");
                }
            }
            true
        }
        PeerCommand::Send(text) => {
            for current in sessions.values_mut() {
                send_message(current, &text, sink);
            }
            // Después de escribir hay que pasar por `poll_output` antes
            // de escribir otra vez.
            false
        }
        PeerCommand::Close { peer } => {
            // Se cae el par de ese equipo: su socket se cierra y los demás
            // siguen como estaban.
            sessions.remove(&peer);
            true
        }
        PeerCommand::Stop => {
            // Se sale de la sala: se caen todos los pares.
            sessions.clear();
            true
        }
    }
}

/// El par con un equipo; si todavía no hay, lo abre.
fn session_for<'a>(
    sessions: &'a mut HashMap<u64, Session>,
    peer: u64,
    sink: &Sink,
) -> Option<&'a mut Session> {
    match sessions.entry(peer) {
        Entry::Occupied(current) => Some(current.into_mut()),
        Entry::Vacant(slot) => {
            eprintln!("[net] se abre el par con el equipo {peer}");

            match start(peer) {
                Ok(new) => Some(slot.insert(new)),
                Err(err) => {
                    eprintln!("[net] no se pudo abrir el socket: {err}");
                    net::publish_notice(sink, format!("no se pudo abrir el socket UDP: {err}"));
                    None
                }
            }
        }
    }
}

/// Abre el socket UDP y prepara el `Rtc`.
fn start(peer: u64) -> io::Result<Session> {
    let socket = UdpSocket::bind(local_address()?)?;
    let local_addr = socket.local_addr()?;

    let rtc = Rtc::new(Instant::now());
    let host = Candidate::host(local_addr, "udp")
        .map_err(|err| io::Error::other(format!("no se pudo crear el candidato local: {err}")))?;

    eprintln!("[net] socket UDP en {local_addr}");

    let stun = start_stun(&socket, local_addr);

    Ok(Session {
        peer,
        rtc,
        socket,
        pending: None,
        channel: None,
        stun,
        local: vec![host],
        trickled: false,
        offered: false,
        dropped: false,
    })
}

/// Arma y manda el primer binding STUN, si hay servidor configurado.
///
/// Sale por el mismo socket que la sesión: la dirección mapeada que
/// devuelve el servidor sirve como candidato del par ICE, porque es hacia
/// donde se va el tráfico real.
fn start_stun(socket: &UdpSocket, local_addr: SocketAddr) -> Option<StunQuery> {
    let server_name = net::stun_server()?;

    let Some(server) = resolve_stun(&server_name, local_addr.is_ipv4()) else {
        eprintln!("[net] no se pudo resolver el servidor STUN: {server_name}");
        return None;
    };

    let request = new_stun_request();
    if let Err(err) = socket.send_to(&request, server) {
        eprintln!("[net] no se pudo mandar el binding al servidor STUN: {err}");
        return None;
    }

    eprintln!("[net] binding STUN a {server}");

    Some(StunQuery {
        server,
        request,
        sent_at: Instant::now(),
        attempts: 1,
    })
}

/// Arma el request del binding del RFC 5389: encabezado de 20 bytes, un
/// trans id al azar y ningún atributo.
///
/// El codec de `str0m::ice` solo habla de ICE: exige MESSAGE-INTEGRITY y
/// USERNAME en los bindings, y un servidor STUN público no los espera.
/// Este es el mínimo que se entiende con cualquiera.
fn new_stun_request() -> Vec<u8> {
    let mut request = [0u8; 20];

    request[0..2].copy_from_slice(&STUN_REQUEST_TYPE.to_be_bytes());
    request[4..8].copy_from_slice(&STUN_MAGIC);

    let mut rng = rand::rng();
    for byte in &mut request[8..20] {
        *byte = rng.random::<u8>();
    }

    request.to_vec()
}

/// Arma la respuesta de binding de éxito del RFC 5389: encabezado de 20
/// bytes y, como único atributo, la XOR-MAPPED-ADDRESS.
///
/// Aca responde el servidor STUN de las pruebas, que remapea la dirección
/// del que preguntó; el cliente tiene que entenderla con `parse_stun_reply`.
#[cfg(test)]
pub(crate) fn binding_success_reply(request: &[u8], mapped: SocketAddr) -> Vec<u8> {
    let mut reply = [0u8; 32];

    reply[0..2].copy_from_slice(&STUN_SUCCESS_TYPE.to_be_bytes());
    // Sin fingerprint, no hace falta: el parser del cliente no lo mira.
    reply[2..4].copy_from_slice(&(12u16).to_be_bytes());
    reply[4..8].copy_from_slice(&STUN_MAGIC);
    // El trans id de la respuesta es el del request.
    reply[8..20].copy_from_slice(&request[8..20]);

    reply[20..22].copy_from_slice(&STUN_MAPPED_ATTR.to_be_bytes());
    reply[22..24].copy_from_slice(&(8u16).to_be_bytes());

    // Familia IPv4, y los bytes desxoreados con la porta y la IP.
    reply[25] = 0x01;

    let port_xor = u16::from_be_bytes([STUN_MAGIC[0], STUN_MAGIC[1]]);
    reply[26..28].copy_from_slice(&(mapped.port() ^ port_xor).to_be_bytes());

    if let IpAddr::V4(ip) = mapped.ip() {
        let octets = ip.octets();
        reply[28] = octets[0] ^ STUN_MAGIC[0];
        reply[29] = octets[1] ^ STUN_MAGIC[1];
        reply[30] = octets[2] ^ STUN_MAGIC[2];
        reply[31] = octets[3] ^ STUN_MAGIC[3];
    }

    reply.to_vec()
}

/// Resuelve la dirección del servidor STUN.
///
/// Toma una de la misma familia de IP que el socket, porque el binding
/// sale por ese socket.
fn resolve_stun(server: &str, ipv4: bool) -> Option<SocketAddr> {
    let target = if server.contains(':') {
        server.to_string()
    } else {
        format!("{server}:{STUN_PORT}")
    };

    let addrs: Vec<SocketAddr> = target.to_socket_addrs().ok()?.collect();
    addrs.into_iter().find(|addr| addr.is_ipv4() == ipv4)
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
    let _ = out_tx.send(PeerOut::Offer {
        to: current.peer,
        sdp: value,
    });
    eprintln!("[net] mandé la oferta");

    // Los candidatos van después del SDP: es trickle ICE puro.
    trickle_local_candidates(current, out_tx);

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
    let _ = out_tx.send(PeerOut::Answer {
        to: current.peer,
        sdp: value,
    });
    eprintln!("[net] acepté la oferta y mandé la respuesta");

    // Los candidatos van después del SDP: es trickle ICE puro.
    trickle_local_candidates(current, out_tx);

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

/// Anuncia los candidatos locales listos.
///
/// Se hace después de armar el SDP a propósito: los candidatos viajan como
/// mensajes `ice` (trickle), no embebidos en el SDP. También queda
/// marcado: si llega otro candidato (el srflx del STUN) se anuncia solo.
fn trickle_local_candidates(current: &mut Session, out_tx: &Sender<PeerOut>) {
    let queued = std::mem::take(&mut current.local);

    for candidate in queued {
        add_and_send_local_candidate(current, candidate, out_tx);
    }

    current.trickled = true;
}

/// Agrega el candidato al `Rtc` y lo manda por la señalización.
///
/// `add_local_candidate` le pone el ufrag al candidato: hay que mandar el
/// que devuelve, no el de entrada. Si el `Rtc` lo rechaza (por redundante,
/// por ejemplo) no viaja.
fn add_and_send_local_candidate(
    current: &mut Session,
    candidate: Candidate,
    out_tx: &Sender<PeerOut>,
) {
    let Some(stored) = current.rtc.add_local_candidate(candidate) else {
        eprintln!("[net] el candidato local fue rechazado");
        return;
    };

    match serde_json::to_value(stored) {
        Ok(value) => {
            let _ = out_tx.send(PeerOut::Ice {
                to: current.peer,
                candidate: value,
            });
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

            match state {
                // La interrupción puede ser pasajera (el agente de ICE se
                // rehace solo): no toca la etiqueta global del panel, solo
                // deja escrito el aviso, y si la conexión vuelve, otro.
                IceConnectionState::Disconnected => {
                    current.dropped = true;
                    net::publish_notice(sink, "la conexión con el otro equipo falló");
                }
                IceConnectionState::Connected | IceConnectionState::Completed
                    if current.dropped =>
                {
                    current.dropped = false;
                    net::publish_state(sink, NetState::Conectado);
                    net::publish_notice(sink, "la conexión con el otro equipo quedó establecida");
                }
                _ => {}
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    /// Sink que anota los eventos para inspección.
    fn test_sink() -> (Sink, Arc<Mutex<Vec<net::Event>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&events);
        let sink: Sink = Arc::new(move |event| {
            recorded
                .lock()
                .expect("el registro no está poisoned")
                .push(event);
        });

        (sink, events)
    }

    /// Una sesión despojada: socket y `Rtc` apenas nacidos.
    ///
    /// Alcanza para los avisos: no hay conversación con el otro equipo,
    /// solo eventos de ICE y la rendición del binding STUN.
    fn bare_session(stun: Option<StunQuery>) -> Session {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("socket de pruebas");

        Session {
            peer: 0,
            rtc: Rtc::new(Instant::now()),
            socket,
            pending: None,
            channel: None,
            stun,
            local: vec![],
            trickled: false,
            offered: false,
            dropped: false,
        }
    }

    /// La respuesta de un binding parsea, y trae la dirección mapeada.
    #[test]
    fn the_stun_reply_carries_the_mapped_address() {
        let request = new_stun_request();

        // Un servidor STUN remapea la dirección del que preguntó: misma
        // IP y otro puerto, como nuestro servidor de pruebas.
        let mapped = SocketAddr::new("192.168.1.8".parse().expect("ip"), 53_000);
        let reply = binding_success_reply(&request, mapped);

        let StunReply::Mapped(parsed) =
            parse_stun_reply(&reply, &request).expect("es la respuesta")
        else {
            panic!("no se entendió la dirección mapeada");
        };

        assert_eq!(parsed, mapped);
    }

    /// Un paquete que no responde a nuestro binding no se interpreta.
    #[test]
    fn a_stun_reply_of_another_transaction_is_ignored() {
        let request = new_stun_request();
        let other = new_stun_request();

        let mapped = SocketAddr::new("192.168.1.8".parse().expect("ip"), 53_000);
        let reply = binding_success_reply(&other, mapped);

        assert!(parse_stun_reply(&reply, &request).is_none());
    }

    /// El ICE interrumpido avisa, sin tocar la etiqueta global: la
    /// interrupción puede ser pasajera.
    #[test]
    fn the_ice_drop_notices_without_touching_the_state() {
        let (sink, events) = test_sink();
        let mut session = bare_session(None);

        // Que esté Diseñando no es una interrupción: ni aviso.
        handle_event(
            Event::IceConnectionStateChange(IceConnectionState::Checking),
            &mut session,
            &sink,
        );
        assert!(
            events
                .lock()
                .expect("el registro no está poisoned")
                .is_empty(),
            "el Checking no hace ruido en el panel"
        );

        handle_event(
            Event::IceConnectionStateChange(IceConnectionState::Disconnected),
            &mut session,
            &sink,
        );

        let recorded = events.lock().expect("el registro no está poisoned").clone();
        assert_eq!(recorded.len(), 1, "solo el aviso: {recorded:?}");
        let net::Event::Notice(text) = &recorded[0] else {
            panic!("no fue un aviso: {recorded:?}");
        };
        assert!(
            text.contains("falló"),
            "el aviso no aclara la falla: {text}"
        );

        // Sin estado global: la interrupción no cambia la etiqueta.
        assert!(
            recorded
                .iter()
                .all(|event| !matches!(event, net::Event::State(_))),
            "la interrupción no toca la etiqueta: {recorded:?}"
        );
        assert!(session.dropped, "quedó marcada la caída para la vuelta");
    }

    /// Si el ICE se rehace, avisa y vuelve a conectar la etiqueta.
    #[test]
    fn the_ice_recovery_notices_after_a_drop() {
        let (sink, events) = test_sink();
        let mut session = bare_session(None);
        session.dropped = true;

        handle_event(
            Event::IceConnectionStateChange(IceConnectionState::Completed),
            &mut session,
            &sink,
        );

        let recorded = events.lock().expect("el registro no está poisoned").clone();
        assert!(
            recorded.iter().any(
                |event| matches!(event, net::Event::Notice(text) if text.contains("establecida"))
            ),
            "falta el aviso de la vuelta: {recorded:?}"
        );
        assert!(
            recorded
                .iter()
                .any(|event| matches!(event, net::Event::State(net::NetState::Conectado))),
            "falta volver a conectar la etiqueta: {recorded:?}"
        );
        assert!(!session.dropped, "la caída ya no vale");
    }

    /// La primera vez que conecta no genera aviso nuevo: de eso ya se
    /// ocupa el canal de datos abierto.
    #[test]
    fn a_recovery_without_a_drop_stays_quiet() {
        let (sink, events) = test_sink();
        let mut session = bare_session(None);

        handle_event(
            Event::IceConnectionStateChange(IceConnectionState::Connected),
            &mut session,
            &sink,
        );

        assert!(
            events
                .lock()
                .expect("el registro no está poisoned")
                .is_empty(),
            "la conexión de primera línea no hace ruido por el ICE"
        );
        assert!(!session.dropped);
    }

    /// El servidor STUN mudo se rinde, con su aviso en el panel.
    #[test]
    fn a_silent_stun_server_gives_up_with_a_notice() {
        let (sink, events) = test_sink();

        // La consulta ya gastó todos sus intentos hace más de un intervalo.
        let query = StunQuery {
            server: "127.0.0.1:9".parse().expect("dirección"),
            request: vec![0u8; 20],
            sent_at: Instant::now() - STUN_RETRY - Duration::from_secs(1),
            attempts: STUN_ATTEMPTS,
        };
        let mut session = bare_session(Some(query));

        maintain_stun(&mut session, &sink);

        // Se rinde: la transacción no corre más.
        assert!(session.stun.is_none());

        let recorded = events.lock().expect("el registro no está poisoned").clone();
        assert_eq!(recorded.len(), 1, "solo el aviso: {recorded:?}");
        let net::Event::Notice(text) = &recorded[0] else {
            panic!("no fue un aviso: {recorded:?}");
        };
        assert!(
            text.contains("STUN no contesta"),
            "el aviso no explica la causa: {text}"
        );
    }
}
