//! Windows per-user autostart, widget preferences, and taskbar theme detection.

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_DWORD,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};

const RUN_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const APP_SUBKEY: &str = "Software\\AttackSharkX3Battery";
const PERSONALIZE_SUBKEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";

const RUN_VALUE_NAME: &str = "AttackSharkX3Battery";
const OFFSET_X_VALUE: &str = "OffsetX";
const LIGHT_TASKBAR_VALUE: &str = "SystemUsesLightTheme";

struct SafeHkey(HKEY);

impl Drop for SafeHkey {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn current_exe_quoted_wide() -> Option<Vec<u16>> {
    let mut buf = vec![0u16; 1024];
    let len = unsafe { GetModuleFileNameW(null_mut(), buf.as_mut_ptr(), buf.len() as u32) };
    if len == 0 || len as usize >= buf.len() {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    Some(to_wide_null(&format!("\"{path}\"")))
}

fn open_key(subkey: &str, access: u32) -> Option<SafeHkey> {
    let subkey_w = to_wide_null(subkey);
    let mut hkey: HKEY = null_mut();
    let res = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey_w.as_ptr(),
            0,
            access,
            &mut hkey,
        )
    };
    if res == ERROR_SUCCESS && !hkey.is_null() {
        Some(SafeHkey(hkey))
    } else {
        None
    }
}

fn create_app_key(access: u32) -> Option<SafeHkey> {
    let subkey_w = to_wide_null(APP_SUBKEY);
    let mut hkey: HKEY = null_mut();
    let res = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey_w.as_ptr(),
            0,
            null(),
            REG_OPTION_NON_VOLATILE,
            access,
            null(),
            &mut hkey,
            null_mut(),
        )
    };
    if res == ERROR_SUCCESS && !hkey.is_null() {
        Some(SafeHkey(hkey))
    } else {
        None
    }
}

fn read_dword(subkey: &str, value_name: &str) -> Option<u32> {
    let hkey = open_key(subkey, KEY_READ)?;
    let name_w = to_wide_null(value_name);
    let mut reg_type = 0u32;
    let mut val = 0u32;
    let mut cb_data = std::mem::size_of::<u32>() as u32;
    let res = unsafe {
        RegQueryValueExW(
            hkey.0,
            name_w.as_ptr(),
            null(),
            &mut reg_type,
            (&mut val as *mut u32).cast(),
            &mut cb_data,
        )
    };
    if res == ERROR_SUCCESS && reg_type == REG_DWORD && cb_data == 4 {
        Some(val)
    } else {
        None
    }
}

fn write_dword(value_name: &str, val: u32) -> bool {
    let Some(hkey) = create_app_key(KEY_WRITE) else {
        return false;
    };
    let name_w = to_wide_null(value_name);
    let res = unsafe {
        RegSetValueExW(
            hkey.0,
            name_w.as_ptr(),
            0,
            REG_DWORD,
            (&val as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    res == ERROR_SUCCESS
}

pub fn is_enabled() -> bool {
    let Some(hkey) = open_key(RUN_SUBKEY, KEY_READ) else {
        return false;
    };
    let name = to_wide_null(RUN_VALUE_NAME);
    let mut reg_type = 0u32;
    let mut cb_data = 0u32;
    let res = unsafe {
        RegQueryValueExW(
            hkey.0,
            name.as_ptr(),
            null(),
            &mut reg_type,
            null_mut(),
            &mut cb_data,
        )
    };
    res == ERROR_SUCCESS && reg_type == REG_SZ && cb_data >= 2
}

pub fn set_enabled(enable: bool) -> bool {
    let Some(hkey) = open_key(RUN_SUBKEY, KEY_WRITE) else {
        return false;
    };
    let name = to_wide_null(RUN_VALUE_NAME);
    if enable {
        let Some(cmd_w) = current_exe_quoted_wide() else {
            return false;
        };
        let byte_len = (cmd_w.len() * 2) as u32;
        let res = unsafe {
            RegSetValueExW(
                hkey.0,
                name.as_ptr(),
                0,
                REG_SZ,
                cmd_w.as_ptr().cast(),
                byte_len,
            )
        };
        res == ERROR_SUCCESS
    } else {
        let res = unsafe { RegDeleteValueW(hkey.0, name.as_ptr()) };
        res == ERROR_SUCCESS
    }
}

pub fn load_offset_x() -> i32 {
    read_dword(APP_SUBKEY, OFFSET_X_VALUE)
        .map(|v| v as i32)
        .unwrap_or(0)
}

pub fn save_offset_x(offset: i32) {
    write_dword(OFFSET_X_VALUE, offset as u32);
}

pub fn is_light_taskbar() -> bool {
    read_dword(PERSONALIZE_SUBKEY, LIGHT_TASKBAR_VALUE).unwrap_or(0) == 1
}
