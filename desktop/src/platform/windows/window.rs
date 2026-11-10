//! Ajustes específicos de Windows para que la barra se comporte como un widget:
//! sin foco (no interrumpe juegos/apps), fuera de la barra de tareas y
//! esquinada abajo en el centro.

use crate::AppWindow;

use std::ffi::c_void;
use std::time::Duration;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, HWND_TOPMOST, SPI_GETWORKAREA, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetWindowLongPtrW,
    SetWindowPos, SystemParametersInfoW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

/// DWMWA_WINDOW_CORNER_PREFERENCE (`Windows 11`). En Windows 10 la llamada falla y se ignora.
const DWMWA_WINDOW_CORNER_PREFERENCE: DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(33);
const DWMWCP_ROUND: u32 = 2;

/// Margen (px físicos) entre la barra y el borde inferior del área de trabajo.
const BOTTOM_MARGIN: i32 = 24;

const RETRY_MS: u64 = 25;
const MAX_RETRIES: u32 = 200;

/// Aplica los ajustes en cuanto la ventana tiene un HWND de verdad.
pub fn configure(ui: &AppWindow) {
    attempt(ui.as_weak(), 0);
}

/// En Windows se puede animar el cambio de tamaño.
///
/// La posición no la decide el sistema: [`reposition`] la calcula como una
/// función pura del tamaño nativo en cada cuadro, así que los pasos
/// intermedios no dejan error — el último siempre cae exacto en el centro.
pub fn animates_resize() -> bool {
    true
}

fn attempt(weak: slint::Weak<AppWindow>, attempt_n: u32) {
    let Some(ui) = weak.upgrade() else {
        return;
    };

    let hwnd = ui
        .window()
        .window_handle()
        .window_handle()
        .ok()
        .and_then(|h| match h.as_raw() {
            RawWindowHandle::Win32(w) => Some(HWND(w.hwnd.get() as *mut c_void)),
            _ => None,
        });

    if let Some(hwnd) = hwnd {
        apply_styles(hwnd);
        reposition(&ui);
        return;
    }

    if attempt_n < MAX_RETRIES {
        let weak = weak.clone();
        slint::Timer::single_shot(Duration::from_millis(RETRY_MS), move || {
            attempt(weak, attempt_n + 1);
        });
    } else {
        eprintln!("[ventana] no se pudo obtener el HWND; se deja el estilo normal");
    }
}

fn apply_styles(hwnd: HWND) {
    // `SINC_ACTIVATABLE=1` permite quitar el "no robar foco" para depurar.
    let no_activate = std::env::var_os("SINC_ACTIVATABLE").is_none();

    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let mut updated = style | WS_EX_TOOLWINDOW.0 as isize;
        if no_activate {
            updated |= WS_EX_NOACTIVATE.0 as isize;
        }
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, updated);

        if let Err(err) = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        ) {
            eprintln!("[ventana] SetWindowPos falló: {err}");
        }

        let preference: u32 = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &preference as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );

        eprintln!("[ventana] estilos aplicados (topmost, toolwindow, no-activate={no_activate})");
    }
}

/// Reancla la ventana abajo al centro del área de trabajo.
///
/// Se llama al arrancar y cada vez que cambia el tamaño (al achicarse al
/// círculo, al abrir Controles o al mostrar un aviso). Usa el tamaño *real*
/// de la ventana y no el que pide la animación: el backend redimensiona de
/// forma asíncrona, así que el valor animado y el nativo se desfasaron un
/// cuadro durante toda la transición. Centrar con el valor animado dejaba la
/// posición calculada con un ancho que la ventana todavía no tenía, y cada
/// ciclo volvía a correr el ancla unos píxeles. Con el tamaño real, la
/// posición es una función pura del tamaño que se ve: repetir el cálculo es
/// idempotente y no se acumula ningún error.
pub fn reposition(ui: &AppWindow) {
    let mut area = RECT::default();
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut area as *mut RECT as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() {
        return;
    }

    // Tamaño físico real de la ventana: es el que está en pantalla en este
    // momento y el que va a ver el compositor.
    let size = ui.window().size();
    let width = size.width as i32;
    let height = size.height as i32;

    let available = area.right - area.left;
    let x = area.left + (available - width) / 2;
    let y = area.bottom - height - BOTTOM_MARGIN;

    // Durante la animación se llama una vez por cuadro; si la cuenta da la
    // misma posición, no hay nada que mover.
    let current = ui.window().position();
    if current.x == x && current.y == y {
        return;
    }

    ui.window().set_position(slint::PhysicalPosition::new(x, y));
}
