// Copyright (c) 2026 Cloudflare, Inc.
// Licensed under the Apache 2.0 license found in the LICENSE file or at:
//     https://opensource.org/licenses/Apache-2.0

//! A disk service's directory, and the entries below it.
//!
//! On Unix the directory is held open and every entry is reached relative to it (`openat` and its
//! kin), as `kj::Directory` reaches them: a path below the root never resolves from the working
//! directory, which `--sandbox`'s seccomp filter refuses (cli/sandbox.rs). Elsewhere entries are
//! reached by path.

use std::fs::File;
use std::io;
use std::path::Path;
use std::time::SystemTime;

/// What a directory entry is, as the listing names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    Symlink,
    BlockDevice,
    CharacterDevice,
    NamedPipe,
    Socket,
    Other,
}

impl Kind {
    /// The listing's name for the kind.
    pub fn name(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Directory => "directory",
            Kind::Symlink => "symlink",
            Kind::BlockDevice => "blockDevice",
            Kind::CharacterDevice => "characterDevice",
            Kind::NamedPipe => "namedPipe",
            Kind::Socket => "socket",
            Kind::Other => "other",
        }
    }
}

/// An entry's metadata, following a symbolic link as `std::fs::metadata` does.
pub struct Metadata {
    pub kind: Kind,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

#[cfg(unix)]
pub use unix::Root;

#[cfg(not(unix))]
pub use paths::Root;

#[cfg(unix)]
mod unix {
    use std::os::fd::AsFd;
    use std::os::fd::BorrowedFd;
    use std::os::fd::OwnedFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::time::Duration;

    use nix::dir::Dir;
    use nix::dir::Type;
    use nix::errno::Errno;
    use nix::fcntl::AtFlags;
    use nix::fcntl::OFlag;
    use nix::fcntl::openat;
    use nix::fcntl::renameat;
    use nix::sys::stat::FileStat;
    use nix::sys::stat::Mode;
    use nix::sys::stat::SFlag;
    use nix::sys::stat::fstatat;
    use nix::sys::stat::mkdirat;
    use nix::unistd::UnlinkatFlags;
    use nix::unistd::unlinkat;

    use super::File;
    use super::Kind;
    use super::Metadata;
    use super::Path;
    use super::SystemTime;
    use super::io;

    /// The directory, held open.
    pub struct Root(OwnedFd);

    /// `.` for the root itself, which an empty path names.
    fn at(rel: &Path) -> &Path {
        if rel.as_os_str().is_empty() {
            Path::new(".")
        } else {
            rel
        }
    }

    fn kind_of_mode(mode: nix::libc::mode_t) -> Kind {
        let format = SFlag::from_bits_truncate(mode & SFlag::S_IFMT.bits());
        if format == SFlag::S_IFREG {
            Kind::File
        } else if format == SFlag::S_IFDIR {
            Kind::Directory
        } else if format == SFlag::S_IFLNK {
            Kind::Symlink
        } else if format == SFlag::S_IFBLK {
            Kind::BlockDevice
        } else if format == SFlag::S_IFCHR {
            Kind::CharacterDevice
        } else if format == SFlag::S_IFIFO {
            Kind::NamedPipe
        } else if format == SFlag::S_IFSOCK {
            Kind::Socket
        } else {
            Kind::Other
        }
    }

    fn metadata_of(st: &FileStat) -> Metadata {
        let modified = u64::try_from(st.st_mtime)
            .ok()
            .zip(u32::try_from(st.st_mtime_nsec).ok())
            .map(|(secs, nanos)| SystemTime::UNIX_EPOCH + Duration::new(secs, nanos));
        Metadata {
            kind: kind_of_mode(st.st_mode),
            len: u64::try_from(st.st_size).unwrap_or(0),
            modified,
        }
    }

    fn kind_of_entry(dir: BorrowedFd<'_>, entry: &nix::dir::Entry) -> Kind {
        match entry.file_type() {
            Some(Type::File) => Kind::File,
            Some(Type::Directory) => Kind::Directory,
            Some(Type::Symlink) => Kind::Symlink,
            Some(Type::BlockDevice) => Kind::BlockDevice,
            Some(Type::CharacterDevice) => Kind::CharacterDevice,
            Some(Type::Fifo) => Kind::NamedPipe,
            Some(Type::Socket) => Kind::Socket,
            // The file system does not say in the entry; the entry itself does.
            None => fstatat(dir, entry.file_name(), AtFlags::AT_SYMLINK_NOFOLLOW)
                .map_or(Kind::Other, |st| kind_of_mode(st.st_mode)),
        }
    }

    /// The entries of the directory `fd` holds, without `.` and `..`, by name.
    fn entries(fd: &OwnedFd) -> io::Result<Vec<(PathBuf, Kind)>> {
        let mut dir = Dir::from_fd(fd.try_clone()?)?;
        let mut out = Vec::new();
        for entry in dir.iter() {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let kind = kind_of_entry(fd.as_fd(), &entry);
            out.push((PathBuf::from(std::ffi::OsStr::from_bytes(name)), kind));
        }
        Ok(out)
    }

    /// Removes the directory `name` in `parent` and everything in it, never following a link.
    fn remove_tree(parent: BorrowedFd<'_>, name: &Path) -> io::Result<()> {
        let dir = openat(
            parent,
            name,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        for (entry, kind) in entries(&dir)? {
            if kind == Kind::Directory {
                remove_tree(dir.as_fd(), &entry)?;
            } else {
                unlinkat(&dir, &entry, UnlinkatFlags::NoRemoveDir)?;
            }
        }
        Ok(unlinkat(parent, name, UnlinkatFlags::RemoveDir)?)
    }

    impl Root {
        /// Opens the directory at `path`.
        pub fn open(path: &Path) -> io::Result<Root> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(OFlag::O_DIRECTORY.bits())
                .open(path)?;
            Ok(Root(file.into()))
        }

        /// The directory `file` holds, opened before the process entered its sandbox.
        pub fn opened(file: &File) -> io::Result<Root> {
            if !file.metadata()?.is_dir() {
                return Err(Errno::ENOTDIR.into());
            }
            Ok(Root(file.try_clone()?.into()))
        }

        pub fn metadata(&self, rel: &Path) -> io::Result<Metadata> {
            Ok(metadata_of(&fstatat(&self.0, at(rel), AtFlags::empty())?))
        }

        pub fn open_file(&self, rel: &Path) -> io::Result<File> {
            let fd = openat(
                &self.0,
                at(rel),
                OFlag::O_RDONLY | OFlag::O_CLOEXEC,
                Mode::empty(),
            )?;
            Ok(File::from(fd))
        }

        /// The entries of the directory at `rel`, without `.` and `..`, by name.
        pub fn read_dir(&self, rel: &Path) -> io::Result<Vec<(String, Kind)>> {
            let dir = openat(
                &self.0,
                at(rel),
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
                Mode::empty(),
            )?;
            Ok(entries(&dir)?
                .into_iter()
                .map(|(name, kind)| (name.to_string_lossy().into_owned(), kind))
                .collect())
        }

        /// Creates the directory at `rel` and every missing one above it.
        pub fn create_dir_all(&self, rel: &Path) -> io::Result<()> {
            let mut path = PathBuf::new();
            for component in rel.components() {
                path.push(component);
                match mkdirat(&self.0, &path, Mode::from_bits_truncate(0o777)) {
                    Ok(()) => {}
                    Err(Errno::EEXIST) if self.metadata(&path)?.kind == Kind::Directory => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(())
        }

        /// Creates the file at `rel`, or truncates it, for writing.
        pub fn create(&self, rel: &Path) -> io::Result<File> {
            let fd = openat(
                &self.0,
                rel,
                OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC | OFlag::O_CLOEXEC,
                Mode::from_bits_truncate(0o666),
            )?;
            Ok(File::from(fd))
        }

        pub fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            Ok(renameat(&self.0, from, &self.0, to)?)
        }

        pub fn remove_file(&self, rel: &Path) -> io::Result<()> {
            Ok(unlinkat(&self.0, rel, UnlinkatFlags::NoRemoveDir)?)
        }

        /// Removes a file, or a directory and everything in it; false when there was nothing
        /// there.
        pub fn remove(&self, rel: &Path) -> io::Result<bool> {
            let st = match fstatat(&self.0, rel, AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(st) => st,
                Err(Errno::ENOENT) => return Ok(false),
                Err(e) => return Err(e.into()),
            };
            if kind_of_mode(st.st_mode) == Kind::Directory {
                remove_tree(self.0.as_fd(), rel)?;
            } else {
                self.remove_file(rel)?;
            }
            Ok(true)
        }
    }
}

#[cfg(not(unix))]
mod paths {
    use std::path::PathBuf;

    use super::File;
    use super::Kind;
    use super::Metadata;
    use super::Path;
    use super::io;

    /// The directory, by path.
    pub struct Root(PathBuf);

    fn kind(file_type: std::fs::FileType) -> Kind {
        if file_type.is_symlink() {
            Kind::Symlink
        } else if file_type.is_dir() {
            Kind::Directory
        } else if file_type.is_file() {
            Kind::File
        } else {
            Kind::Other
        }
    }

    impl Root {
        /// The directory at `path`.
        pub fn open(path: &Path) -> io::Result<Root> {
            if !std::fs::metadata(path)?.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "not a directory",
                ));
            }
            Ok(Root(path.to_owned()))
        }

        /// A directory opened before the process started serving: only Unix has `--sandbox`.
        pub fn opened(_file: &File) -> io::Result<Root> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "a directory opened in advance",
            ))
        }

        pub fn metadata(&self, rel: &Path) -> io::Result<Metadata> {
            let metadata = std::fs::metadata(self.0.join(rel))?;
            Ok(Metadata {
                kind: kind(metadata.file_type()),
                len: metadata.len(),
                modified: metadata.modified().ok(),
            })
        }

        pub fn open_file(&self, rel: &Path) -> io::Result<File> {
            File::open(self.0.join(rel))
        }

        pub fn read_dir(&self, rel: &Path) -> io::Result<Vec<(String, Kind)>> {
            let mut out = Vec::new();
            for entry in std::fs::read_dir(self.0.join(rel))? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                out.push((name, entry.file_type().map_or(Kind::Other, kind)));
            }
            Ok(out)
        }

        pub fn create_dir_all(&self, rel: &Path) -> io::Result<()> {
            std::fs::create_dir_all(self.0.join(rel))
        }

        pub fn create(&self, rel: &Path) -> io::Result<File> {
            File::create(self.0.join(rel))
        }

        pub fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            std::fs::rename(self.0.join(from), self.0.join(to))
        }

        pub fn remove_file(&self, rel: &Path) -> io::Result<()> {
            std::fs::remove_file(self.0.join(rel))
        }

        pub fn remove(&self, rel: &Path) -> io::Result<bool> {
            let path = self.0.join(rel);
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e),
            };
            if metadata.is_dir() {
                std::fs::remove_dir_all(path)?;
            } else {
                std::fs::remove_file(path)?;
            }
            Ok(true)
        }
    }
}
