//! The platform's settings for this process (AppSynergy's apsy.capnp `Platform`): what upstream's
//! `Config` has no field for, kept in a schema of their own so that upstream's ordinals never
//! collide with them. The command line reads them (`--platform-config`, `--platform-config-fd`);
//! [`Platform::none`] is the process started without them, which runs every Worker as upstream
//! workerd does.

use std::collections::HashMap;
use std::time::Duration;

use apsy_capnp::platform;
use capnp::message::ReaderOptions;
use workerd_capnp::config;
use workerd_capnp::service;

use crate::Result;
use crate::config::Reporter;
use crate::config::capnp_error;

/// One Worker's limits (`Platform.Limits`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    pub cpu_ms: u32,
    pub subrequests: u32,
    /// Bytes the isolate may hold; 0 for no memory limit.
    pub memory_bytes: u64,
    /// Bytes each Durable Object's database may hold; 0 for no limit.
    pub durable_object_storage_bytes: u64,
}

/// The most `Limits.memoryBytes` may be: what one isolate's pointer-compression cage leaves room
/// for past the limit (factory/apsy-limit-enforcers.c++ `MAX_ISOLATE_MEMORY_LIMIT_BYTES`).
pub const MAX_MEMORY_BYTES: u64 = 1 << 30;

/// One Worker's settings (`Platform.Service`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceSettings {
    /// None: no limit, as upstream.
    pub limits: Option<Limits>,
    /// `Service.rebuildGroup`; none when unset or empty.
    pub rebuild_group: Option<String>,
    /// Built on its first request (lazy.rs).
    pub lazy: bool,
}

/// The settings, decoded.
#[derive(Debug, Default)]
pub struct Platform {
    /// The encoded message, which the C++ factory reads too (`ffi::factory_set_platform`); empty
    /// for [`Platform::none`].
    words: Vec<u64>,
    services: HashMap<String, ServiceSettings>,
    /// `Platform.durableObjectMaxQueuedRequests`; none without settings or when it is 0.
    max_queued_requests: Option<u32>,
    /// `Platform.isolateMemoryBudget`, `mallocSoftLimit` and `lazyWorkerEstimateBytes`.
    isolate_memory_budget: u64,
    malloc_soft_limit: u64,
    lazy_worker_estimate_bytes: u64,
    /// `Platform.lazyIdleMs` and `lazySweepMs`.
    lazy_idle: Duration,
    lazy_sweep: Duration,
}

impl Platform {
    /// The process started without settings.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Decodes an encoded `Platform` message, as the command line produced it.
    pub fn decode(words: Vec<u64>) -> Result<Self> {
        let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_ne_bytes()).collect();
        let message = capnp::serialize::read_message(&mut bytes.as_slice(), ReaderOptions::new())
            .map_err(capnp_error)?;
        let root = message
            .get_root::<platform::Reader>()
            .map_err(capnp_error)?;
        let mut services = HashMap::new();
        for service in root.get_services().map_err(capnp_error)? {
            let name = service
                .get_name()
                .map_err(capnp_error)?
                .to_string()
                .map_err(|e| kj::failed!("--platform-config: a service name: {e}"))?;
            let limits = if service.has_limits() {
                let limits = service.get_limits().map_err(capnp_error)?;
                if limits.get_memory_bytes() > MAX_MEMORY_BYTES {
                    return Err(kj::failed!(
                        "--platform-config: service \"{name}\": memoryBytes is at most \
                         {MAX_MEMORY_BYTES}"
                    ));
                }
                Some(Limits {
                    cpu_ms: limits.get_cpu_ms(),
                    subrequests: limits.get_subrequests(),
                    memory_bytes: limits.get_memory_bytes(),
                    durable_object_storage_bytes: limits.get_durable_object_storage_bytes(),
                })
            } else {
                None
            };
            let rebuild_group = Some(
                service
                    .get_rebuild_group()
                    .map_err(capnp_error)?
                    .to_string()
                    .map_err(|e| kj::failed!("--platform-config: a rebuildGroup: {e}"))?,
            )
            .filter(|group| !group.is_empty());
            let lazy = service.get_lazy();
            if services
                .insert(
                    name.clone(),
                    ServiceSettings {
                        limits,
                        rebuild_group,
                        lazy,
                    },
                )
                .is_some()
            {
                return Err(kj::failed!(
                    "--platform-config: service \"{name}\" is given more than once"
                ));
            }
        }
        let max_queued_requests =
            Some(root.get_durable_object_max_queued_requests()).filter(|&limit| limit > 0);
        Ok(Self {
            words,
            services,
            max_queued_requests,
            isolate_memory_budget: root.get_isolate_memory_budget(),
            malloc_soft_limit: root.get_malloc_soft_limit(),
            lazy_worker_estimate_bytes: root.get_lazy_worker_estimate_bytes(),
            lazy_idle: Duration::from_millis(root.get_lazy_idle_ms().into()),
            lazy_sweep: Duration::from_millis(root.get_lazy_sweep_ms().into()),
        })
    }

    /// The encoded message; empty when the process was started without settings.
    #[must_use]
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    /// The settings of the Worker service `name`, if the platform names it.
    #[must_use]
    pub fn service(&self, name: &str) -> Option<&ServiceSettings> {
        self.services.get(name)
    }

    /// The group whose rebuild allowance the Worker service `name` shares (rebuild.rs); none
    /// when the settings put it in none.
    #[must_use]
    pub fn rebuild_group(&self, name: &str) -> Option<&str> {
        self.service(name)?.rebuild_group.as_deref()
    }

    /// The requests stubs may have unanswered at one Durable Object before the next is refused
    /// overloaded; none for no limit, as upstream.
    #[must_use]
    pub fn max_queued_requests(&self) -> Option<u32> {
        self.max_queued_requests
    }

    /// Whether the Worker service `name` is built on its first request (lazy.rs).
    #[must_use]
    pub fn is_lazy(&self, name: &str) -> bool {
        self.service(name).is_some_and(|service| service.lazy)
    }

    /// Bytes of resident memory past which idle lazy Workers are given up; 0 for never.
    #[must_use]
    pub fn isolate_memory_budget(&self) -> u64 {
        self.isolate_memory_budget
    }

    /// Bytes the allocator holds itself to; 0 for the allocator's own behaviour.
    #[must_use]
    pub fn malloc_soft_limit(&self) -> u64 {
        self.malloc_soft_limit
    }

    /// What giving up one idle lazy Worker is counted to free, in bytes.
    #[must_use]
    pub fn lazy_worker_estimate_bytes(&self) -> u64 {
        self.lazy_worker_estimate_bytes
    }

    /// How recently a built lazy Worker must not have been asked for to be given up.
    #[must_use]
    pub fn lazy_idle(&self) -> Duration {
        self.lazy_idle
    }

    /// How often idle lazy Workers are swept against the memory budget; never less than 1 ms.
    #[must_use]
    pub fn lazy_sweep(&self) -> Duration {
        self.lazy_sweep.max(Duration::from_millis(1))
    }

    /// The bytes each Durable Object of the Worker service `name` may hold; 0 for no limit.
    #[must_use]
    pub fn durable_object_storage_bytes(&self, name: &str) -> u64 {
        self.service(name)
            .and_then(|service| service.limits)
            .map_or(0, |limits| limits.durable_object_storage_bytes)
    }

    /// Reports every service the settings name that is not a Worker service of `config`: a typo
    /// would otherwise leave a Worker without the limits the platform meant it to have.
    pub fn check_against(&self, config: config::Reader<'_>, report: &Reporter) -> Result<()> {
        let mut workers = std::collections::HashSet::new();
        for service in config.get_services().map_err(capnp_error)? {
            if let Ok(service::Which::Worker(_)) = service.which() {
                workers.insert(
                    service
                        .get_name()
                        .map_err(capnp_error)?
                        .to_string()
                        .map_err(|e| kj::failed!("{e}"))?,
                );
            }
        }
        let mut names: Vec<&String> = self.services.keys().collect();
        names.sort();
        for name in names {
            if !workers.contains(name) {
                report.error(format!(
                    "--platform-config names service \"{name}\", which the config does not \
                     define as a Worker."
                ));
            }
        }
        Ok(())
    }
}

/// `build`'s `Platform` as the command line hands it over: an encoded message in words.
#[cfg(test)]
pub(crate) fn encoded(build: impl FnOnce(platform::Builder<'_>)) -> Vec<u64> {
    let mut message = capnp::message::Builder::new_default();
    build(message.init_root::<platform::Builder>());
    capnp::serialize::write_message_to_words(&message)
        .chunks_exact(8)
        .map(|chunk| u64::from_ne_bytes(chunk.try_into().unwrap()))
        .collect()
}

#[cfg(test)]
#[path = "platform-test.rs"]
mod tests;
