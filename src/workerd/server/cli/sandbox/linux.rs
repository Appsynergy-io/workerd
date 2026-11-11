//! The sandbox on Linux (see `sandbox.rs`).

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

use workerd_capnp::config;
use workerd_capnp::service;
use workerd_server::run::RunOptions;

use crate::config::Config;

/// A disk directory the config names, mounted into the sandbox's root at `disk/<n>`.
struct DiskDirectory {
    name: String,
    path: PathBuf,
    writable: bool,
}

/// Enters the sandbox (see the module doc of `sandbox.rs`). The config's own mistakes under
/// `--sandbox` (a socket that is not a UNIX domain socket, an external service on an abstract
/// socket) go to `options.config_errors`, which the server reports as config errors; a system call
/// that fails is the error, and the process does not start.
pub fn enter(config: &Config, options: &mut RunOptions) -> Result<(), String> {
    let message = config.message().map_err(|e| e.to_string())?;
    let config = message
        .get_root::<config::Reader>()
        .map_err(|e| e.to_string())?;
    let disks = disk_directories(config, &mut options.overrides.directories)?;
    let sockets = socket_directories(config, options)?;
    bind_sockets(config, options)?;

    // The server's disk filesystem opens `/` and the working directory, which the seccomp filter
    // refuses once it is installed. Opened now, its descriptors are among the host's below.
    crate::bridge::ffi::open_disk_filesystem().map_err(|e| format!("open the filesystem: {e}"))?;
    // The platform's TLS roots are read from files too: read now, they verify servers inside.
    kj_hyper::tls::load_platform_roots();
    // Every descriptor this process holds on the host's filesystem, found while /proc is there:
    // the parent's, and any a library left open. After the pivot each is pointed at the empty
    // root in place, so none of them is a way back to the host.
    let host_fds = host_descriptors()?;
    // Opened after the host's descriptors are listed, so it is not pointed at the empty root: the
    // sweep of the workers built on demand reads the resident memory through it (server
    // memory.rs).
    options.process_status = Some(
        std::fs::File::open("/proc/self/status")
            .map_err(|e| format!("open /proc/self/status: {e}"))?,
    );
    enter_namespaces(&disks, &sockets)?;
    let new_root = open_dir(Path::new("/"), libc::O_RDONLY)?;
    for fd in host_fds {
        // SAFETY: dup3 onto a descriptor number this process holds; the object owning that number
        // keeps a valid descriptor, now naming the new root.
        syscall("dup3", unsafe {
            libc::dup3(new_root.as_raw_fd(), fd, libc::O_CLOEXEC)
        })?;
    }
    drop(new_root);

    // The server opens each disk directory where it now is, so `..` from any of them ends at the
    // tmpfs, never on the host. The disk service reaches its entries relative to the directory,
    // opened here while a path still resolves: the filter refuses one from the working directory.
    for (i, disk) in disks.into_iter().enumerate() {
        let path = format!("/disk/{i}");
        let opened = std::fs::File::open(&path).map_err(|e| format!("open {path}: {e}"))?;
        options
            .overrides
            .opened_directories
            .insert(disk.name.clone(), opened);
        options.overrides.directories.insert(disk.name, path);
    }

    drop_all_capabilities()?;
    forbid_tracing()?;
    install_seccomp()
}

/// Every disk directory the config names, found now while paths resolve; its `--directory-path`
/// override, if any, is taken in its place. One that does not exist is left to the server, which
/// reports it as it does without the sandbox.
fn disk_directories(
    config: config::Reader<'_>,
    overrides: &mut HashMap<String, String>,
) -> Result<Vec<DiskDirectory>, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("current directory: {e}"))?;
    let mut disks = Vec::new();
    for svc in config.get_services().map_err(|e| e.to_string())? {
        let Ok(service::Which::Disk(conf)) = svc.which() else {
            continue;
        };
        let conf = conf.map_err(|e| e.to_string())?;
        let name = text(svc.get_name())?;
        let path = match overrides.get(&name) {
            Some(path) => path.clone(),
            None if conf.has_path() => text(conf.get_path())?,
            None => continue,
        };
        let path = cwd.join(path);
        if !path.is_dir() {
            continue;
        }
        overrides.remove(&name);
        disks.push(DiskDirectory {
            name,
            path,
            writable: conf.get_writable(),
        });
    }
    Ok(disks)
}

/// The components of an absolute directory path, or none when it is not one the sandbox mounts a
/// socket directory at: relative, the root itself, holding an empty, `.` or `..` component, or
/// under the sandbox's own `/disk` and `/old`.
fn socket_directory_parts(dir: &str) -> Option<Vec<&str>> {
    let rest = dir.strip_prefix('/')?;
    if rest.is_empty() {
        return None;
    }
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.iter().any(|part| matches!(*part, "" | "." | "..")) {
        return None;
    }
    if matches!(parts[0], "disk" | "old") {
        return None;
    }
    Some(parts)
}

/// Every directory holding a socket file an external service dials: the sandbox has a network
/// namespace of its own, where no abstract socket of the host's is reachable, so the platform's
/// services are socket files, and each directory holding one is mounted at the path the config
/// names it by. Only a directory, and only what the config dials.
fn socket_directories(
    config: config::Reader<'_>,
    options: &mut RunOptions,
) -> Result<Vec<String>, String> {
    let mut dirs: Vec<String> = Vec::new();
    for svc in config.get_services().map_err(|e| e.to_string())? {
        let Ok(service::Which::External(external)) = svc.which() else {
            continue;
        };
        let external = external.map_err(|e| e.to_string())?;
        let name = text(svc.get_name())?;
        let address = match options.overrides.externals.get(&name) {
            Some(address) => address.clone(),
            None if external.has_address() => text(external.get_address())?,
            None => continue,
        };
        if address.starts_with("unix-abstract:") {
            options.config_errors.push(format!(
                "External service \"{name}\": under --sandbox the runtime has a network namespace \
                 of its own, where an abstract socket is not reachable; name a socket file, \
                 `unix:PATH`, not {address}"
            ));
            continue;
        }
        let Some(path) = address.strip_prefix("unix:") else {
            continue;
        };
        let Some((dir, _)) = path.rsplit_once('/') else {
            continue;
        };
        if socket_directory_parts(dir).is_none() {
            options.config_errors.push(format!(
                "External service \"{name}\": under --sandbox a socket file is named by an \
                 absolute path outside /disk and /old, not {address}"
            ));
            continue;
        }
        if Path::new(dir).is_dir() && !dirs.iter().any(|known| known == dir) {
            dirs.push(dir.to_owned());
        }
    }
    Ok(dirs)
}

/// Binds every socket the config names now, while a UNIX socket path resolves: bound and not
/// listening, so no connection is accepted (even into a backlog) before the Workers are built.
/// Only a UNIX domain socket can be bound: the sandbox admits no other family. A socket the
/// command line hands in (`--socket-fd`) is already listening, and is left as it is.
fn bind_sockets(config: config::Reader<'_>, options: &mut RunOptions) -> Result<(), String> {
    for sock in config.get_sockets().map_err(|e| e.to_string())? {
        let name = text(sock.get_name())?;
        if options.socket_fds.contains_key(&name) {
            continue;
        }
        let address = match options.socket_addresses.remove(&name) {
            Some(address) => address,
            None if sock.has_address() => text(sock.get_address())?,
            // The server reports the missing address.
            None => continue,
        };
        let addr = if let Some(path) = address.strip_prefix("unix:") {
            // A file left by an earlier process on the same path.
            let _ = std::fs::remove_file(path);
            socket2::SockAddr::unix(path)
        } else if let Some(abstract_name) = address.strip_prefix("unix-abstract:") {
            let mut bytes = vec![0_u8];
            bytes.extend_from_slice(abstract_name.as_bytes());
            socket2::SockAddr::unix(std::ffi::OsStr::from_bytes(&bytes))
        } else {
            options.config_errors.push(format!(
                "Socket \"{name}\": under --sandbox a socket is a UNIX domain socket, \
                 `unix:PATH` or `unix-abstract:NAME`, not {address}"
            ));
            continue;
        };
        let addr = match addr {
            Ok(addr) => addr,
            Err(e) => {
                options
                    .config_errors
                    .push(format!("Socket \"{name}\": {address}: {e}"));
                continue;
            }
        };
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
            .map_err(|e| format!("Socket \"{name}\": socket(): {e}"))?;
        socket
            .bind(&addr)
            .map_err(|e| format!("Socket \"{name}\": bind({address}): {e}"))?;
        options.prebound.insert(name, socket);
    }
    Ok(())
}

/// Every descriptor above stderr that reaches the host's filesystem -- a directory, a regular
/// file, a device or a symlink -- found through `/proc/self/fd`. A socket, a pipe and an anonymous
/// inode (epoll, eventfd, signalfd, timerfd) reach nothing by a path and are not listed.
fn host_descriptors() -> Result<Vec<i32>, String> {
    let entries = std::fs::read_dir("/proc/self/fd").map_err(|e| format!("/proc/self/fd: {e}"))?;
    let mut numbers = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("/proc/self/fd: {e}"))?;
        if let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        {
            numbers.push(fd);
        }
    }
    // The directory's own descriptor is closed by now, and its number fails fstat or is reused by
    // nothing this process opened since.
    let mut fds = Vec::new();
    for fd in numbers {
        if fd <= libc::STDERR_FILENO {
            continue;
        }
        // SAFETY: fstat writes into a zeroed stat buffer it is given; a closed number fails.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { libc::fstat(fd, &raw mut st) } != 0 {
            continue;
        }
        if matches!(
            st.st_mode & libc::S_IFMT,
            libc::S_IFDIR | libc::S_IFREG | libc::S_IFCHR | libc::S_IFBLK | libc::S_IFLNK
        ) {
            fds.push(fd);
        }
    }
    Ok(fds)
}

/// The user, mount, network, IPC and UTS namespaces, and an empty read-only root holding only the
/// disk and socket directories.
fn enter_namespaces(disks: &[DiskDirectory], sockets: &[String]) -> Result<(), String> {
    // SAFETY: getuid and getgid cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    // The user namespace first: it is what lets an unprivileged process own the other namespaces
    // and the mounts below. The process must be single-threaded here: the event loop and V8 start
    // after this returns. The network namespace holds no interface but a loopback that is down:
    // the sockets the process listens on were bound before it, and it dials only socket files.
    // SAFETY: unshare takes flags only.
    syscall("unshare", unsafe {
        libc::unshare(
            libc::CLONE_NEWUSER
                | libc::CLONE_NEWNS
                | libc::CLONE_NEWNET
                | libc::CLONE_NEWIPC
                | libc::CLONE_NEWUTS,
        )
    })?;
    write_proc_file("/proc/self/setgroups", "deny")?;
    write_proc_file("/proc/self/uid_map", &format!("{uid} {uid} 1\n"))?;
    write_proc_file("/proc/self/gid_map", &format!("{gid} {gid} 1\n"))?;

    // An empty root: a tmpfs on a directory the host has, made the root by pivot_root, the old
    // root detached and its mount point removed. Nothing is left in it.
    mount(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None)?;
    let base = ["/tmp", "/var/tmp", "/mnt", "/run", "/dev/shm"]
        .into_iter()
        .find(|candidate| Path::new(candidate).is_dir())
        .ok_or("no directory to mount the sandbox's empty root on")?;
    // Each disk and socket directory, held open before the tmpfs goes over `base`: a directory
    // under it (a state directory in /tmp) is hidden by that mount, and its descriptor still names
    // it.
    let disk_fds = disks
        .iter()
        .map(|disk| open_dir(&disk.path, libc::O_PATH))
        .collect::<Result<Vec<_>, _>>()?;
    let socket_fds = sockets
        .iter()
        .map(|dir| open_dir(Path::new(dir), libc::O_PATH))
        .collect::<Result<Vec<_>, _>>()?;
    mount(
        Some("none"),
        base,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        Some("size=64k,mode=0700"),
    )?;
    let old = format!("{base}/old");
    make_dir(&old)?;
    // Each disk directory, bind-mounted under the new root at `disk/<n>`. A read-only one is
    // remounted so, keeping the flags the kernel locked on it.
    if !disks.is_empty() {
        let disk_root = format!("{base}/disk");
        make_dir(&disk_root)?;
        for (i, (disk, fd)) in disks.iter().zip(&disk_fds).enumerate() {
            let target = format!("{disk_root}/{i}");
            make_dir(&target)?;
            bind_directory(fd, &target, disk.writable)?;
        }
    }
    // Each directory of socket files an external service dials, read-only at the path the config
    // names it by: connecting to a socket needs its file, not a writable mount.
    for (dir, fd) in sockets.iter().zip(&socket_fds) {
        let mut target = base.to_owned();
        for part in socket_directory_parts(dir).ok_or("socket directory path changed")? {
            target = format!("{target}/{part}");
            match std::fs::create_dir(&target) {
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
                    return Err(format!("mkdir {target}: {e}"));
                }
                _ => {}
            }
        }
        bind_directory(fd, &target, false)?;
    }
    drop((disk_fds, socket_fds));
    let base_c = c_string(base)?;
    let old_c = c_string(&old)?;
    // SAFETY: pivot_root takes two NUL-terminated paths, alive across the call.
    syscall("pivot_root", unsafe {
        libc::syscall(libc::SYS_pivot_root, base_c.as_ptr(), old_c.as_ptr())
    })?;
    std::env::set_current_dir("/").map_err(|e| format!("chdir /: {e}"))?;
    let detached = c_string("/old")?;
    // SAFETY: umount2 takes a NUL-terminated path, alive across the call.
    syscall("umount2", unsafe {
        libc::umount2(detached.as_ptr(), libc::MNT_DETACH)
    })?;
    std::fs::remove_dir("/old").map_err(|e| format!("rmdir /old: {e}"))?;
    mount(
        None,
        "/",
        None,
        libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    )
}

/// Bind-mounts the directory `fd` holds at `target`, read-only unless `writable`.
fn bind_directory(fd: &OwnedFd, target: &str, writable: bool) -> Result<(), String> {
    let source = format!("/proc/self/fd/{}", fd.as_raw_fd());
    mount(
        Some(&source),
        target,
        None,
        libc::MS_BIND | libc::MS_REC,
        None,
    )?;
    if !writable {
        mount(
            None,
            target,
            None,
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | locked_mount_flags(target)?,
            None,
        )?;
    }
    Ok(())
}

/// The mount flags a bind mount of `path` already carries, which a remount inside a user
/// namespace must keep: the kernel locks them [mount(2) MS_REMOUNT, MS_BIND].
fn locked_mount_flags(path: &str) -> Result<libc::c_ulong, String> {
    let path_c = c_string(path)?;
    // SAFETY: statvfs writes into a zeroed buffer it is given, from a NUL-terminated path.
    let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    syscall("statvfs", unsafe {
        libc::statvfs(path_c.as_ptr(), &raw mut sv)
    })?;
    let mut flags = 0;
    for (st, ms) in [
        (libc::ST_NOSUID, libc::MS_NOSUID),
        (libc::ST_NODEV, libc::MS_NODEV),
        (libc::ST_NOEXEC, libc::MS_NOEXEC),
        (libc::ST_NOATIME, libc::MS_NOATIME),
        (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        (libc::ST_RELATIME, libc::MS_RELATIME),
    ] {
        if sv.f_flag & st != 0 {
            flags |= ms;
        }
    }
    Ok(flags)
}

/// Drops every capability and the bounding set, and sets `no_new_privs`.
fn drop_all_capabilities() -> Result<(), String> {
    // The kernel knows capabilities up to its last one and answers EINVAL past it.
    for cap in 0..64 {
        // SAFETY: prctl with integer arguments only.
        if unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINVAL) {
                break;
            }
            return Err(format!("prctl(PR_CAPBSET_DROP, {cap}): {error}"));
        }
    }
    // `struct __user_cap_header_struct` and two `struct __user_cap_data_struct`, all zero but the
    // version: no capability in any set [capget(2)].
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    let header: [u32; 2] = [LINUX_CAPABILITY_VERSION_3, 0];
    let data = [0_u32; 6];
    // SAFETY: capset reads a header and two data structs from the buffers it is given.
    syscall("capset", unsafe {
        libc::syscall(libc::SYS_capset, header.as_ptr(), data.as_ptr())
    })?;
    // SAFETY: prctl with integer arguments only.
    syscall("prctl(PR_SET_NO_NEW_PRIVS)", unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)
    })
}

/// Makes the process non-dumpable: no other process of the same user may ptrace it, read its
/// memory through `/proc`, or have it dump core.
fn forbid_tracing() -> Result<(), String> {
    // SAFETY: prctl with integer arguments only.
    syscall("prctl(PR_SET_DUMPABLE)", unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0)
    })
}

// `struct seccomp_data`'s fields, by offset [linux/seccomp.h].
const SECCOMP_DATA_NR: u32 = 0;
const SECCOMP_DATA_ARCH: u32 = 4;
const SECCOMP_DATA_ARG0: u32 = 16;
const SECCOMP_DATA_ARG1: u32 = 24;

// The architecture a call is made in [linux/audit.h].
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

// The extended-attribute calls of Linux 6.13, which libc does not yet name; one number on every
// architecture [include/uapi/asm-generic/unistd.h].
const SYS_SETXATTRAT: i64 = 463;
const SYS_GETXATTRAT: i64 = 464;
const SYS_LISTXATTRAT: i64 = 465;
const SYS_REMOVEXATTRAT: i64 = 466;

// The socket ioctls that set a descriptor's owner, one number on every architecture
// [include/uapi/asm-generic/sockios.h].
const FIOSETOWN: u32 = 0x8901;
const SIOCSPGRP: u32 = 0x8902;

// fcntl()'s F_SETOWN_EX, which libc does not yet name; one number on every architecture
// [include/uapi/asm-generic/fcntl.h].
const F_SETOWN_EX: libc::c_int = 15;

/// Classic BPF over `struct seccomp_data`. Every rule leaves the syscall number loaded in the
/// accumulator for the next one.
#[derive(Default)]
struct Filter {
    prog: Vec<libc::sock_filter>,
}

impl Filter {
    fn stmt(&mut self, code: u32, k: u32) {
        self.prog.push(libc::sock_filter {
            code: code as u16,
            jt: 0,
            jf: 0,
            k,
        });
    }
    fn jump(&mut self, op: u32, k: u32, jt: u8, jf: u8) {
        self.prog.push(libc::sock_filter {
            code: (libc::BPF_JMP | op | libc::BPF_K) as u16,
            jt,
            jf,
            k,
        });
    }
    fn load(&mut self, offset: u32) {
        self.stmt(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, offset);
    }
    fn ret(&mut self, k: u32) {
        self.stmt(libc::BPF_RET | libc::BPF_K, k);
    }
    fn deny_errno(&mut self, nr: i64, errno: i32) {
        self.jump(libc::BPF_JEQ, nr as u32, 0, 1);
        self.ret(libc::SECCOMP_RET_ERRNO | errno as u32);
    }
    fn deny(&mut self, nr: i64) {
        self.deny_errno(nr, libc::EPERM);
    }
    /// Refuses `nr` when the low 32 bits of its first argument equal `arg0`.
    fn deny_if_arg0(&mut self, nr: i64, arg0: u32) {
        self.jump(libc::BPF_JEQ, nr as u32, 0, 4);
        self.load(SECCOMP_DATA_ARG0);
        self.jump(libc::BPF_JEQ, arg0, 0, 1);
        self.ret(libc::SECCOMP_RET_ERRNO | libc::EPERM as u32);
        self.load(SECCOMP_DATA_NR);
    }
    /// Refuses `nr` unless the low 32 bits of its first argument equal `arg0`. Every argument
    /// this is used on is an int the kernel reads from those 32 bits alone.
    fn deny_unless_arg0(&mut self, nr: i64, arg0: u32) {
        self.jump(libc::BPF_JEQ, nr as u32, 0, 4);
        self.load(SECCOMP_DATA_ARG0);
        self.jump(libc::BPF_JEQ, arg0, 1, 0);
        self.ret(libc::SECCOMP_RET_ERRNO | libc::EPERM as u32);
        self.load(SECCOMP_DATA_NR);
    }
    /// Refuses `nr` when the low 32 bits of its second argument equal `arg1`. Every argument this
    /// is used on is a command the kernel reads from those 32 bits alone.
    fn deny_if_arg1(&mut self, nr: i64, arg1: u32) {
        self.jump(libc::BPF_JEQ, nr as u32, 0, 4);
        self.load(SECCOMP_DATA_ARG1);
        self.jump(libc::BPF_JEQ, arg1, 0, 1);
        self.ret(libc::SECCOMP_RET_ERRNO | libc::EPERM as u32);
        self.load(SECCOMP_DATA_NR);
    }
    /// Refuses `nr` when its first argument has any of `mask` set.
    fn deny_if_arg0_has(&mut self, nr: i64, mask: u32) {
        self.jump(libc::BPF_JEQ, nr as u32, 0, 4);
        self.load(SECCOMP_DATA_ARG0);
        self.jump(libc::BPF_JSET, mask, 0, 1);
        self.ret(libc::SECCOMP_RET_ERRNO | libc::EPERM as u32);
        self.load(SECCOMP_DATA_NR);
    }
    /// The program's bytes, as the tests hand a forked child the filter for its own pid.
    #[cfg(test)]
    fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `sock_filter` is plain data with no padding (u16, u8, u8, u32), so its bytes are
        // its value and any bytes are one.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.prog.as_mut_ptr().cast::<u8>(),
                std::mem::size_of_val(self.prog.as_slice()),
            )
        }
    }

    fn install(&self) -> Result<(), String> {
        let len = u16::try_from(self.prog.len())
            .ok()
            .filter(|len| u32::from(*len) < libc::BPF_MAXINSNS as u32)
            .ok_or("seccomp program too long")?;
        let fprog = libc::sock_fprog {
            len,
            filter: self.prog.as_ptr().cast_mut(),
        };
        // SAFETY: seccomp reads the program `fprog` points at, alive across the call.
        syscall("seccomp", unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                libc::SECCOMP_FILTER_FLAG_TSYNC,
                &raw const fprog,
            )
        })
    }
}

/// Installs the seccomp filter on every thread (TSYNC): refuses every path-taking filesystem call,
/// every descriptor-relative one against `AT_FDCWD`, the extended-attribute calls, `socket()` and
/// `socketpair()` for any family but `AF_UNIX`, a signal to any process but this one, setting a
/// descriptor's owner (which directs its I/O signals), the pidfd
/// calls, tracing and cross-process memory calls, `kcmp()`, mounts, namespaces, modules, keys, BPF
/// and io_uring. `clone3()` answers `ENOSYS` so glibc falls back to `clone()`, whose namespace
/// flags are refused. A refused call returns `EPERM`; another architecture or an x32 call kills the
/// process. Requires `no_new_privs` or `CAP_SYS_ADMIN`.
pub fn install_seccomp() -> Result<(), String> {
    seccomp_filter().install()
}

/// The filter `install_seccomp` installs.
fn seccomp_filter() -> Filter {
    seccomp_filter_for(std::process::id())
}

/// The filter for the process `me`, whose signals are the only ones it admits.
fn seccomp_filter_for(me: u32) -> Filter {
    let mut f = Filter::default();
    f.load(SECCOMP_DATA_ARCH);
    f.jump(libc::BPF_JEQ, AUDIT_ARCH, 1, 0);
    f.ret(libc::SECCOMP_RET_KILL_PROCESS);
    f.load(SECCOMP_DATA_NR);
    // An x32 call carries the x86_64 arch with this bit set in the number, so it would match none
    // of the numbers below [arch/x86/include/uapi/asm/unistd.h __X32_SYSCALL_BIT].
    #[cfg(target_arch = "x86_64")]
    {
        f.jump(libc::BPF_JGE, 0x4000_0000, 0, 1);
        f.ret(libc::SECCOMP_RET_KILL_PROCESS);
    }

    for nr in denied_calls() {
        f.deny(nr);
    }

    // Signals reach this process and no other: the user it runs as may run other processes too,
    // other workerd processes among them. A process group or -1 is refused.
    for nr in [
        libc::SYS_kill,
        libc::SYS_tgkill,
        libc::SYS_rt_sigqueueinfo,
        libc::SYS_rt_tgsigqueueinfo,
    ] {
        f.deny_unless_arg0(nr, me);
    }

    // A descriptor's owner is where its I/O signals go, with whatever signal F_SETSIG names,
    // SIGKILL among them, and the kernel admits any process of the same user as the owner
    // [fcntl(2) F_SETOWN, F_SETSIG; linux fs/fcntl.c sigio_perm]: owning one by another pid
    // signals that process past the rule above. Nothing the runtime runs asks for I/O signals.
    for cmd in [libc::F_SETOWN, F_SETOWN_EX] {
        f.deny_if_arg1(libc::SYS_fcntl, cmd as u32);
    }
    for request in [FIOSETOWN, SIOCSPGRP] {
        f.deny_if_arg1(libc::SYS_ioctl, request);
    }

    // Descriptor-relative calls, refused against the working directory.
    let at_fdcwd = libc::AT_FDCWD as u32;
    for nr in [
        libc::SYS_openat,
        libc::SYS_openat2,
        libc::SYS_newfstatat,
        libc::SYS_statx,
        libc::SYS_mkdirat,
        libc::SYS_unlinkat,
        libc::SYS_renameat,
        libc::SYS_renameat2,
        libc::SYS_readlinkat,
        libc::SYS_fchmodat,
        libc::SYS_fchmodat2,
        libc::SYS_fchownat,
        libc::SYS_faccessat,
        libc::SYS_faccessat2,
        libc::SYS_utimensat,
        #[cfg(target_arch = "x86_64")]
        libc::SYS_futimesat,
    ] {
        f.deny_if_arg0(nr, at_fdcwd);
    }

    // Network: local UNIX domain sockets and nothing else.
    let af_unix = libc::AF_UNIX as u32;
    f.deny_unless_arg0(libc::SYS_socket, af_unix);
    f.deny_unless_arg0(libc::SYS_socketpair, af_unix);

    // Threads may be created; namespaces may not.
    let namespace_flags = (libc::CLONE_NEWUSER
        | libc::CLONE_NEWNS
        | libc::CLONE_NEWNET
        | libc::CLONE_NEWPID
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWCGROUP) as u32;
    f.deny_if_arg0_has(libc::SYS_clone, namespace_flags);
    f.deny_errno(libc::SYS_clone3, libc::ENOSYS);

    f.ret(libc::SECCOMP_RET_ALLOW);
    f
}

/// The calls refused outright: path-taking filesystem calls, extended attributes, mounts and
/// namespaces, other processes, modules and keys, and the rest.
fn denied_calls() -> Vec<i64> {
    let mut calls = vec![
        // Path-taking filesystem calls.
        libc::SYS_mknodat,
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_acct,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_linkat,
        libc::SYS_symlinkat,
        libc::SYS_name_to_handle_at,
        libc::SYS_open_by_handle_at,
        libc::SYS_inotify_init1,
        libc::SYS_inotify_add_watch,
        libc::SYS_fanotify_init,
        libc::SYS_truncate,
        libc::SYS_statfs,
        libc::SYS_chdir,
        libc::SYS_fchdir,
        libc::SYS_chroot,
        // Extended attributes, by path and by descriptor: nothing the runtime stores uses them,
        // and a descriptor handed in before the sandbox is not a way to a host inode's metadata.
        libc::SYS_setxattr,
        libc::SYS_lsetxattr,
        libc::SYS_fsetxattr,
        libc::SYS_getxattr,
        libc::SYS_lgetxattr,
        libc::SYS_fgetxattr,
        libc::SYS_listxattr,
        libc::SYS_llistxattr,
        libc::SYS_flistxattr,
        libc::SYS_removexattr,
        libc::SYS_lremovexattr,
        libc::SYS_fremovexattr,
        SYS_SETXATTRAT,
        SYS_GETXATTRAT,
        SYS_LISTXATTRAT,
        SYS_REMOVEXATTRAT,
        // Mounts and namespaces.
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_fspick,
        libc::SYS_open_tree,
        libc::SYS_move_mount,
        libc::SYS_mount_setattr,
        libc::SYS_quotactl,
        libc::SYS_sethostname,
        libc::SYS_setdomainname,
        // Other processes: tracing them, their memory, their descriptors, comparing their kernel
        // objects with this one's, and a handle on them.
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_process_madvise,
        libc::SYS_migrate_pages,
        libc::SYS_kcmp,
        libc::SYS_pidfd_open,
        libc::SYS_pidfd_getfd,
        libc::SYS_pidfd_send_signal,
        // Modules, keys, and the rest.
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_reboot,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_personality,
        libc::SYS_syslog,
        libc::SYS_lookup_dcookie,
        // A thread signalled by its id alone may be another process's: raise() and
        // pthread_kill() use tgkill(), which names this process.
        libc::SYS_tkill,
    ];
    // The calls x86_64 keeps from before the descriptor-relative ones; aarch64 never had them.
    #[cfg(target_arch = "x86_64")]
    calls.extend([
        libc::SYS_open,
        libc::SYS_creat,
        libc::SYS_stat,
        libc::SYS_lstat,
        libc::SYS_access,
        libc::SYS_readlink,
        libc::SYS_mkdir,
        libc::SYS_rmdir,
        libc::SYS_unlink,
        libc::SYS_rename,
        libc::SYS_link,
        libc::SYS_symlink,
        libc::SYS_chmod,
        libc::SYS_chown,
        libc::SYS_lchown,
        libc::SYS_mknod,
        libc::SYS_utime,
        libc::SYS_utimes,
        libc::SYS_uselib,
        libc::SYS_inotify_init,
    ]);
    calls
}

// =====================================================================================
// System calls

/// A system call's result: its error, with `what`, when it returned -1.
fn syscall<T: PartialEq + From<i8>>(what: &str, result: T) -> Result<(), String> {
    if result == T::from(-1) {
        return Err(format!("{what}: {}", io::Error::last_os_error()));
    }
    Ok(())
}

fn c_string(text: &str) -> Result<CString, String> {
    CString::new(text).map_err(|_| format!("a path holds a NUL byte: {text:?}"))
}

fn mount(
    source: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<(), String> {
    let source_c = source.map(c_string).transpose()?;
    let target_c = c_string(target)?;
    let fstype_c = fstype.map(c_string).transpose()?;
    let data_c = data.map(c_string).transpose()?;
    let ptr = |s: &Option<CString>| s.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
    // SAFETY: every pointer is null or a NUL-terminated string alive across the call.
    syscall(&format!("mount {target}"), unsafe {
        libc::mount(
            ptr(&source_c),
            target_c.as_ptr(),
            ptr(&fstype_c),
            flags,
            ptr(&data_c).cast(),
        )
    })
}

fn make_dir(path: &str) -> Result<(), String> {
    std::fs::create_dir(path).map_err(|e| format!("mkdir {path}: {e}"))
}

/// The directory `path` names, opened with `flags` (and `O_DIRECTORY | O_CLOEXEC`).
fn open_dir(path: &Path, flags: libc::c_int) -> Result<OwnedFd, String> {
    let path_c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("a path holds a NUL byte: {}", path.display()))?;
    // SAFETY: open takes a NUL-terminated path, alive across the call.
    let fd = unsafe { libc::open(path_c.as_ptr(), flags | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(format!(
            "open {}: {}",
            path.display(),
            io::Error::last_os_error()
        ));
    }
    // SAFETY: `fd` was just opened and is owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn write_proc_file(path: &str, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|e| format!("{path}: {e}"))
}

fn text(text: capnp::Result<capnp::text::Reader<'_>>) -> Result<String, String> {
    text.and_then(|t| t.to_string().map_err(Into::into))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "linux-test.rs"]
mod tests;
