//! A lazy worker (apsy.capnp `Platform.Service.lazy`, `--platform-config`): a worker from the
//! config built when first asked for, not when the process starts, and given up while idle past
//! `Platform.isolateMemoryBudget`; the next request builds it again.
//!
//! A process serving many workers built every isolate at launch: a cold start cost every
//! worker's build (5.3 s at 500 workers), and every one stayed resident (4 MiB each before
//! any request). A [`LazyWorkerService`] stands in for the [`WorkerService`] it builds: the
//! sockets and the other workers' bindings that name the worker reach the stand-in's
//! [`LazyEntrypoint`] channels, and a request arriving while the worker is being built waits for
//! it. A build's config errors fail the requests waiting on it, not the process's start.
//!
//! A lazy worker defines no Durable Object namespace and runs no Workflow, which are wired before
//! any request; the config is refused otherwise (config.rs `make_service`). Under the inspector,
//! where every isolate registers at start, every worker is built at start as before.
//!
//! [`sweep`] runs every second while the server serves: past the budget of resident memory
//! (memory.rs), idle lazy workers are given up, least recently used first, as many as the excess
//! needs at `Platform.lazyWorkerEstimateBytes` each. A worker with a request in flight, background
//! work past a response, a rebuild running, or a request in the last `Platform.lazyIdleMs` is
//! not idle. Over the budget with no idle worker left, what the given-up workers are estimated to
//! have freed and the allocator kept goes back to the system (factory/apsy-memory.h), never more:
//! the running workers' own memory is theirs.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::rc::Weak;
use std::time::Duration;

use futures::FutureExt;
use futures::future::LocalBoxFuture;
use futures::future::Shared;
use kj_rs::KjOwn;
use worker::Interface as _;
use worker::PromisedInterface;
use workerd_capnp::config;
use workerd_capnp::service;

use crate::Result;
use crate::bindings::ActorConfigs;
use crate::bridge::ffi;
use crate::channels::Channel;
use crate::channels::Frankenvalue;
use crate::channels::PendingToken;
use crate::channels::Persistent;
use crate::channels::RequestMetadata;
use crate::channels::TokenUsage;
use crate::channels::WorkerInterface;
use crate::channels::attach;
use crate::config::Factory;
use crate::config::Reporter;
use crate::config::Server;
use crate::config::capnp_error;
use crate::memory::ResidentMemory;
use crate::worker::WorkerService;

/// A build in flight, which every request arriving meanwhile waits on.
type Build = Shared<LocalBoxFuture<'static, Result<()>>>;

#[derive(Default)]
struct State {
    built: Option<Rc<WorkerService>>,
    /// The build in progress; kept until the next starts, so that it is never dropped from
    /// inside its own continuation.
    building: Option<Build>,
}

/// One worker of the config built on demand, standing in for the [`WorkerService`] it builds.
pub struct LazyWorkerService {
    this: Weak<Self>,
    factory: Rc<Factory>,
    name: String,
    /// The worker's service in the config.
    service_index: u32,
    /// Every worker's namespaces, which the worker's bindings are checked against.
    actor_configs: ActorConfigs,
    /// `--environment-fd`'s records, which `fromEnvironment` bindings read first.
    environment: HashMap<String, String>,
    inbound_listeners: Vec<ffi::InboundListener>,
    /// Set once the server exists; the build links the worker into its graph.
    server: RefCell<Weak<Server>>,
    state: RefCell<State>,
    /// The requests started and not yet ended.
    in_flight: Cell<u32>,
    /// When the worker was last asked for, by the factory timer.
    last_used: Cell<Duration>,
}

impl LazyWorkerService {
    #[must_use]
    pub fn new(
        factory: Rc<Factory>,
        name: &str,
        service_index: u32,
        actor_configs: &ActorConfigs,
        environment: &HashMap<String, String>,
        inbound_listeners: Vec<ffi::InboundListener>,
    ) -> Rc<Self> {
        let now = factory.now();
        Rc::new_cyclic(|this| Self {
            this: Weak::clone(this),
            factory,
            name: name.to_owned(),
            service_index,
            actor_configs: actor_configs.clone(),
            environment: environment.clone(),
            inbound_listeners,
            server: RefCell::new(Weak::new()),
            state: RefCell::new(State::default()),
            in_flight: Cell::new(0),
            last_used: Cell::new(now),
        })
    }

    /// Gives the worker the server whose graph its builds link into.
    pub fn set_server(&self, server: &Rc<Server>) {
        *self.server.borrow_mut() = Rc::downgrade(server);
    }

    /// The channel of an entrypoint (the default when `entrypoint` is `None`), with `props`
    /// bound. Whether the worker exports it is known once it is built: a request to one it does
    /// not fails then, as the built worker's own would.
    #[must_use]
    pub fn entrypoint(
        self: &Rc<Self>,
        entrypoint: Option<&str>,
        props: Option<KjOwn<Frankenvalue>>,
        persistent: Persistent,
    ) -> Rc<dyn Channel> {
        Rc::new(LazyEntrypoint {
            owner: Rc::clone(self),
            entrypoint: entrypoint.map(str::to_owned),
            props,
            persistent,
        })
    }

    /// The built worker, built first if it is not: `workerd test` and `bench` look for handlers
    /// in it (run.rs `start_local`).
    pub async fn built(self: &Rc<Self>) -> Result<Rc<WorkerService>> {
        if let Some(built) = self.state.borrow().built.clone() {
            return Ok(built);
        }
        self.when_built().await?;
        self.state
            .borrow()
            .built
            .clone()
            .ok_or_else(|| kj::failed!("the lazy worker was given up while built"))
    }

    /// Whether the worker is built, nothing runs in it, and it was not asked for just now.
    fn is_idle(&self) -> bool {
        let state = self.state.borrow();
        let Some(built) = &state.built else {
            return false;
        };
        self.in_flight.get() == 0
            && built.is_quiescent()
            && self.factory.now().saturating_sub(self.last_used.get())
                >= self.factory.platform().lazy_idle()
    }

    /// Gives the built worker up; the next request builds it again.
    pub fn evict(&self) {
        let built = {
            let mut state = self.state.borrow_mut();
            state.building = None;
            state.built.take()
        };
        if let Some(built) = built {
            built.unlink();
            if let Some(server) = self.server.borrow().upgrade() {
                server.report().lazy_worker(&self.name, "evicted");
            }
        }
    }

    /// Starts a request on the built worker, building it first if it is not.
    fn start_request(
        self: &Rc<Self>,
        entrypoint: Option<&str>,
        props: Option<KjOwn<Frankenvalue>>,
        persistent: Persistent,
        metadata: KjOwn<RequestMetadata>,
    ) -> Result<KjOwn<WorkerInterface>> {
        self.last_used.set(self.factory.now());
        let built = self.state.borrow().built.clone();
        let request =
            if let Some(built) = built {
                start_built(&built, entrypoint, props, persistent, metadata)?
            } else {
                let build = self.when_built();
                let this = Rc::clone(self);
                let entrypoint = entrypoint.map(str::to_owned);
                PromisedInterface::new(async move {
                    build.await?;
                    let built =
                        this.state.borrow().built.clone().ok_or_else(|| {
                            kj::failed!("the lazy worker was given up while built")
                        })?;
                    start_built(&built, entrypoint.as_deref(), props, persistent, metadata)
                })
                .into_kj()
            };
        self.in_flight.set(self.in_flight.get() + 1);
        Ok(attach(request, InFlight(Rc::clone(self))))
    }

    /// The build every request arriving now waits on: the one in progress, or a new one.
    fn when_built(&self) -> Build {
        let mut state = self.state.borrow_mut();
        if let Some(building) = &state.building
            && building.peek().is_none()
        {
            return building.clone();
        }
        let build = self
            .factory
            .spawn(build(Weak::clone(&self.this)))
            .boxed_local()
            .shared();
        state.building = Some(build.clone());
        build
    }
}

/// The worker built, linked and kept; its config errors are the build's error, which every
/// request waiting on it fails with, and the next request builds again.
async fn build(this: Weak<LazyWorkerService>) -> Result<()> {
    let lazy = this
        .upgrade()
        .ok_or_else(|| kj::disconnected!("the lazy worker is gone"))?;
    let server = lazy
        .server
        .borrow()
        .upgrade()
        .ok_or_else(|| kj::disconnected!("the server is gone"))?;
    let message = lazy.factory.config()?;
    let config = message.get_root::<config::Reader>().map_err(capnp_error)?;
    let service_conf = config
        .get_services()
        .map_err(capnp_error)?
        .get(lazy.service_index);
    let Ok(service::Which::Worker(worker_conf)) = service_conf.which() else {
        return Err(kj::failed!("service \"{}\" is not a worker", lazy.name));
    };
    let worker_conf = worker_conf.map_err(capnp_error)?;

    let errors = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&errors);
    let report = Reporter::new(
        Box::new(move |error| sink.borrow_mut().push(error)),
        Box::new(|warning| tracing::warn!("{warning}")),
        false,
    );
    let listeners = lazy
        .inbound_listeners
        .iter()
        .map(|listener| ffi::InboundListener {
            protocol: listener.protocol.clone(),
            address: listener.address.clone(),
            port: listener.port,
        })
        .collect();
    let worker = WorkerService::new(
        Rc::clone(&lazy.factory),
        &lazy.name,
        lazy.service_index,
        worker_conf,
        &lazy.actor_configs,
        &lazy.environment,
        listeners,
        &report,
    )
    .await?;
    server.link_worker_with(&lazy.name, &worker, &report)?;
    let errors = std::mem::take(&mut *errors.borrow_mut());
    if !errors.is_empty() {
        worker.unlink();
        return Err(kj::failed!("jsg.Error: {}", errors.join("; ")));
    }
    lazy.state.borrow_mut().built = Some(worker);
    server.report().lazy_worker(&lazy.name, "built");
    Ok(())
}

fn start_built(
    built: &Rc<WorkerService>,
    entrypoint: Option<&str>,
    props: Option<KjOwn<Frankenvalue>>,
    persistent: Persistent,
    metadata: KjOwn<RequestMetadata>,
) -> Result<KjOwn<WorkerInterface>> {
    let Some(channel) = built.entrypoint(entrypoint, props, persistent) else {
        return Err(kj::failed!(
            "jsg.Error: Worker has no such entrypoint: {}",
            entrypoint.unwrap_or("default")
        ));
    };
    channel.start_request(metadata)
}

/// A request of a lazy worker, counted until its interface is dropped.
struct InFlight(Rc<LazyWorkerService>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0
            .in_flight
            .set(self.0.in_flight.get().saturating_sub(1));
    }
}

/// One entrypoint of a lazy worker, as a binding from another worker or a socket holds it.
struct LazyEntrypoint {
    owner: Rc<LazyWorkerService>,
    entrypoint: Option<String>,
    props: Option<KjOwn<Frankenvalue>>,
    persistent: Persistent,
}

impl Channel for LazyEntrypoint {
    fn start_request(&self, metadata: KjOwn<RequestMetadata>) -> Result<KjOwn<WorkerInterface>> {
        let props = self.props.as_deref().map(ffi::frankenvalue_clone);
        self.owner
            .start_request(self.entrypoint.as_deref(), props, self.persistent, metadata)
    }

    fn token(&self, usage: TokenUsage) -> Result<KjOwn<PendingToken>> {
        Ok(ffi::factory_encode_subrequest_token(
            self.owner.factory.raw(),
            &self.owner.name,
            self.entrypoint.as_deref().into(),
            self.props.as_deref().into(),
            self.persistent,
            usage,
        )?)
    }

    fn for_props(
        &self,
        props: KjOwn<Frankenvalue>,
        persistent: Persistent,
    ) -> Result<Rc<dyn Channel>> {
        if self.props.is_some() {
            return Err(kj::failed!("can't override props for this service"));
        }
        Ok(self
            .owner
            .entrypoint(self.entrypoint.as_deref(), Some(props), persistent))
    }
}

/// Every `interval` (`Platform.lazySweepMs`): past `budget` bytes resident, idle lazy workers are
/// given up, least recently used first, as many as the excess needs at `estimate` bytes each
/// (`Platform.lazyWorkerEstimateBytes`). Runs until dropped (the server's drain, run.rs).
pub async fn sweep(
    server: Weak<Server>,
    budget: u64,
    estimate: u64,
    interval: Duration,
    memory: ResidentMemory,
) {
    // Bytes the given-up workers are estimated to have freed that the sweep has not yet returned
    // to the system.
    let mut freed_unreturned: u64 = 0;
    loop {
        let Some(factory) = server.upgrade().map(|server| Rc::clone(server.factory())) else {
            return;
        };
        factory.sleep(interval).await;
        drop(factory);
        let Some(server) = server.upgrade() else {
            return;
        };
        let Some(resident) = memory.bytes() else {
            continue;
        };
        if resident <= budget {
            continue;
        }
        let over = resident - budget;
        let mut freed = 0;
        while freed < over {
            let victim = server
                .lazy_workers()
                .filter(|lazy| lazy.is_idle())
                .min_by_key(|lazy| lazy.last_used.get());
            let Some(victim) = victim else {
                break;
            };
            victim.evict();
            freed += estimate.max(1);
        }
        freed_unreturned += freed;
        // Over the budget with no idle worker left: what the given-up workers freed and the
        // allocator kept goes back now rather than at the background release's pace, which
        // leaves free spans it does not reach. Only that much, and only once.
        if freed < over && freed_unreturned > 0 {
            let release = (over - freed).min(freed_unreturned);
            ffi::factory_release_memory_to_system(usize::try_from(release).unwrap_or(usize::MAX));
            freed_unreturned -= release;
        }
    }
}
