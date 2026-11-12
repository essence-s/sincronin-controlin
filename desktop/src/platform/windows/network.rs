//! Vigilancia de cambios de red en Windows: `NotifyIpInterfaceChange`.
//!
//! El sistema avisa por callback cada vez que una interfaz se da de alta,
//! de baja o cambia. El callback no hace trabajo: solo deja el aviso en el
//! canal que junta la ráfaga.

use std::ffi::c_void;

use crossbeam_channel::Sender;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::IpHelper::{
    MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE, NotifyIpInterfaceChange,
};
use windows::Win32::Networking::WinSock::AF_UNSPEC;

/// Registra la notificación del sistema.
pub(crate) fn spawn_network_source(tx: Sender<()>) {
    // El contexto del callback, que el sistema guarda mientras la
    // notificación esté registrada. Va al heap para que la dirección no
    // cambie al mover el `Sender`.
    let context = Box::into_raw(Box::new(tx));
    let mut handle = HANDLE::default();

    let result = unsafe {
        NotifyIpInterfaceChange(
            AF_UNSPEC,
            Some(notify),
            Some(context.cast()),
            false,
            &mut handle,
        )
    };

    if result.is_err() {
        eprintln!(
            "[net] no se pudo registrar el aviso de cambios de red: {}",
            result.0
        );
        // No quedó registrada: el contexto no lo usa nadie.
        unsafe { drop(Box::from_raw(context)) };
        return;
    }

    eprintln!("[net] escucho los cambios de red por NotifyIpInterfaceChange");
}

/// El callback del sistema: solo deja el aviso en el canal.
unsafe extern "system" fn notify(
    context: *const c_void,
    _row: *const MIB_IPINTERFACE_ROW,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    if context.is_null() {
        return;
    }

    // SAFETY: el puntero es el `Box<Sender<()>>` que sigue vivo mientras
    // la notificación esté registrada.
    let tx = unsafe { &*context.cast::<Sender<()>>() };
    let _ = tx.send(());
}
