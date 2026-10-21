//! El nombre que se muestra de una sesión de GSMTC.
//!
//! Windows no da un identificador de sesión: lo único que hay es el AUMID de
//! la aplicación, y no dice gran cosa. Puede ser el nombre del ejecutable
//! ("chrome.exe"), el de una app de la Store
//! ("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic") o un hexadecimal
//! que el sistema genera para las apps que no registran uno
//! ("F0DC299D809B9700", que es el caso de Firefox y Zen).
//!
//! Acá se traduce ese AUMID a algo que se pueda leer: el nombre del paquete si
//! es una app de la Store, el del proceso si es de escritorio y, si no hay
//! manera, el AUMID limpio.

use std::collections::HashMap;

use windows::Management::Deployment::PackageManager;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WIN32_ERROR};
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::core::{HSTRING, PWSTR};

/// Los nombres que el del ejecutable no aclara.
const NAMES: &[(&str, &str)] = &[
    ("msedge", "Edge"),
    ("chrome", "Chrome"),
    ("firefox", "Firefox"),
    ("zen", "Zen"),
    ("vlc", "VLC"),
    ("spotify", "Spotify"),
];

/// Los nombres ya resueltos.
///
/// Buscar el proceso que publica una sesión es caro: se hace una sola vez por
/// app y el resultado queda guardado acá.
#[derive(Default)]
pub struct Names {
    cache: HashMap<String, String>,
}

impl Names {
    /// El nombre de la app que publica la sesión.
    pub fn name(&mut self, aumid: &str) -> String {
        if aumid.is_empty() {
            return "Sin nombre".to_string();
        }
        if let Some(name) = self.cache.get(aumid) {
            return name.clone();
        }
        let name = resolve(aumid);
        self.cache.insert(aumid.to_string(), name.clone());
        name
    }
}

/// Resuelve el nombre de una app a partir de su AUMID.
fn resolve(aumid: &str) -> String {
    // Las apps de la Store: el nombre viene del paquete.
    if let Some(family) = family(aumid) {
        if let Some(name) = package_name(family) {
            return name;
        }
    }
    // Las de escritorio: el nombre del proceso que la publica.
    if let Some(name) = process_name(aumid) {
        return name;
    }
    // Si no hay manera, el AUMID limpio.
    clean(aumid)
}

/// El nombre de familia del paquete, si el AUMID es de una app de la Store.
///
/// Las apps empacadas se identifican como `Familia_Editorial!App`.
fn family(aumid: &str) -> Option<&str> {
    let (family, _) = aumid.split_once('!')?;
    if family.contains('_') {
        Some(family)
    } else {
        None
    }
}

/// El nombre visible de una app de la Store.
fn package_name(family: &str) -> Option<String> {
    let manager = PackageManager::new().ok()?;
    let packages = manager
        .FindPackagesByPackageFamilyName(&HSTRING::from(family))
        .ok()?;
    for package in packages {
        if let Ok(name) = package.DisplayName() {
            let name = name.to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// El nombre del proceso que publica esta sesión.
///
/// Windows genera un AUMID para las apps que no registran uno, a partir del
/// ejecutable, así que no dice nada. La única forma de saber qué app es es
/// buscar el proceso que tiene ese AUMID.
fn process_name(aumid: &str) -> Option<String> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
    let _close = Handle(snapshot);

    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_err() {
        return None;
    }
    loop {
        if process_aumid(entry.th32ProcessID).as_deref() == Some(aumid) {
            return exe_name(&entry.szExeFile);
        }
        if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
            return None;
        }
    }
}

/// El AUMID de un proceso, si tiene uno.
fn process_aumid(pid: u32) -> Option<String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let _close = Handle(process);

    // Primero se pregunta el tamaño del AUMID.
    let mut len = 0u32;
    unsafe {
        let _ = GetApplicationUserModelId(process, &mut len, None);
    }
    if len == 0 {
        return None;
    }

    let mut buffer = vec![0u16; len as usize];
    let result =
        unsafe { GetApplicationUserModelId(process, &mut len, Some(PWSTR(buffer.as_mut_ptr()))) };
    if result != WIN32_ERROR(0) {
        return None;
    }
    Some(String::from_utf16_lossy(&buffer[..len as usize]))
}

/// "chrome.exe" → "Chrome".
fn exe_name(name: &[u16]) -> Option<String> {
    let full = String::from_utf16_lossy(name);
    let full = full.split('\0').next().unwrap_or("").trim();
    if full.is_empty() {
        return None;
    }
    let base = full
        .strip_suffix(".exe")
        .or_else(|| full.strip_suffix(".EXE"))
        .unwrap_or(full);
    Some(
        NAMES
            .iter()
            .find(|(exe, _)| exe.eq_ignore_ascii_case(base))
            .map(|(_, name)| (*name).to_string())
            .unwrap_or_else(|| capitalize(base)),
    )
}

/// La primera letra en mayúscula.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Un nombre legible a partir del AUMID, cuando no se puede resolver mejor.
///
/// "Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic" → "Microsoft.ZuneMusic"
/// "SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify" → "Spotify"
/// "Spotify.exe" → "Spotify"
/// "MSEdge" → "MSEdge"
fn clean(aumid: &str) -> String {
    let mut name = aumid.rsplit('!').next().unwrap_or(aumid).trim();
    if let Some((family, _)) = name.split_once('_') {
        name = family;
    }
    let name = name.strip_suffix(".exe").unwrap_or(name);
    if name.is_empty() {
        aumid.to_string()
    } else {
        name.to_string()
    }
}

/// Cierra el handle cuando se deja de usar.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[test]
    fn cleans_the_aumid() {
        assert_eq!(
            clean("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            "Microsoft.ZuneMusic"
        );
        assert_eq!(
            clean("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify"),
            "Spotify"
        );
        assert_eq!(clean("Spotify.exe"), "Spotify");
        assert_eq!(clean("MSEdge"), "MSEdge");
        assert_eq!(clean(""), "");
    }

    #[test]
    fn recognizes_store_apps() {
        assert_eq!(
            family("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            Some("Microsoft.ZuneMusic_8wekyb3d8bbwe")
        );
        assert_eq!(family("chrome.exe"), None);
        assert_eq!(family("MSEdge"), None);
    }

    #[test]
    fn takes_the_name_from_the_executable() {
        assert_eq!(exe_name(&wide("chrome.exe")).as_deref(), Some("Chrome"));
        assert_eq!(exe_name(&wide("msedge.exe")).as_deref(), Some("Edge"));
        assert_eq!(exe_name(&wide("vlc.exe")).as_deref(), Some("VLC"));
        assert_eq!(exe_name(&wide("")).as_deref(), None);
    }
}
