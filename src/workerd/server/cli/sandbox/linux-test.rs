use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;

use super::F_SETOWN_EX;
use super::FIOSETOWN;
use super::SIOCSPGRP;
use super::forbid_tracing;
use super::host_descriptors;
use super::seccomp_filter_for;
use super::socket_directory_parts;

/// Runs `body` in a child that installs the filter the way the sandbox does (`no_new_privs`, then
/// the filter); passes when the child exits 0, which it does when every check held. The child
/// writes what failed to stderr.
///
/// The filter names the pid it admits signals to, which the child learns only once forked, so the
/// parent builds the child's and writes it down a pipe into a filter of the same length allocated
/// before the fork: the child allocates nothing.
fn under_filter(body: impl FnOnce() -> bool) {
    let mut filter = seccomp_filter_for(0);
    let mut fds = [0; 2];
    // SAFETY: pipe2 fills the two-element array.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: the child makes only system calls and exits.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        // SAFETY: read into the filter's own bytes, prctl with integer arguments; _exit ends the
        // child at once.
        unsafe {
            let bytes = filter.as_bytes_mut();
            let mut read = 0;
            while read < bytes.len() {
                let n = libc::read(
                    fds[0],
                    bytes[read..].as_mut_ptr().cast(),
                    bytes.len() - read,
                );
                if n <= 0 {
                    libc::_exit(3);
                }
                read += n.cast_unsigned();
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 || filter.install().is_err()
            {
                libc::_exit(2);
            }
            libc::_exit(i32::from(!body()));
        }
    }
    let mut child = seccomp_filter_for(pid.cast_unsigned());
    let bytes = child.as_bytes_mut();
    // SAFETY: writes the filter's bytes, which a pipe takes whole at this size.
    let written = unsafe { libc::write(fds[1], bytes.as_ptr().cast(), bytes.len()) };
    assert_eq!(written.cast_unsigned(), bytes.len());
    // SAFETY: closes the two ends this process opened.
    unsafe {
        libc::close(fds[0]);
        libc::close(fds[1]);
    }
    let mut status = 0;
    // SAFETY: waits for the child just forked.
    assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "status {status}"
    );
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Writes `what` to stderr without allocating, for a child after fork.
fn note(what: &str) {
    // SAFETY: writes a borrowed buffer to stderr.
    unsafe {
        libc::write(libc::STDERR_FILENO, what.as_ptr().cast(), what.len());
        libc::write(libc::STDERR_FILENO, b"\n".as_ptr().cast(), 1);
    }
}

fn refused(what: &str, result: i64) -> bool {
    let ok = result == -1 && errno() == libc::EPERM;
    if !ok {
        note(what);
    }
    ok
}

fn admitted(what: &str, result: i64) -> bool {
    let ok = result >= 0;
    if !ok {
        note(what);
    }
    ok
}

#[test]
fn a_signal_reaches_this_process_and_no_other() {
    under_filter(|| {
        // SAFETY: each call takes integers only.
        unsafe {
            let parent = libc::getppid();
            let me = libc::getpid();
            let mut ok = true;
            ok &= refused("kill(parent)", libc::kill(parent, 0).into());
            ok &= refused("kill(-1)", libc::kill(-1, 0).into());
            ok &= refused("kill(0)", libc::kill(0, 0).into());
            ok &= refused(
                "tgkill(parent)",
                libc::syscall(libc::SYS_tgkill, parent, parent, 0),
            );
            ok &= refused("tkill(parent)", libc::syscall(libc::SYS_tkill, parent, 0));
            ok &= admitted("kill(self)", libc::kill(me, 0).into());
            ok &= admitted(
                "tgkill(self)",
                libc::syscall(libc::SYS_tgkill, me, libc::syscall(libc::SYS_gettid), 0),
            );
            ok
        }
    });
}

#[test]
fn no_handle_on_another_process_its_memory_or_its_descriptors() {
    under_filter(|| {
        // SAFETY: each call is refused before it reads its arguments; the buffers are the child's.
        unsafe {
            let parent = libc::getppid();
            let me = libc::getpid();
            let mut ok = true;
            ok &= refused(
                "pidfd_open(parent)",
                libc::syscall(libc::SYS_pidfd_open, parent, 0),
            );
            ok &= refused(
                "pidfd_open(self)",
                libc::syscall(libc::SYS_pidfd_open, me, 0),
            );
            ok &= refused(
                "pidfd_getfd",
                libc::syscall(libc::SYS_pidfd_getfd, -1, 0, 0),
            );
            ok &= refused(
                "pidfd_send_signal",
                libc::syscall(libc::SYS_pidfd_send_signal, -1, 0, 0, 0),
            );
            // KCMP_FILE is 0.
            ok &= refused("kcmp", libc::syscall(libc::SYS_kcmp, me, parent, 0, 0, 0));
            ok &= refused(
                "ptrace(PTRACE_TRACEME)",
                libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0),
            );
            let mut buf = [0_u8; 8];
            let mut src = [b'x'; 8];
            let local = libc::iovec {
                iov_base: buf.as_mut_ptr().cast(),
                iov_len: buf.len(),
            };
            let remote = libc::iovec {
                iov_base: src.as_mut_ptr().cast(),
                iov_len: src.len(),
            };
            ok &= refused(
                "process_vm_readv",
                libc::process_vm_readv(me, &raw const local, 1, &raw const remote, 1, 0) as i64,
            );
            ok &= refused(
                "process_vm_writev",
                libc::process_vm_writev(me, &raw const remote, 1, &raw const local, 1, 0) as i64,
            );
            ok
        }
    });
}

#[test]
fn no_extended_attribute_by_path_or_by_descriptor() {
    // SAFETY: opens the root directory.
    let dir = unsafe {
        OwnedFd::from_raw_fd(libc::open(
            c"/".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        ))
    };
    let fd = dir.as_raw_fd();
    under_filter(|| {
        let mut value = [0_u8; 64];
        let name = c"user.apsy".as_ptr();
        // SAFETY: each call is refused before it reads its arguments; the buffers are the child's.
        unsafe {
            let mut ok = true;
            ok &= refused(
                "fgetxattr",
                libc::fgetxattr(fd, name, value.as_mut_ptr().cast(), value.len()) as i64,
            );
            ok &= refused(
                "flistxattr",
                libc::flistxattr(fd, value.as_mut_ptr().cast(), value.len()) as i64,
            );
            ok &= refused(
                "fsetxattr",
                libc::fsetxattr(fd, name, b"x".as_ptr().cast(), 1, 0).into(),
            );
            ok &= refused("fremovexattr", libc::fremovexattr(fd, name).into());
            ok &= refused(
                "getxattr",
                libc::getxattr(c"/".as_ptr(), name, value.as_mut_ptr().cast(), value.len()) as i64,
            );
            ok &= refused(
                "listxattr",
                libc::listxattr(c"/".as_ptr(), value.as_mut_ptr().cast(), value.len()) as i64,
            );
            ok
        }
    });
}

/// The calls a runtime taken over would reach the kernel through first: io_uring, which runs file
/// and network operations outside every other rule here, userfaultfd, BPF programs and
/// performance counters. Each is refused before it reads its arguments.
#[test]
fn no_io_uring_userfaultfd_bpf_or_perf_events() {
    under_filter(|| {
        let mut params = [0_u8; 120];
        // SAFETY: each call is refused before it reads its arguments; the buffer is the child's.
        unsafe {
            let mut ok = true;
            ok &= refused(
                "io_uring_setup",
                libc::syscall(libc::SYS_io_uring_setup, 8, params.as_mut_ptr()),
            );
            ok &= refused(
                "io_uring_enter",
                libc::syscall(libc::SYS_io_uring_enter, -1, 0, 0, 0, 0, 0),
            );
            ok &= refused(
                "io_uring_register",
                libc::syscall(libc::SYS_io_uring_register, -1, 0, 0, 0),
            );
            ok &= refused(
                "userfaultfd",
                libc::syscall(libc::SYS_userfaultfd, libc::O_CLOEXEC),
            );
            ok &= refused(
                "bpf(BPF_PROG_LOAD)",
                libc::syscall(libc::SYS_bpf, 5, params.as_mut_ptr(), params.len()),
            );
            ok &= refused(
                "perf_event_open",
                libc::syscall(libc::SYS_perf_event_open, params.as_mut_ptr(), 0, -1, -1, 0),
            );
            ok
        }
    });
}

/// A descriptor owned by another process sends that process its I/O signals, with any signal
/// F_SETSIG names: SIGKILL to the supervisor, past the rule on kill(). No call sets an owner.
#[test]
fn no_descriptor_is_owned_by_another_process() {
    under_filter(|| {
        // SAFETY: each call takes a descriptor this child made, integers, or a pointer to the
        // child's own memory; the refused ones are refused before they read it.
        unsafe {
            let parent = libc::getppid();
            let mut pair = [0; 2];
            if libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) != 0 {
                note("socketpair");
                return false;
            }
            let fd = pair[0];
            // `struct f_owner_ex`: F_OWNER_PID and the pid [fcntl(2)].
            let owner: [libc::c_int; 2] = [1, parent];
            let mut ok = true;
            ok &= refused(
                "fcntl(F_SETOWN, parent)",
                libc::fcntl(fd, libc::F_SETOWN, parent).into(),
            );
            ok &= refused(
                "fcntl(F_SETOWN_EX, parent)",
                libc::fcntl(fd, F_SETOWN_EX, owner.as_ptr()).into(),
            );
            ok &= refused(
                "ioctl(FIOSETOWN, parent)",
                libc::ioctl(fd, FIOSETOWN as _, &raw const parent).into(),
            );
            ok &= refused(
                "ioctl(SIOCSPGRP, parent)",
                libc::ioctl(fd, SIOCSPGRP as _, &raw const parent).into(),
            );
            ok &= admitted("fcntl(F_GETFL)", libc::fcntl(fd, libc::F_GETFL).into());
            ok
        }
    });
}

#[test]
fn only_unix_sockets_no_cwd_paths_no_namespaces() {
    under_filter(|| {
        // SAFETY: each call takes integers or a static path.
        unsafe {
            let mut ok = true;
            ok &= refused(
                "socket(AF_INET)",
                libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0).into(),
            );
            ok &= refused(
                "socket(AF_NETLINK)",
                libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, 0).into(),
            );
            ok &= admitted(
                "socket(AF_UNIX)",
                libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0).into(),
            );
            ok &= refused(
                "openat(AT_FDCWD)",
                libc::openat(libc::AT_FDCWD, c"/".as_ptr(), libc::O_RDONLY).into(),
            );
            ok &= refused(
                "unshare(CLONE_NEWNET)",
                libc::unshare(libc::CLONE_NEWNET).into(),
            );
            ok &= refused(
                "clone(CLONE_NEWUSER)",
                libc::syscall(
                    libc::SYS_clone,
                    libc::CLONE_NEWUSER | libc::SIGCHLD,
                    0,
                    0,
                    0,
                    0,
                ),
            );
            ok
        }
    });
}

#[test]
fn forbid_tracing_makes_the_process_non_dumpable() {
    // SAFETY: the child makes only system calls and exits.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        let ok = forbid_tracing().is_ok()
            // SAFETY: prctl with integer arguments.
            && unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } == 0;
        // SAFETY: ends the child at once.
        unsafe { libc::_exit(i32::from(!ok)) };
    }
    let mut status = 0;
    // SAFETY: waits for the child just forked.
    assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
}

#[test]
fn host_descriptors_lists_what_reaches_the_filesystem_and_nothing_else() {
    let dir = std::fs::File::open("/").unwrap();
    let file = std::fs::File::open("/proc/self/status").unwrap();
    let (pipe_read, pipe_write) = std::io::pipe().unwrap();
    let (socket_a, socket_b) = std::os::unix::net::UnixStream::pair().unwrap();
    let fds = host_descriptors().unwrap();
    assert!(fds.contains(&dir.as_raw_fd()));
    assert!(fds.contains(&file.as_raw_fd()));
    assert!(!fds.contains(&pipe_read.as_raw_fd()));
    assert!(!fds.contains(&pipe_write.as_raw_fd()));
    assert!(!fds.contains(&socket_a.as_raw_fd()));
    assert!(!fds.contains(&socket_b.as_raw_fd()));
    assert!(!fds.contains(&libc::STDIN_FILENO));
    assert!(!fds.contains(&libc::STDERR_FILENO));
}

#[test]
fn a_socket_directory_is_absolute_and_outside_the_sandbox_own() {
    assert_eq!(
        socket_directory_parts("/run/apsy"),
        Some(vec!["run", "apsy"])
    );
    assert_eq!(socket_directory_parts("run/apsy"), None);
    assert_eq!(socket_directory_parts("/"), None);
    assert_eq!(socket_directory_parts("/run//apsy"), None);
    assert_eq!(socket_directory_parts("/run/../etc"), None);
    assert_eq!(socket_directory_parts("/disk/0"), None);
    assert_eq!(socket_directory_parts("/old/x"), None);
}

// As kj-hyper's tls-test.rs: an EC P-256 CA, an example.com certificate it signed, the key under
// both, and a self-signed example.com certificate over that key. Valid from 2026 on.
const CA_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBmzCCAUGgAwIBAgIUFtVfWCEoNJw9tRYkhBCqWkPYQAkwCgYIKoZIzj0EAwIw
GjEYMBYGA1UEAwwPd29ya2VyZCB0ZXN0IENBMCAXDTI2MDkxODEzNDcwMVoYDzIx
MjYwODI1MTM0NzAxWjAaMRgwFgYDVQQDDA93b3JrZXJkIHRlc3QgQ0EwWTATBgcq
hkjOPQIBBggqhkjOPQMBBwNCAAR0c/eq28LGrosC4Jp0m5O6/xS5vvetDh6lDWNG
LfwBXbM3O4yoeSz9pUKY4cChCSL4BMldwTbepDKMCBmVMfJDo2MwYTAdBgNVHQ4E
FgQU1HLxzohU0eGQqgstH3O0chw0cVIwHwYDVR0jBBgwFoAU1HLxzohU0eGQqgst
H3O0chw0cVIwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAgQwCgYIKoZI
zj0EAwIDSAAwRQIgYtV4qsw7p1xhZ1OSOWmRmjyc4LzQplblEr4jZmZO6BECIQCx
Al0TgxgxuWjJ4FuakSJ5qfCA2BiIliBop+phth7LKw==
-----END CERTIFICATE-----
";

const HOST_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBwzCCAWmgAwIBAgIUPPhG/ycBgb1qLtsWGGJiMhxtewAwCgYIKoZIzj0EAwIw
GjEYMBYGA1UEAwwPd29ya2VyZCB0ZXN0IENBMCAXDTI2MDkxODEzNDcwMVoYDzIx
MjYwODI1MTM0NzAxWjAWMRQwEgYDVQQDDAtleGFtcGxlLmNvbTBZMBMGByqGSM49
AgEGCCqGSM49AwEHA0IABCc5+7lyl50H3MHWYyEAgNbxnIhMc6TBtR7Wvpp6XOBg
7CzaOCZFwix4Mj8KXPoyhi7xgNQVKAgE1maTCPVPlB2jgY4wgYswDAYDVR0TAQH/
BAIwADAOBgNVHQ8BAf8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwEwFgYDVR0R
BA8wDYILZXhhbXBsZS5jb20wHQYDVR0OBBYEFOFpqeWqxNaPtlBncznOXK34Slsh
MB8GA1UdIwQYMBaAFNRy8c6IVNHhkKoLLR9ztHIcNHFSMAoGCCqGSM49BAMCA0gA
MEUCIGkJrUe5mthCxcMYy8zUKtbDuURGIS1OqeT9xqypm2nlAiEA5jYVU9d8NUV8
WBIiMYUVDYKLwUf1elA4zib/Qnms8DQ=
-----END CERTIFICATE-----
";

const SELF_SIGNED_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBnDCCAUGgAwIBAgIUS2xurXFfrYHJlrcRk6lqv44ritUwCgYIKoZIzj0EAwIw
FjEUMBIGA1UEAwwLZXhhbXBsZS5jb20wIBcNMjYwOTE4MTM0OTAwWhgPMjEyNjA4
MjUxMzQ5MDBaMBYxFDASBgNVBAMMC2V4YW1wbGUuY29tMFkwEwYHKoZIzj0CAQYI
KoZIzj0DAQcDQgAEJzn7uXKXnQfcwdZjIQCA1vGciExzpMG1Hta+mnpc4GDsLNo4
JkXCLHgyPwpc+jKGLvGA1BUoCATWZpMI9U+UHaNrMGkwHQYDVR0OBBYEFOFpqeWq
xNaPtlBncznOXK34SlshMB8GA1UdIwQYMBaAFOFpqeWqxNaPtlBncznOXK34Slsh
MA8GA1UdEwEB/wQFMAMBAf8wFgYDVR0RBA8wDYILZXhhbXBsZS5jb20wCgYIKoZI
zj0EAwIDSQAwRgIhAP8xzPMa6tuqL9p3AIKUn1eABTfZv7o/VmiEvtCK3rU2AiEA
9S5OQ1DXe5Nt5ZD5h3qOJF1IN9uR5Jb0kf1bl7lKIGI=
-----END CERTIFICATE-----
";

const HOST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgb9K+RSaSGrigmGFU
Ucyf1+0zBpj3gEnQ9LCKN9gIFhqhRANCAAQnOfu5cpedB9zB1mMhAIDW8ZyITHOk
wbUe1r6aelzgYOws2jgmRcIseDI/Clz6MoYu8YDUFSgIBNZmkwj1T5Qd
-----END PRIVATE KEY-----
";

/// A TLS handshake between `client` and a server presenting `certificate` (over HOST_KEY), for
/// `host`, in memory: no socket, no thread. Whether both sides finished it.
fn handshakes(client: &kj_hyper::tls::TlsOptions, certificate: &str, host: &str) -> bool {
    let Ok(client) = kj_hyper::tls::client_config(client) else {
        note("client_config");
        return false;
    };
    let server = kj_hyper::tls::server_config(&kj_hyper::tls::TlsOptions {
        keypair: Some(kj_hyper::tls::Keypair {
            certificate_chain: certificate.to_owned(),
            private_key: HOST_KEY.to_owned(),
        }),
        ..kj_hyper::tls::TlsOptions::default()
    })
    .unwrap();
    let name = rustls::pki_types::ServerName::try_from(host.to_owned()).unwrap();
    let mut c = rustls::ClientConnection::new(client, name).unwrap();
    let mut s = rustls::ServerConnection::new(server).unwrap();
    for _ in 0..16 {
        let mut flight = Vec::new();
        let _ = c.write_tls(&mut flight);
        if !flight.is_empty()
            && (s.read_tls(&mut flight.as_slice()).is_err() || s.process_new_packets().is_err())
        {
            return false;
        }
        let mut flight = Vec::new();
        let _ = s.write_tls(&mut flight);
        if !flight.is_empty()
            && (c.read_tls(&mut flight.as_slice()).is_err() || c.process_new_packets().is_err())
        {
            return false;
        }
        if !c.is_handshaking() && !s.is_handshaking() {
            return true;
        }
    }
    false
}

/// The platform's store is read before the filter (`load_platform_roots`, as `enter` does it);
/// inside, where the filter refuses opening the store's files, every way of trusting a server
/// still verifies it: the store with a private root beside it, a private root alone, and a pinned
/// certificate with the store.
#[test]
fn tls_verifies_inside_the_filter_with_the_store_read_before_it() {
    kj_hyper::tls::load_platform_roots();
    under_filter(|| {
        let trusting = |browser_cas: bool, trusted: &str| kj_hyper::tls::TlsOptions {
            trust_browser_cas: browser_cas,
            trusted_certificates: vec![trusted.to_owned()],
            ..kj_hyper::tls::TlsOptions::default()
        };
        let mut ok = true;
        // SAFETY: open takes a static NUL-terminated path.
        ok &= refused("open(the store)", unsafe {
            libc::open(
                c"/etc/ssl/certs/ca-certificates.crt".as_ptr(),
                libc::O_RDONLY,
            )
            .into()
        });
        let store_only = kj_hyper::tls::TlsOptions {
            trust_browser_cas: true,
            ..kj_hyper::tls::TlsOptions::default()
        };
        if kj_hyper::tls::client_config(&store_only).is_err() {
            note("the store read before the filter");
            ok = false;
        }
        ok &= !handshakes(&store_only, HOST_CERT, "example.com");
        ok &= handshakes(&trusting(true, CA_CERT), HOST_CERT, "example.com");
        ok &= !handshakes(&trusting(true, CA_CERT), HOST_CERT, "wrong.example.com");
        ok &= handshakes(&trusting(false, CA_CERT), HOST_CERT, "example.com");
        ok &= handshakes(
            &trusting(true, SELF_SIGNED_CERT),
            SELF_SIGNED_CERT,
            "example.com",
        );
        ok
    });
}
