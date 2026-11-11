#pragma once

// The limits a Worker from the config runs under: CPU time and subrequests per
// invocation, memory per isolate, and script startup, for a Worker the platform's settings give
// `limits` (apsy.capnp `Platform.Service.limits`, `--platform-config`). workerd's own policy
// objects (NullIsolateLimitEnforcer, NullLimitEnforcer in worker-factory.c++) enforce none; the
// production runtime supplies its own, which is not open source. These hold a Worker to the limits
// Cloudflare documents.
//
// The decisions they rest on are apsy-limits.h's. The rebuild of a Worker whose isolate went over
// its memory limit is the server's (server/worker.rs decides when, worker_rebuild() builds); what
// the isolate knows of its own state is read through the functions below.

#include <workerd/io/limit-enforcer.h>
#include <workerd/io/worker.h>
#include <workerd/server/apsy.capnp.h>

#include <v8-isolate.h>

namespace workerd::server {

// The limits of one Worker (apsy.capnp `Platform.Service.limits`), with the platform's CPU
// rollover figures (`Platform.cpuRolloverInitialMs`, `cpuRolloverCapMs`).
struct WorkerLimits {
  uint32_t cpuMs;
  uint32_t subrequests;
  // `Limits.memoryBytes`; 0 for no memory limit.
  uint64_t memoryBytes;
  int64_t cpuRolloverInitialNanos;
  int64_t cpuRolloverCapNanos;
};

// The limits `platform` gives the config's Worker service `service`; none without settings, or
// when they give it none, which enforces no CPU, subrequest or memory limit, as upstream.
kj::Maybe<WorkerLimits> workerLimits(
    kj::Maybe<apsy::Platform::Reader> platform, kj::StringPtr service);

// What a request observer that records CPU time implements: MeasuringLimitEnforcer reports each
// invocation's CPU time to it (see RequestObserverWithTracer in worker-factory.c++).
class CpuTimeObserver {
 public:
  virtual void addCpuTime(kj::Duration time) = 0;

 protected:
  ~CpuTimeObserver() noexcept(false) = default;
};

// The IsolateLimitEnforcer of a Worker with `limits`: its isolate may hold `limits.memoryBytes`
// of JavaScript heap, ArrayBuffer and WebAssembly memory together (none when 0), and its startup
// one second of CPU time. Its CPU bank starts at `limits.cpuRolloverInitialNanos`.
kj::Own<IsolateLimitEnforcer> newLimitedIsolateLimitEnforcer(const WorkerLimits& limits);

// The isolate group, and so the pointer-compression cage, of a Worker with a memory limit: one of
// its own. Fails on a V8 built with one shared cage, which must not serve (see
// apsy-limit-enforcers.c++ CAGE_HEAP_BUDGET_BYTES).
v8::IsolateGroup newWorkerIsolateGroup();

// The LimitEnforcer of one invocation (or, for a Durable Object, one IoContext) on `worker`: it
// measures the invocation's CPU time and, under `limits`, holds it to them.
kj::Own<LimitEnforcer> newMeasuringLimitEnforcer(
    kj::Maybe<WorkerLimits> limits, bool isActor, const Worker& worker);

// A startup that went past a limit without the failure reaching upstream's error reporting (its
// termination absorbed at a microtask checkpoint, so the Worker's constructor returned): the build
// fails with it. None for a Worker without the limited enforcer.
kj::Maybe<kj::Exception> takeAbsorbedStartupFailure(const Worker& worker);

// Whether `worker`'s isolate is condemned: it has held more than its memory limit, and the next
// invocation belongs in a new isolate.
bool isCondemned(const Worker& worker);

// Marks `worker`'s isolate as one the server can build again: only such an isolate stays
// condemned, refuses the events that begin on it afterwards, and is broken.
void setReplaceable(const Worker& worker);

// The exception an invocation over its CPU or memory limit ends with.
kj::Exception memoryLimitExceeded();

// The condemned isolates of one service that it replaced and that still live: past
// MAX_CONDEMNED_GENERATIONS, the oldest is broken, so Durable Objects and WebSockets cannot pin
// one isolate per generation.
class CondemnedGenerations {
 public:
  CondemnedGenerations();
  ~CondemnedGenerations() noexcept(false);
  KJ_DISALLOW_COPY_AND_MOVE(CondemnedGenerations);

  // `old` was replaced by a new Worker.
  void retire(const Worker& old);

 private:
  struct Impl;
  kj::Own<Impl> impl;
};

}  // namespace workerd::server
