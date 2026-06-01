use std::{
    ffi::{OsStr, OsString},
    os::windows::ffi::{OsStrExt as _, OsStringExt as _},
    path::{Path, PathBuf},
    ptr,
    sync::OnceLock,
};

use windows_sys::Win32::{
    Foundation::ERROR_SUCCESS,
    System::Registry::{RegGetValueW, HKEY, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY},
};

/// Default configuration directory for the Datadog Agent.
///
/// The Datadog Agent Windows installer stores the effective configuration root in
/// `HKLM\\SOFTWARE\\Datadog\\Datadog Agent\\ConfigRoot`. This constant is the fallback
/// used when the registry value is unavailable.
pub const DATADOG_AGENT_CONF_DIR: &str = r"C:\ProgramData\Datadog";

/// Default log directory for the Datadog Agent.
///
/// The effective log directory is derived from the registry-backed configuration root at runtime.
/// This constant is the fallback used when the registry value is unavailable.
pub const DATADOG_AGENT_LOG_DIR: &str = r"C:\ProgramData\Datadog\logs";

/// Default local syslog URI for the Datadog Agent.
///
/// The core Agent does not support syslog logging on Windows.
pub const DATADOG_AGENT_DEFAULT_SYSLOG_URI: &str = "";

const DATADOG_AGENT_REGISTRY_SUBKEY: &str = r"SOFTWARE\Datadog\Datadog Agent";
const DATADOG_AGENT_CONFIG_ROOT_VALUE: &str = "ConfigRoot";

static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();
static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Returns the path to the default Datadog Agent configuration directory.
pub fn get_config_dir_path() -> &'static Path {
    CONFIG_DIR
        .get_or_init(|| read_config_root_from_registry().unwrap_or_else(|| PathBuf::from(DATADOG_AGENT_CONF_DIR)))
        .as_path()
}

/// Returns the path to the default Datadog Agent log directory.
pub fn get_log_dir_path() -> &'static Path {
    LOG_DIR.get_or_init(|| get_config_dir_path().join("logs")).as_path()
}

fn read_config_root_from_registry() -> Option<PathBuf> {
    read_registry_string(
        HKEY_LOCAL_MACHINE,
        DATADOG_AGENT_REGISTRY_SUBKEY,
        DATADOG_AGENT_CONFIG_ROOT_VALUE,
    )
    .filter(|value| !value.as_os_str().is_empty())
    .map(PathBuf::from)
}

fn read_registry_string(hkey: HKEY, subkey: &str, value: &str) -> Option<OsString> {
    let subkey_wide = to_wide_null(subkey);
    let value_wide = to_wide_null(value);
    let mut byte_len = 0u32;

    // SAFETY: `subkey_wide` and `value_wide` are null-terminated UTF-16 strings. Passing a null data pointer with a
    // valid byte-count pointer asks Windows for the required buffer size without writing data.
    let status = unsafe {
        RegGetValueW(
            hkey,
            subkey_wide.as_ptr(),
            value_wide.as_ptr(),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut byte_len,
        )
    };
    if status != ERROR_SUCCESS || byte_len == 0 {
        return None;
    }

    let mut buffer = vec![0u16; byte_len as usize / size_of::<u16>()];
    // SAFETY: `buffer` is sized from the preceding `RegGetValueW` query and is passed as a writable byte buffer with
    // the corresponding byte count. The input strings remain valid and null-terminated for the duration of the call.
    let status = unsafe {
        RegGetValueW(
            hkey,
            subkey_wide.as_ptr(),
            value_wide.as_ptr(),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut byte_len,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }

    let value_len = byte_len as usize / size_of::<u16>();
    buffer.truncate(value_len);
    if buffer.last() == Some(&0) {
        buffer.pop();
    }

    Some(OsString::from_wide(&buffer))
}

fn to_wide_null(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain([0]).collect()
}
