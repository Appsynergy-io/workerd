//! `--platform-config` and `--platform-config-fd`: the platform's settings for this process
//! (AppSynergy's apsy.capnp `Platform`), as Cap'n Proto text, read and decoded before the config
//! is loaded. The server applies them (`workerd_server::platform`); without either option it runs
//! every Worker as upstream workerd does.

use std::io::Read;

use crate::bridge::ffi;

/// The most bytes either option reads; more is an error.
pub const MAX_BYTES: usize = 4 << 20;

/// Where the settings come from.
pub enum Source<'a> {
    File(&'a str),
    Descriptor(u32),
}

/// The settings `source` holds, as an encoded `Platform` message in 8-byte words.
pub fn read(source: &Source<'_>) -> Result<Vec<u64>, String> {
    let bytes = match source {
        Source::File(path) => {
            let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
            let mut bytes = Vec::new();
            file.take(
                u64::try_from(MAX_BYTES)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
            )
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
            if bytes.len() > MAX_BYTES {
                return Err(format!("holds more than {MAX_BYTES} bytes"));
            }
            bytes
        }
        Source::Descriptor(fd) => {
            crate::socket_fd::read_descriptor(*fd, MAX_BYTES).map_err(|e| e.to_string())?
        }
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| "is not UTF-8".to_owned())?;
    ffi::decode_platform_config(text).map_err(|e| e.to_string())
}
