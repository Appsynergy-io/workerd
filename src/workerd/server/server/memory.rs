//! This process's resident memory, which the sweep of the workers built on demand holds to
//! apsy.capnp `Platform.isolateMemoryBudget` (lazy.rs).
//!
//! Read from `VmRSS` in `/proc/self/status`. Under `--sandbox`, which leaves no `/proc` and refuses
//! every path-taking call, the command line opens the file before it enters the sandbox and the
//! reads go through that descriptor, which names only this process's own figures.

use std::fs::File;
use std::io;

/// Where the resident figure is read from.
pub struct ResidentMemory {
    /// `/proc/self/status`, held open by `--sandbox`; none to open it by its path each time.
    held: Option<File>,
}

impl ResidentMemory {
    #[must_use]
    pub const fn new(held: Option<File>) -> Self {
        Self { held }
    }

    /// The resident bytes; none where the figure cannot be read.
    #[must_use]
    pub fn bytes(&self) -> Option<u64> {
        let status = self.read().ok()?;
        parse_vm_rss(&status)
    }

    #[cfg(unix)]
    fn read(&self) -> io::Result<String> {
        use std::os::unix::fs::FileExt;

        let Some(file) = &self.held else {
            return std::fs::read_to_string("/proc/self/status");
        };
        // A procfs file is generated whole at offset 0; it is a few kilobytes.
        let mut buf = vec![0; 16 << 10];
        let n = file.read_at(&mut buf, 0)?;
        buf.truncate(n);
        String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    #[cfg(not(unix))]
    fn read(&self) -> io::Result<String> {
        let _ = &self.held;
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

/// `VmRSS:  1234 kB` from a `/proc/<pid>/status` text, in bytes.
fn parse_vm_rss(status: &str) -> Option<u64> {
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?;
    let kib = line.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()?;
    kib.checked_mul(1024)
}

#[cfg(test)]
#[path = "memory-test.rs"]
mod tests;
