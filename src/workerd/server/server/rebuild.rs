//! When a worker whose isolate went over its memory limit is built again.
//!
//! A worker with a memory limit (apsy.capnp `Limits.memoryBytes`, Cloudflare's 128 MB) holds
//! its isolate to it (factory/apsy-limit-enforcers.h). Found
//! over it, the isolate is condemned: the invocations already running in it finish within a
//! grace, and the next request belongs in a new isolate. The factory builds one
//! (`worker_rebuild`); this module decides when. The next request that is not a Durable Object's
//! event waits on the build, which runs as a background task on the event loop, never inside the
//! caller's own JavaScript (a service binding or `ctx.exports` call reaches the worker holding
//! another isolate's lock); requests arriving meanwhile share it. Durable Objects already running
//! stay in the isolate they were constructed in; a new one waits too (`ActorClass::when_ready`).
//!
//! Rebuilding runs a startup on the one thread every isolate shares, so it is rationed:
//!
//! - A build that fails, or whose startup alone condemns its isolate, is served as that failure
//!   for a backoff doubling from one second to a minute; then the next request builds again.
//! - Each build adds `REBUILD_COST_FACTOR` times its wall time to its service's rebuild debt and
//!   to its group's (apsy.capnp `Service.rebuildGroup`), and `PROCESS_REBUILD_COST_FACTOR` times
//!   to the process's, all repaid by the wall time that passes. While any is past
//!   `REBUILD_DEBT_ALLOWANCE` the service is not rebuilt, and its requests run in the condemned
//!   isolate, which ends them with `exceededMemory` before their JavaScript starts. So the
//!   services of one group together spend at most a tenth of the thread rebuilding past a first
//!   second, and five groups doing so reach the process's allowance, which holds rebuilding to
//!   half the thread: one group's services cannot keep another's condemned worker from being
//!   rebuilt. A service in no group is held to its own debt and the process's.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::rc::Weak;
use std::time::Duration;
use std::time::Instant;

use futures::FutureExt;
use futures::future::LocalBoxFuture;
use futures::future::Shared;

use crate::Result;
use crate::bridge::ffi;
use crate::config::Factory;
use crate::worker::WorkerService;

/// What a build costs its service's and its group's debt, per unit of its wall time.
const REBUILD_COST_FACTOR: u32 = 10;
/// What a build costs the process's debt, per unit of its wall time.
const PROCESS_REBUILD_COST_FACTOR: u32 = 2;
/// Past this debt the service is not rebuilt.
const REBUILD_DEBT_ALLOWANCE: Duration = Duration::from_secs(10);
/// How long a failed build is served as its failure before the next request builds again,
/// doubling up to `REBUILD_BACKOFF_MAX`.
const REBUILD_BACKOFF_FIRST: Duration = Duration::from_secs(1);
const REBUILD_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Wall time owed for rebuilding, as of `at` (the factory timer's reading), which wall time
/// repays.
#[derive(Default)]
struct Debt {
    debt: Duration,
    at: Duration,
}

impl Debt {
    fn repay(&mut self, now: Duration) {
        let repaid = now.saturating_sub(self.at);
        self.at = now;
        self.debt = self.debt.saturating_sub(repaid);
    }

    fn charge(&mut self, cost: Duration, now: Duration) {
        self.repay(now);
        self.debt += cost;
    }

    fn past_allowance(&mut self, now: Duration) -> bool {
        self.repay(now);
        self.debt > REBUILD_DEBT_ALLOWANCE
    }
}

thread_local! {
    /// What every service of the process owes together.
    static PROCESS_DEBT: RefCell<Debt> = RefCell::new(Debt::default());
    /// What the services of each rebuild group owe together.
    static GROUP_DEBTS: RefCell<HashMap<String, Debt>> = RefCell::new(HashMap::new());
}

/// Whether `group`'s debt is past the allowance; never for a service in no group.
fn group_past_allowance(group: Option<&str>, now: Duration) -> bool {
    group.is_some_and(|group| {
        GROUP_DEBTS.with_borrow_mut(|debts| {
            debts
                .entry(group.to_owned())
                .or_default()
                .past_allowance(now)
        })
    })
}

/// Charges `cost` to `group`'s debt, if the service is in one.
fn charge_group(group: Option<&str>, cost: Duration, now: Duration) {
    if let Some(group) = group {
        GROUP_DEBTS.with_borrow_mut(|debts| {
            debts.entry(group.to_owned()).or_default().charge(cost, now);
        });
    }
}

/// A build in flight, which every request arriving meanwhile waits on.
type Build = Shared<LocalBoxFuture<'static, Result<()>>>;

#[derive(Default)]
struct State {
    building: Option<Build>,
    /// The last build's failure, served while the worker stays condemned and the backoff runs.
    failure: Option<crate::Error>,
    retry_at: Duration,
    backoff: Duration,
    debt: Debt,
}

/// One worker service's rebuilds.
pub struct Rebuild {
    /// The group whose debt the service shares (`Service.rebuildGroup`); none for none.
    group: Option<String>,
    state: RefCell<State>,
}

impl Rebuild {
    #[must_use]
    pub fn new(group: Option<String>) -> Self {
        Self {
            group,
            state: RefCell::new(State {
                backoff: REBUILD_BACKOFF_FIRST,
                ..State::default()
            }),
        }
    }

    /// What the next request that is not a Durable Object's event must wait on before it starts:
    /// a build of `service`'s worker, when its isolate is condemned and a build is allowed. None
    /// when it starts at once: in the current worker, or as the last build's failure
    /// (`failure`).
    pub fn when_ready(&self, service: &Rc<WorkerService>) -> Option<Build> {
        let factory = service.factory();
        let worker = service.compiled();
        if !ffi::worker_rebuildable(worker) {
            return None;
        }
        let mut state = self.state.borrow_mut();
        if let Some(building) = &state.building {
            if building.peek().is_none() {
                return Some(building.clone());
            }
            state.building = None;
        }
        if !ffi::worker_condemned(worker) {
            return None;
        }
        let now = factory.now();
        if state.failure.is_some() && now < state.retry_at {
            return None;
        }
        let group_past = group_past_allowance(self.group.as_deref(), now);
        let process_past = PROCESS_DEBT.with_borrow_mut(|debt| debt.past_allowance(now));
        if state.debt.past_allowance(now) || group_past || process_past {
            return None;
        }
        let build = factory
            .spawn(build(Rc::downgrade(service)))
            .boxed_local()
            .shared();
        state.building = Some(build.clone());
        Some(build)
    }

    /// Whether a build is running now.
    pub fn in_progress(&self) -> bool {
        self.state
            .borrow()
            .building
            .as_ref()
            .is_some_and(|building| building.peek().is_none())
    }

    /// The last build's failure, while the worker it would have replaced is still condemned.
    pub fn failure(&self, worker: &ffi::CompiledWorker) -> Option<crate::Error> {
        let state = self.state.borrow();
        let failure = state.failure.as_ref()?;
        ffi::worker_condemned(worker).then(|| failure.clone())
    }

    /// Records a build that took `took` and ended as `result`.
    fn ended(&self, factory: &Factory, took: Duration, result: &Result<()>) {
        let now = factory.now();
        let mut state = self.state.borrow_mut();
        state.debt.charge(took * REBUILD_COST_FACTOR, now);
        charge_group(self.group.as_deref(), took * REBUILD_COST_FACTOR, now);
        PROCESS_DEBT.with_borrow_mut(|debt| debt.charge(took * PROCESS_REBUILD_COST_FACTOR, now));
        match result {
            Ok(()) => {
                state.failure = None;
                state.backoff = REBUILD_BACKOFF_FIRST;
            }
            Err(error) => {
                state.failure = Some(error.clone());
                state.retry_at = now + state.backoff;
                state.backoff = (state.backoff * 2).min(REBUILD_BACKOFF_MAX);
            }
        }
    }
}

/// Builds `service`'s worker again, from the event loop with no isolate lock held.
async fn build(service: Weak<WorkerService>) -> Result<()> {
    let Some(service) = service.upgrade() else {
        return Err(kj::failed!("the worker's service is gone"));
    };
    let started = Instant::now();
    let result = ffi::worker_rebuild(service.compiled()).map_err(crate::Error::from);
    if let Err(error) = &result {
        tracing::error!(
            "could not replace a Worker whose isolate exceeded its memory limit: {}: {}",
            service.transferable_name().unwrap_or("(dynamic)"),
            error.description()
        );
    }
    service
        .rebuild()
        .ended(service.factory(), started.elapsed(), &result);
    result
}

#[cfg(test)]
#[path = "rebuild-test.rs"]
mod tests;
