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
        position_bottom_center(&ui);
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

fn position_bottom_center(ui: &AppWindow) {
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

    let mut window_size = ui.window().size();
    if window_size.width == 0 || window_size.height == 0 {
        // Todavía no hay tamaño real: usamos el preferido escalado.
        let scale = ui.window().scale_factor();
        window_size = slint::PhysicalSize::new(
            (420.0 * scale).round() as u32,
            (72.0 * scale).round() as u32,
        );
    }

    let width = area.right - area.left;
    let x = area.left + (width - window_size.width as i32) / 2;
    let y = area.bottom - window_size.height as i32 - BOTTOM_MARGIN;

    ui.window().set_position(slint::PhysicalPosition::new(x, y));
}
