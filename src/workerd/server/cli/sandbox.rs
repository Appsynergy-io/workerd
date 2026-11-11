//! `--sandbox`: the layer-2 sandbox.
//!
//! Cloudflare's runtime process, after it has started and before any isolate is loaded, enters
//! Linux namespaces and applies a seccomp filter: it has a totally empty filesystem (its own mount
//! namespace), no network, and communicates only over local UNIX domain sockets, a supervisor
//! outside it fetching code and configuration. workerd has no such mode; this adds one, in the
//! process itself, once the config is loaded and before the server's event loop and V8 start, while
//! the process is still single-threaded (a user namespace needs that).
//!
//! [`enter`]:
//!
//! 1. finds every disk directory the config names (Durable Object storage, assets: the
//!    supervisor hands the runtime its storage, and an object's storage stays on the machine that
//!    runs it) and every directory holding a socket file an external service dials, while
//!    paths still resolve;
//! 2. binds every socket the config names, which must be a UNIX domain socket: bound and not yet
//!    listening, handed to the server as `RunOptions::prebound`, which listens once its services
//!    are built, so a supervisor that connects knows the process serves;
//! 3. opens the server's disk filesystem (its descriptors on `/` and the working directory, which
//!    the filter would refuse later) and reads the platform's TLS roots; notes every descriptor it
//!    holds on the host's filesystem; unshares a user, mount, network, IPC and UTS namespace;
//!    makes every mount private; bind-mounts each disk directory under `disk/<n>` and each socket
//!    directory read-only at the path the config names it by, in a read-only tmpfs holding nothing
//!    else; pivots into it and detaches the old root; points every noted descriptor at that root in
//!    place; and names each disk directory to the server by its path there (a `--directory-path`
//!    override), so `..` from any of them ends at the tmpfs, handing it the directory opened there,
//!    which the disk service reaches its entries relative to;
//! 4. drops every capability and its bounding set, sets `no_new_privs`, makes itself non-dumpable,
//!    and installs the seccomp filter (`install_seccomp`).
//!
//! A refused call answers `EPERM` rather than killing the process: a library probing for a file it
//! does not need gets an error. No PID namespace: after `unshare(CLONE_NEWPID)` the process can
//! create no thread, and one that forks into it is no longer the process its supervisor signals;
//! the seccomp rules keep its signals to itself, and it has no `/proc`. Not combined with
//! `--watch`: nothing is watched once the filesystem is gone.

#![allow(
    unsafe_code,
    reason = "the sandbox is made of system calls libc declares and std does not wrap"
)]

#[cfg(target_os = "linux")]
#[path = "sandbox/linux.rs"]
mod imp;

#[cfg(target_os = "linux")]
pub use imp::enter;

/// `--sandbox` elsewhere: refused.
#[cfg(not(target_os = "linux"))]
pub fn enter(
    _config: &crate::config::Config,
    _options: &mut workerd_server::run::RunOptions,
) -> Result<(), String> {
    Err("--sandbox is implemented on Linux only".to_owned())
}
