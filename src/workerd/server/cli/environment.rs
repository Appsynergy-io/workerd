//! `--environment-fd`: values for `fromEnvironment` bindings handed over on a descriptor rather
//! than in the process environment, which any process of the same user reads in
//! `/proc/<pid>/environ` for as long as workerd runs.
//!
//! The descriptor holds `NAME=value` records, each ended by a NUL byte (the layout of
//! `/proc/<pid>/environ`), read to its end, at most [`MAX_BYTES`], before the config is loaded. A
//! record with no `=`, an empty name, a name given twice or a last record with no NUL is an error.
//! A `fromEnvironment` binding takes the value its name has among the records, else the process
//! environment's (`Overrides::environment`). The descriptor is then closed or, when it is also
//! `--control-fd`'s (one socket carrying the records in and the events out), shut down for
//! reading.

use std::collections::HashMap;

/// The most bytes `--environment-fd` reads; more is an error.
pub const MAX_BYTES: usize = 4 << 20;

/// The records in `bytes`, by name.
pub fn parse(bytes: &[u8]) -> Result<HashMap<String, String>, String> {
    let mut records = HashMap::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let Some(end) = rest.iter().position(|&b| b == 0) else {
            return Err("the last environment record is not ended by a NUL byte".to_owned());
        };
        let (record, after) = rest.split_at(end);
        rest = &after[1..];
        let record = std::str::from_utf8(record)
            .map_err(|_| "an environment record is not UTF-8".to_owned())?;
        let Some((name, value)) = record.split_once('=') else {
            return Err("an environment record has no '=' in it".to_owned());
        };
        if name.is_empty() {
            return Err("an environment record has an empty name".to_owned());
        }
        if records.insert(name.to_owned(), value.to_owned()).is_some() {
            return Err(format!(
                "an environment record's name is given twice: {name}"
            ));
        }
    }
    Ok(records)
}

/// Reads and parses the records on `fd`, then closes it, or shuts it down for reading when it is
/// also the control descriptor, which stays open for the events.
pub fn read(fd: u32, control_fd: Option<u32>) -> Result<HashMap<String, String>, String> {
    let bytes = crate::socket_fd::read_environment(fd, MAX_BYTES, control_fd == Some(fd))
        .map_err(|error| error.to_string())?;
    parse(&bytes)
}

#[cfg(test)]
#[path = "environment-test.rs"]
mod tests;
