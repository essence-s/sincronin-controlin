//! Ajustes de ventana para Linux.
//!
//! En Wayland el compositor es el dueño de la ventana: no existe un
//! protocolo para que una app pida "flotame", "no me des foco" o "no me
//! pongas en la barra de tareas". Ni Slint ni winit tienen nada que ofrecer
//! ahí — `set_skip_taskbar` solo existe en la implementación de Windows de
//! winit, y `always-on-top` no funciona en Wayland.
//!
//! Por eso la app no intenta hackear el compositor. Las reglas van en la
//! configuración de Hyprland (Lua desde 0.55):
//!
//! ```lua
//! -- ~/.config/hypr/modules/rules.lua
//! hl.window_rule({
//!     name = "sincronin-controlin",
//!     match = { title = "^sincronin-controlin$" },
//!     float = true,
//!     pin = true,
//!     focus_on_activate = false,
//!     no_initial_focus = true,
//!     no_blur = true,
//!     no_anim = true,
//!     move = { "(monitor_w-window_w)/2", "monitor_h-window_h-24" },
//! })
//! ```
//!
//! El match es por `title` y no por `class` a propósito: Slint no setea el
//! `app_id` de Wayland, así que la clase de la ventana viene vacía.
//!
//! Y `no_focus = true` no va: deja la ventana permanentemente inenfocable, y
//! deja de responder a los clics — ni los botones ni el arrastre funcionan.
//! Para "no robar foco al aparecer" está `focus_on_activate = false`, que no
//! le impide ser interactiva.
//!
//! Lo único portable que queda ya está en la UI: `no-frame: true` y
//! `always-on-top: true`. El segundo funciona en X11 y en Windows, y en
//! Wayland simplemente se ignora.

use crate::AppWindow;

/// No hay nada que ajustar: en Wayland lo decide el compositor.
pub fn configure(_ui: &AppWindow) {}
