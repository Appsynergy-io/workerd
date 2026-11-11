//! The platform's settings for this process (AppSynergy's apsy.capnp `Platform`): what upstream's
//! `Config` has no field for, kept in a schema of their own so that upstream's ordinals never
//! collide with them. The command line reads them (`--platform-config`, `--platform-config-fd`);
//! [`Platform::none`] is the process started without them, which runs every Worker as upstream
//! workerd does.

use std::collections::HashMap;

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
}

/// The settings, decoded.
#[derive(Debug, Default)]
pub struct Platform {
    /// The encoded message, which the C++ factory reads too (`ffi::factory_set_platform`); empty
    /// for [`Platform::none`].
    words: Vec<u64>,
    services: HashMap<String, ServiceSettings>,
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
            if services
                .insert(
                    name.clone(),
                    ServiceSettings {
                        limits,
                        rebuild_group,
                    },
                )
                .is_some()
            {
                return Err(kj::failed!(
                    "--platform-config: service \"{name}\" is given more than once"
                ));
            }
        }
        Ok(Self { words, services })
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
