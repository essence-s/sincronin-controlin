//! Vigilancia de cambios de red en Linux: netlink.
//!
//! El kernel manda un datagrama cada vez que un enlace se da de alta, de
//! baja o cambia; no hay que consultar nada. Se escuchan los cambios de
//! enlace y los de dirección IPv4/IPv6 (DHCP o SLAAC pueden darle otra
//! dirección a una interfaz que sigue existiendo), y cada aviso se reenvía
//! al hilo que junta la ráfaga.

use std::io;

use crossbeam_channel::Sender;

/// Bytes del datagrama de netlink. Un aviso de enlace entra de sobra.
const BUFFER_SIZE: usize = 4096;

/// Levanta el hilo que escucha netlink y avisa por el canal.
pub(crate) fn spawn_network_source(tx: Sender<()>) {
    std::thread::Builder::new()
        .name("net-watch-netlink".to_string())
        .spawn(move || {
            let Some(fd) = open_netlink() else {
                return;
            };

            let mut buf = [0u8; BUFFER_SIZE];

            loop {
                // Bloqueante: el kernel despierta solo cuando hay un aviso.
                let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };

                if n < 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    eprintln!("[net] se cortó el aviso de cambios de red: {err}");
                    break;
                }

                if tx.send(()).is_err() {
                    break;
                }
            }

            unsafe { libc::close(fd) };
        })
        .expect("no se pudo crear el hilo de netlink");
}

/// Abre el socket de netlink y se suscribe a los cambios de enlace.
fn open_netlink() -> Option<libc::c_int> {
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    };
    if fd < 0 {
        eprintln!(
            "[net] no se pudo abrir netlink: {}",
            io::Error::last_os_error()
        );
        return None;
    }

    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    // Enlaces (altas, bajas y cambios) y direcciones IPv4/IPv6: una
    // interfaz puede seguir existiendo y cambiar de dirección (DHCP,
    // SLAAC), y eso también es un camino nuevo.
    addr.nl_groups =
        (libc::RTMGRP_LINK | libc::RTMGRP_IPV4_IFADDR | libc::RTMGRP_IPV6_IFADDR) as u32;

    let len = std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t;
    let rc = unsafe {
        libc::bind(
            fd,
            &addr as *const libc::sockaddr_nl as *const libc::sockaddr,
            len,
        )
    };
    if rc < 0 {
        eprintln!(
            "[net] no se pudo escuchar netlink: {}",
            io::Error::last_os_error()
        );
        unsafe { libc::close(fd) };
        return None;
    }

    eprintln!("[net] escucho los cambios de red por netlink");
    Some(fd)
}
