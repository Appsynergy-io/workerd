// The limits a Worker with `limits` runs under; see apsy-limit-enforcers.h.

#include "apsy-limit-enforcers.h"

#include <workerd/io/io-context.h>
#include <workerd/io/tracked-wasm-instance.h>
#include <workerd/jsg/jsg.h>
#include <workerd/server/factory/apsy-limits.h>
#include <workerd/server/factory/apsy-memory.h>
#include <workerd/util/exception.h>

#include <pthread.h>

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <memory>
#include <mutex>
#include <thread>
#include <vector>

namespace workerd::server {

namespace {

// The most memory one isolate may be held to (apsy.capnp `Limits.memoryBytes`): the limit, the
// grace and LARGEST_OBJECT_GRANT_BYTES twice over must fit CAGE_HEAP_BUDGET_BYTES.
static constexpr uint64_t MAX_ISOLATE_MEMORY_LIMIT_BYTES = 1ull << 30;

// V8 ends the process when a near-heap-limit callback does not raise the heap limit far enough
// for the allocation that reached it, and the largest single object V8 allocates is about 1 GiB (a
// FixedArray or FixedDoubleArray of FixedArray::kMaxSize, or a two-byte string of
// String::kMaxLength). A near-heap-limit call is granted that much past what the heap spans, in the
// isolate's own cage (see CAGE_HEAP_BUDGET_BYTES), so the JavaScript it terminates can unwind and
// that one allocation can succeed without taking the process down. The room is given back when the
// invocation's JavaScript scope closes (see LimitedIsolateLimitEnforcer::exitInvocationJs()).
static constexpr size_t LARGEST_OBJECT_GRANT_BYTES = (1ull << 30) + (16ull << 20);

// The least a near-heap-limit call is granted past what the heap spans when the cage has less
// room: a call V8 makes as the heap grows by ordinary allocations needs only room for the next
// few. It is also the ArrayBuffer and WebAssembly room past the limit a condemned isolate's
// in-flight invocations get.
static constexpr size_t GRACE_STEP_BYTES = 64ull << 20;

// Every Worker with a memory limit is created in a v8::IsolateGroup of its own, and so allocates
// its heap in a pointer-compression cage of 4 GiB no other isolate shares: the build sets
// `v8_enable_pointer_compression_shared_cage` to false (.bazelrc), which is what
// v8::IsolateGroup::Create() requires [upstream: v8/v8, `include/v8-isolate.h`
// `IsolateGroup::CanCreateNewGroups`]. V8's default for x64 is one cage for every isolate of the
// process, where many isolates near their limit leave no room for one near-heap-limit grant and V8
// ends the process. An isolate's own heap is held to this budget, the cage less half a GiB for what
// V8 keeps in the cage outside the heap, which holds the memory limit, the grace and
// LARGEST_OBJECT_GRANT_BYTES twice over. See grantHeapLimit().
static constexpr size_t CAGE_HEAP_BUDGET_BYTES = (3ull << 30) + (512ull << 20);

// How often the JavaScript running under a limit is interrupted to measure its isolate's memory.
// ArrayBuffer and WebAssembly memory grow outside V8's heap, and V8 collects only as the heap or
// its external-memory accounting next asks, so a tight loop growing and touching them is caught
// here rather than at the next full collection. See CpuLimitWatchdog.
static constexpr int64_t MEMORY_CHECK_INTERVAL_NANOS = 10'000'000;

// An invocation's JavaScript scope closing over the limit asks V8 for a full collection only once
// the isolate has grown this much since the last one, so garbage alone does not buy a full
// collection at every return.
static constexpr size_t ISOLATE_MEMORY_COLLECT_STEP_BYTES = 32ull << 20;

// Condemned isolates one service keeps alive for the invocations and Durable Objects still
// running in them, counting broken ones until they are freed. Condemning another breaks everything
// left in the oldest.
static constexpr uint MAX_CONDEMNED_GENERATIONS = 2;

// A Worker must parse and execute its global scope within one second.
static constexpr int64_t STARTUP_CPU_LIMIT_NANOS = 1'000'000'000;

// Cloudflare names the limit startup time, and the error for exceeding it names CPU time. The limit
// is enforced as CPU time; a startup that waits without using CPU is also ended once its scopes
// have been open this long, with the same error, so no Worker holds the build of every other
// Worker of the process behind it. Ten times the CPU limit, since a startup under
// its CPU limit on a busy machine takes longer than that limit in wall time.
static constexpr int64_t STARTUP_WALL_LIMIT_NANOS = 10'000'000'000;

// The exception an invocation over one of its limits ends with. The CPU message is the one
// Cloudflare documents; the memory message is upstream's own
// [upstream: cloudflare/workerd, `src/workerd/io/io-context.h`].
static kj::Exception limitExceededException(EventOutcome outcome) {
  if (outcome == EventOutcome::EXCEEDED_MEMORY) {
    auto e = JSG_KJ_EXCEPTION(OVERLOADED, Error, "Worker has exceeded memory limit.");
    e.setDetail(MEMORY_LIMIT_DETAIL_ID, kj::heapArray<kj::byte>(0));
    return e;
  }
  auto e = JSG_KJ_EXCEPTION(OVERLOADED, Error, "Worker exceeded CPU time limit.");
  e.setDetail(CPU_LIMIT_DETAIL_ID, kj::heapArray<kj::byte>(0));
  return e;
}

// The instant the outermost JsScope on this thread was entered at, and how deep JsScopes are
// nested on it (see jsEntryTime()).
thread_local kj::Maybe<kj::Date> jsEntryInstant;
thread_local uint jsScopeDepth = 0;

// A clock that cannot be read counts as over its limit (see apsy-limits.h).
using apsy::cpuDeadlineFrom;
using apsy::cpuDeadlineReached;
using apsy::readClockNanos;
using apsy::scopeCpuNanos;

// One thread for the whole process that watches every JavaScript execution running under a CPU
// limit, reading the CPU clock of the thread that runs it, and terminates the execution once
// that thread has used the invocation's remaining budget. V8 permits TerminateExecution() from
// any thread. An execution on a memory-limited isolate is also interrupted every
// MEMORY_CHECK_INTERVAL_NANOS of wall time with `memoryCheck`, which runs on the isolate's own
// thread (v8::Isolate::RequestInterrupt() may be called from any thread).
class CpuLimitWatchdog {
 public:
  struct Entry {
    clockid_t clock;
    // False when the running thread's CPU clock could not be found or read: the watchdog then
    // terminates the execution at once, as one over its limit.
    bool clockReadable = true;
    // INT64_MAX when the execution has no CPU limit.
    int64_t deadlineNanos = INT64_MAX;
    // CLOCK_MONOTONIC time past which the execution is terminated; INT64_MAX for none.
    int64_t wallDeadlineNanos = INT64_MAX;
    v8::Isolate* isolate;
    bool fired = false;
    v8::InterruptCallback memoryCheck = nullptr;
    void* memoryCheckData = nullptr;
    int64_t nextMemoryCheckNanos = 0;
  };

  static int64_t wallNanos() {
    return KJ_ASSERT_NONNULL(readClockNanos(CLOCK_MONOTONIC), "CLOCK_MONOTONIC cannot be read");
  }

  static CpuLimitWatchdog& get() {
    // Never destroyed: the thread outlives static destruction at process exit.
    static CpuLimitWatchdog* instance = new CpuLimitWatchdog();
    return *instance;
  }

  void add(Entry& entry) {
    {
      std::lock_guard<std::mutex> guard(mutex);
      entries.push_back(&entry);
    }
    wake.notify_one();
  }

  // Stops watching `entry`. Returns true if the watchdog terminated its execution. Holding the
  // mutex means no termination can be issued for this entry after this returns.
  bool remove(Entry& entry) {
    std::lock_guard<std::mutex> guard(mutex);
    for (auto it = entries.begin(); it != entries.end(); ++it) {
      if (*it == &entry) {
        entries.erase(it);
        break;
      }
    }
    return entry.fired;
  }

 private:
  CpuLimitWatchdog() {
    std::thread([this]() { run(); }).detach();
  }

  void run() {
    std::unique_lock<std::mutex> lock(mutex);
    for (;;) {
      if (entries.empty()) {
        wake.wait(lock);
        continue;
      }
      // A thread cannot use CPU time faster than wall time passes, so sleeping for the smallest
      // remaining budget never oversleeps a deadline.
      int64_t sleepNanos = 100'000'000;
      int64_t wall = wallNanos();
      for (auto* entry: entries) {
        if (entry->fired) continue;
        if (entry->memoryCheck != nullptr) {
          if (wall >= entry->nextMemoryCheckNanos) {
            entry->isolate->RequestInterrupt(entry->memoryCheck, entry->memoryCheckData);
            entry->nextMemoryCheckNanos = wall + MEMORY_CHECK_INTERVAL_NANOS;
          }
          sleepNanos = kj::min(sleepNanos, entry->nextMemoryCheckNanos - wall);
        }
        if (entry->wallDeadlineNanos != INT64_MAX) {
          if (wall >= entry->wallDeadlineNanos) {
            entry->fired = true;
            entry->isolate->TerminateExecution();
            continue;
          }
          sleepNanos = kj::min(sleepNanos, entry->wallDeadlineNanos - wall);
        }
        if (entry->deadlineNanos == INT64_MAX) continue;
        kj::Maybe<int64_t> now = kj::none;
        if (entry->clockReadable) now = readClockNanos(entry->clock);
        if (cpuDeadlineReached(now, entry->deadlineNanos)) {
          entry->fired = true;
          entry->isolate->TerminateExecution();
        } else {
          sleepNanos = kj::min(sleepNanos, entry->deadlineNanos - KJ_ASSERT_NONNULL(now));
        }
      }
      sleepNanos = kj::max(sleepNanos, static_cast<int64_t>(200'000));
      wake.wait_for(lock, std::chrono::nanoseconds(sleepNanos));
    }
  }

  std::mutex mutex;
  std::condition_variable wake;
  std::vector<Entry*> entries;
};

// An invocation, WebSocket request or Durable Object bound to a limited isolate, which can be told
// to stop because the isolate is being given up (see IsolateMemoryState::breakAll()).
class MemoryLimitedContext {
 public:
  virtual void breakForMemory() = 0;

 protected:
  ~MemoryLimitedContext() noexcept(false) = default;
};

// What one limited isolate's memory accounting shares between its IsolateLimitEnforcer, its
// ArrayBuffer allocator, the invocations running in it and the service that builds it. Shared
// by std::shared_ptr because V8 keeps the allocator alive until the last backing store is freed,
// which can be after the enforcer is gone.
struct IsolateMemoryState {
  // Set once the isolate exists, in customizeIsolate().
  v8::Isolate* isolate = nullptr;
  // Memory the isolate may use, its JavaScript heap and its ArrayBuffer and WebAssembly memory
  // together (apsy.capnp `Limits.memoryBytes`, set by newLimitedIsolateLimitEnforcer()); 0 for
  // none, when nothing below acts on memory.
  size_t memoryLimitBytes = 0;
  // JavaScript scopes open on this isolate that a memory hit terminates: an invocation's, and the
  // startup, dynamic import, logging and inspector scopes. Main thread only.
  uint openJsScopes = 0;
  // Times JavaScript running on this isolate was terminated for memory. Each scope compares it
  // before and after to learn whether it was.
  std::atomic<uint64_t> limitHits{0};
  // Set when the server can build this isolate's Worker again (setReplaceable()). Only a
  // replaceable isolate stays condemned, refuses events that begin after, and is broken: one whose
  // Worker cannot be rebuilt would otherwise refuse every request after one overrun. Main thread
  // only.
  bool replaceable = false;
  // Set once the isolate has held more than memoryLimitBytes: the server builds a
  // new Worker, in a new isolate, for the next request, and the invocations already running here
  // finish within the grace. A Worker that cannot be rebuilt has it cleared again once a full
  // collection finds the isolate back under the limit.
  std::atomic<bool> condemned{false};
  // Set when a condemned isolate is found over its limit again: its in-flight invocations used
  // the grace, and everything still in it is broken at the next point that can (see breakAll()).
  std::atomic<bool> graceUsed{false};
  // Set once breakAll() has run: no invocation runs JavaScript here again.
  std::atomic<bool> broken{false};
  // Bytes of ArrayBuffer backing store currently allocated.
  std::atomic<size_t> arrayBufferBytes{0};
  // How far V8's old-generation limit for this isolate stands above memoryLimitBytes, as
  // last granted or observed (see noteHeapLimit()).
  std::atomic<size_t> grantedBytes{0};
  // heap_size_limit() less the old-generation limit: the young generation's part, as the isolate's
  // creation found it. Main thread only.
  size_t heapSizeLimitOverhead = 0;
  // What the isolate held after its last full collection. Main thread only.
  size_t bytesAfterLastCollection = 0;
  // Set when the isolate's IsolateLimitEnforcer is destroyed, with the isolate. Main thread only.
  bool disposed = false;
  // The CPU time invocations on this isolate may run past their limit, which invocations under it
  // refill; a fresh isolate starts with the platform's initial allowance (apsy.capnp
  // `Platform.cpuRolloverInitialMs`, set by newLimitedIsolateLimitEnforcer()). Main thread only.
  int64_t cpuBankNanos = 0;
  // Every context whose LimitEnforcer is bound to this isolate. Main thread only.
  kj::Vector<MemoryLimitedContext*> contexts;

  size_t ceiling() const {
    if (memoryLimitBytes == 0) return SIZE_MAX;
    return condemned.load(std::memory_order_relaxed) && !broken.load(std::memory_order_relaxed)
        ? memoryLimitBytes + GRACE_STEP_BYTES + grantedBytes.load(std::memory_order_relaxed)
        : memoryLimitBytes;
  }

  size_t overMemoryLimit(size_t bytes) const {
    return bytes > memoryLimitBytes ? bytes - memoryLimitBytes : 0;
  }

  void addContext(MemoryLimitedContext& context) {
    contexts.add(&context);
  }

  void removeContext(MemoryLimitedContext& context) {
    for (size_t i = 0; i < contexts.size(); i++) {
      if (contexts[i] == &context) {
        contexts[i] = contexts.back();
        contexts.removeLast();
        return;
      }
    }
  }

  // Gives the isolate up: every invocation, WebSocket request and Durable Object still bound to
  // it ends with `exceededMemory`, and JavaScript any of them tries to run here is terminated
  // before it starts. Only from the main thread outside garbage collection, since it rejects
  // promises: CondemnedGenerations calls it, and a JavaScript scope as it closes.
  void breakAll() {
    condemned.store(true, std::memory_order_relaxed);
    if (broken.exchange(true, std::memory_order_relaxed)) return;
    auto snapshot = kj::heapArray<MemoryLimitedContext*>(contexts.asPtr());
    for (auto* context: snapshot) {
      context->breakForMemory();
    }
  }

  // Condemns the isolate; condemned already, its grace is used.
  void condemn() {
    if (condemned.exchange(true, std::memory_order_relaxed)) {
      graceUsed.store(true, std::memory_order_relaxed);
    }
  }

  // Terminates the JavaScript running on this isolate, if any scope that may be terminated is
  // open: a termination requested while none is would stay pending and kill the next one,
  // whoever's it is. V8 permits TerminateExecution() from any context.
  void terminateRunningJs() {
    if (openJsScopes == 0 || isolate == nullptr) return;
    limitHits.fetch_add(1, std::memory_order_relaxed);
    isolate->TerminateExecution();
  }
};

// Records how far `limit`, the isolate's old-generation limit, stands above the memory limit: the
// room a near-heap-limit grant left, until V8 restores the initial limit.
static void noteHeapLimit(IsolateMemoryState& state, size_t limit) {
  state.grantedBytes.store(state.overMemoryLimit(limit), std::memory_order_relaxed);
}

// The old-generation limit a near-heap-limit call sets for `state`, whose limit is `currentLimit`
// and whose heap spans `held` bytes: room for V8's largest object past both, within the isolate's
// own cage (see CAGE_HEAP_BUDGET_BYTES), and never less than GRACE_STEP_BYTES past them, since V8
// ends the process when the limit is not raised for the allocation that reached it.
static size_t grantHeapLimit(IsolateMemoryState& state, size_t currentLimit, size_t held) {
  size_t base = kj::max(currentLimit, held);
  size_t limit = kj::max(
      kj::min(base + LARGEST_OBJECT_GRANT_BYTES, CAGE_HEAP_BUDGET_BYTES), base + GRACE_STEP_BYTES);
  noteHeapLimit(state, limit);
  return limit;
}

// The ArrayBuffer allocator of a limited isolate: V8's default allocator, which refuses an
// allocation that would take the isolate's ArrayBuffer memory past its ceiling. V8 answers a
// refusal by collecting garbage (which runs the full-GC check below) and retrying, then by
// throwing `RangeError: Array buffer allocation failed` in JavaScript, never by aborting. Must
// not call into V8.
class LimitedArrayBufferAllocator final: public v8::ArrayBuffer::Allocator {
 public:
  explicit LimitedArrayBufferAllocator(std::shared_ptr<IsolateMemoryState> state)
      : state(kj::mv(state)),
        inner(v8::ArrayBuffer::Allocator::NewDefaultAllocator()) {}

  void* Allocate(size_t length) override {
    if (!reserve(length)) return nullptr;
    return track(inner->Allocate(length), length);
  }

  void* AllocateUninitialized(size_t length) override {
    if (!reserve(length)) return nullptr;
    return track(inner->AllocateUninitialized(length), length);
  }

  void Free(void* data, size_t length) override {
    inner->Free(data, length);
    if (data != nullptr) apsy::releaseBytes(state->arrayBufferBytes, length);
  }

  // No one ArrayBuffer, nor a resizable one's maximum length, is larger than the memory limit and
  // the grace together: V8 refuses those with its own RangeError before reserving anything, so a
  // single builtin cannot grow and touch more than that before the next memory check.
  size_t MaxAllocationSize() const override {
    return kj::min(inner->MaxAllocationSize(), state->memoryLimitBytes + GRACE_STEP_BYTES);
  }

  v8::PageAllocator* GetPageAllocator() override {
    return inner->GetPageAllocator();
  }

 private:
  std::shared_ptr<IsolateMemoryState> state;
  std::unique_ptr<v8::ArrayBuffer::Allocator> inner;

  // A zero-length store is never refused: V8's own API allocates those with no failure mode.
  // The bytes are taken in the same atomic step that checks them against the ceiling, before the
  // allocation (see apsy::reserveBytes()).
  bool reserve(size_t length) {
    return apsy::reserveBytes(state->arrayBufferBytes, length, state->ceiling());
  }

  // Gives back what reserve() took for an allocation that failed.
  void* track(void* data, size_t length) {
    if (data == nullptr) apsy::releaseBytes(state->arrayBufferBytes, length);
    return data;
  }
};

// IsolateLimitEnforcer for a Worker the platform's settings give `limits`: with a memory limit
// (`Limits.memoryBytes`), the isolate may hold that much JavaScript heap, ArrayBuffer and
// WebAssembly memory together.
// It is checked after every full garbage collection, which V8 runs as the heap or the
// external memory grows, and when an invocation's JavaScript scope ends; V8's own heap limit is
// set to the same size. The JavaScript running when the isolate is found over the limit is
// terminated (`exceededMemory` for an invocation; a failed build for a Worker's startup) and the
// isolate is condemned: invocations already running in it complete within the grace, events that
// begin in it afterwards are terminated as they start, and the server starts the next request
// in a new isolate (server/worker.rs). Found over the limit only when a scope ends, the invocation has completed and
// the isolate is condemned. Found over the limit again once condemned, everything still in it is
// broken. A Worker the server cannot build again is never refused or broken: its invocations
// over the limit are terminated, and its isolate is no longer condemned once a collection finds it
// under the limit. Without a memory limit none of that applies. Script startup is held to
// STARTUP_CPU_LIMIT_NANOS of CPU time either way. Everything else behaves as
// NullIsolateLimitEnforcer.
class LimitedIsolateLimitEnforcer final: public IsolateLimitEnforcer {
 public:
  ~LimitedIsolateLimitEnforcer() noexcept(false) {
    state->disposed = true;
  }

  v8::Isolate::CreateParams getCreateParams() override {
    v8::Isolate::CreateParams params;
    // Without a memory limit the isolate is created as upstream's (NullIsolateLimitEnforcer).
    if (state->memoryLimitBytes == 0) return params;
    params.constraints.set_max_old_generation_size_in_bytes(state->memoryLimitBytes);
    params.array_buffer_allocator_shared = std::make_shared<LimitedArrayBufferAllocator>(state);
    return params;
  }

  void customizeIsolate(v8::Isolate* isolate) override {
    state->isolate = isolate;
    if (state->memoryLimitBytes == 0) return;
    {
      v8::HeapStatistics stats;
      isolate->GetHeapStatistics(&stats);
      state->heapSizeLimitOverhead = stats.heap_size_limit() > state->memoryLimitBytes
          ? stats.heap_size_limit() - state->memoryLimitBytes
          : 0;
    }
    // A limit a near-heap-limit call raised goes back to the memory limit once a full collection
    // leaves the old generation under half of it, which ends the grace a grant gave.
    isolate->AutomaticallyRestoreInitialHeapLimit(0.5);
    isolate->AddNearHeapLimitCallback(&nearHeapLimit, state.get());
    isolate->AddGCEpilogueCallback(&afterFullGc, state.get(), v8::kGCTypeMarkSweepCompact);
  }

  ActorCacheSharedLruOptions getActorCacheLruOptions() override {
    return {.softLimit = 16 * (1ull << 20),
      .hardLimit = 128 * (1ull << 20),
      .staleTimeout = 30 * kj::SECONDS,
      .dirtyListByteLimit = 8 * (1ull << 20),
      .maxKeysPerRpc = 128,
      .neverFlush = true};
  }

  // Every startup scope of the isolate draws on one budget: the global scope's compilation and
  // its evaluation together get STARTUP_CPU_LIMIT_NANOS.
  kj::Own<void> enterStartupJs(
      jsg::Lock& lock, kj::OneOf<kj::Exception, kj::Duration>& limitErrorOrTime) const override {
    return kj::heap<StartupScope>(*this, lock.v8Isolate, limitErrorOrTime);
  }

  kj::Own<void> enterStartupPython(
      jsg::Lock& lock, kj::OneOf<kj::Exception, kj::Duration>& limitErrorOrTime) const override {
    return kj::heap<MemoryScope>(*state, limitErrorOrTime, absorbedStartupFailure);
  }

  kj::Own<void> enterDynamicImportJs(
      jsg::Lock& lock, kj::OneOf<kj::Exception, kj::Duration>& limitErrorOrTime) const override {
    return kj::heap<MemoryScope>(*state, limitErrorOrTime, kj::none);
  }

  kj::Own<void> enterLoggingJs(
      jsg::Lock& lock, kj::OneOf<kj::Exception, kj::Duration>& limitErrorOrTime) const override {
    return kj::heap<MemoryScope>(*state, limitErrorOrTime, kj::none);
  }

  kj::Own<void> enterInspectorJs(
      jsg::Lock& loc, kj::OneOf<kj::Exception, kj::Duration>& limitErrorOrTime) const override {
    return kj::heap<MemoryScope>(*state, limitErrorOrTime, kj::none);
  }

  // A startup that went past a limit without the failure reaching upstream's error reporting:
  // its termination was absorbed at a microtask checkpoint, so the Worker's constructor returned
  // normally. Upstream's check for that in tryResolveMainModule() runs before the startup scope
  // closes, which is where the failure is known, and setting it any earlier would have that check
  // throw while the exception of a startup that did fail is still propagating. The server fails
  // the build with it instead.
  kj::Maybe<kj::Exception> takeAbsorbedStartupFailure() const {
    return kj::mv(absorbedStartupFailure);
  }

  void completedRequest(kj::StringPtr id) const override {}

  bool exitJs(jsg::Lock& lock) const override {
    apsy::collectUnderMallocPressure(lock);
    return false;
  }

  void reportMetrics(IsolateObserver& isolateMetrics) const override {}

  kj::Maybe<size_t> checkPbkdfIterations(jsg::Lock& lock, size_t iterations) const override {
    return kj::none;
  }

  std::shared_ptr<IsolateMemoryState> memoryState() const {
    return state;
  }

  bool hasExcessivelyExceededHeapLimit() const override {
    return false;
  }

  const TrackedWasmInstanceList& getTrackedWasmInstances() const override {
    return trackedWasmInstances;
  }

  uint64_t limitHits() const {
    return state->limitHits.load(std::memory_order_relaxed);
  }

  bool isCondemned() const {
    return state->condemned.load(std::memory_order_relaxed);
  }

  bool isBroken() const {
    return state->broken.load(std::memory_order_relaxed);
  }

  bool isReplaceable() const {
    return state->replaceable;
  }

  void enterInvocationJs() const {
    ++state->openJsScopes;
  }

  // Terminates the JavaScript an invocation's scope is about to run, counted as a memory hit.
  void refuseInvocationJs() const {
    state->terminateRunningJs();
  }

  // Closes an invocation's JavaScript scope, still inside the CPU time it is charged. The room a
  // near-heap-limit grant left past what the heap holds is given back: V8 sets the limit to the
  // memory limit or a quarter past the heap's size, whichever is more, and grantedBytes follows
  // it. An isolate that used its grace is broken here, one whose Worker
  // cannot be built again included, since its heap would otherwise grow by a grant at every
  // invocation. One grown ISOLATE_MEMORY_COLLECT_STEP_BYTES past its last full collection and over
  // the limit gets a full collection, and if that confirms it, afterFullGc() condemns it without
  // terminating anything.
  void exitInvocationJs() const {
    --state->openJsScopes;
    auto* isolate = state->isolate;
    if (isolate == nullptr || state->memoryLimitBytes == 0) return;
    if (state->openJsScopes == 0) {
      if (state->grantedBytes.load(std::memory_order_relaxed) > 0) {
        isolate->RemoveNearHeapLimitCallback(&nearHeapLimit, state->memoryLimitBytes);
        isolate->AddNearHeapLimitCallback(&nearHeapLimit, state.get());
      }
      v8::HeapStatistics stats;
      isolate->GetHeapStatistics(&stats);
      noteHeapLimit(*state, stats.heap_size_limit() - state->heapSizeLimitOverhead);
    }
    if (state->graceUsed.load(std::memory_order_relaxed)) {
      state->breakAll();
      return;
    }
    if (isCondemned()) return;
    size_t used = usedBytes(*isolate);
    if (used > state->memoryLimitBytes &&
        used > state->bytesAfterLastCollection + ISOLATE_MEMORY_COLLECT_STEP_BYTES) {
      isolate->LowMemoryNotification();
    }
  }

 private:
  TrackedWasmInstanceList trackedWasmInstances;
  std::shared_ptr<IsolateMemoryState> state = std::make_shared<IsolateMemoryState>();
  // CPU time the isolate's startup scopes have used. Only touched with the lock held.
  mutable int64_t startupCpuUsedNanos = 0;
  // Wall time the isolate's startup scopes have been open. Only touched with the lock held.
  mutable int64_t startupWallUsedNanos = 0;
  // See takeAbsorbedStartupFailure(). Only touched with the lock held.
  mutable kj::Maybe<kj::Exception> absorbedStartupFailure;

  // Keeps a startup scope's failure when the scope closes with no exception in flight, which
  // means nothing upstream reports it.
  static void recordStartupFailure(
      kj::Maybe<kj::Exception>& absorbed, kj::OneOf<kj::Exception, kj::Duration>& error) {
    if (std::uncaught_exceptions() > 0 || absorbed != kj::none) return;
    KJ_IF_SOME(e, error.tryGet<kj::Exception>()) {
      absorbed = e.clone();
    }
  }

  // A JavaScript scope outside any invocation (startup, dynamic import, logging, the inspector)
  // that a memory hit terminates, failing it with the memory error as it closes. A startup's
  // failure is also kept when no exception is in flight (see takeAbsorbedStartupFailure()).
  class MemoryScope {
   public:
    MemoryScope(IsolateMemoryState& state,
        kj::OneOf<kj::Exception, kj::Duration>& error,
        kj::Maybe<kj::Maybe<kj::Exception>&> absorbed)
        : state(state),
          error(error),
          absorbed(absorbed),
          limitHitsAtStart(state.limitHits.load(std::memory_order_relaxed)) {
      ++state.openJsScopes;
    }

    ~MemoryScope() noexcept(false) {
      --state.openJsScopes;
      if (state.limitHits.load(std::memory_order_relaxed) == limitHitsAtStart) return;
      if (state.isolate != nullptr && !state.isolate->IsExecutionTerminating()) {
        // The JavaScript returned before V8 acted on the termination, which would otherwise stay
        // pending and terminate whatever this isolate runs next.
        state.isolate->CancelTerminateExecution();
      }
      if (!error.is<kj::Exception>()) {
        error = limitExceededException(EventOutcome::EXCEEDED_MEMORY);
      }
      KJ_IF_SOME(slot, absorbed) {
        recordStartupFailure(slot, error);
      }
    }

    KJ_DISALLOW_COPY_AND_MOVE(MemoryScope);

   private:
    IsolateMemoryState& state;
    kj::OneOf<kj::Exception, kj::Duration>& error;
    kj::Maybe<kj::Maybe<kj::Exception>&> absorbed;
    uint64_t limitHitsAtStart;
  };

  class StartupScope {
   public:
    StartupScope(const LimitedIsolateLimitEnforcer& enforcer,
        v8::Isolate* isolate,
        kj::OneOf<kj::Exception, kj::Duration>& limitErrorOrTime)
        : enforcer(enforcer),
          limitErrorOrTime(limitErrorOrTime),
          memory(*enforcer.state, limitErrorOrTime, enforcer.absorbedStartupFailure) {
      // The watchdog reads this thread's CPU clock from its own thread, so it needs the clock's id;
      // CLOCK_THREAD_CPUTIME_ID would read the watchdog's.
      entry.clockReadable = pthread_getcpuclockid(pthread_self(), &entry.clock) == 0;
      entry.isolate = isolate;
      startNanos = readClockNanos(CLOCK_THREAD_CPUTIME_ID);
      kj::Maybe<int64_t> clockNow = kj::none;
      if (entry.clockReadable) clockNow = readClockNanos(entry.clock);
      if (clockNow == kj::none) entry.clockReadable = false;
      entry.deadlineNanos =
          cpuDeadlineFrom(clockNow, STARTUP_CPU_LIMIT_NANOS - enforcer.startupCpuUsedNanos);
      startWallNanos = CpuLimitWatchdog::wallNanos();
      entry.wallDeadlineNanos = startWallNanos +
          kj::max(
              STARTUP_WALL_LIMIT_NANOS - enforcer.startupWallUsedNanos, static_cast<int64_t>(0));
      enforcer.watchMemory(entry);
      CpuLimitWatchdog::get().add(entry);
    }

    ~StartupScope() noexcept(false) {
      bool fired = CpuLimitWatchdog::get().remove(entry);
      // A startup whose CPU time cannot be measured has used all of its limit.
      int64_t used = STARTUP_CPU_LIMIT_NANOS;
      KJ_IF_SOME(measured, scopeCpuNanos(startNanos, readClockNanos(CLOCK_THREAD_CPUTIME_ID))) {
        used = measured;
      } else {
        fired = true;
      }
      enforcer.startupCpuUsedNanos += used;
      enforcer.startupWallUsedNanos +=
          kj::max(CpuLimitWatchdog::wallNanos() - startWallNanos, static_cast<int64_t>(0));
      if (fired) {
        if (!entry.isolate->IsExecutionTerminating()) {
          entry.isolate->CancelTerminateExecution();
        }
        if (!limitErrorOrTime.is<kj::Exception>()) {
          limitErrorOrTime =
              JSG_KJ_EXCEPTION(OVERLOADED, Error, "Script startup exceeded CPU time limit");
        }
        recordStartupFailure(enforcer.absorbedStartupFailure, limitErrorOrTime);
      } else if (!limitErrorOrTime.is<kj::Exception>()) {
        limitErrorOrTime = used * kj::NANOSECONDS;
      }
    }

    KJ_DISALLOW_COPY_AND_MOVE(StartupScope);

   private:
    const LimitedIsolateLimitEnforcer& enforcer;
    kj::OneOf<kj::Exception, kj::Duration>& limitErrorOrTime;
    CpuLimitWatchdog::Entry entry;
    kj::Maybe<int64_t> startNanos;
    int64_t startWallNanos = 0;
    // Destroyed after the destructor's body: a memory hit is looked at after the CPU limit.
    MemoryScope memory;
  };

  // The JavaScript heap, the ArrayBuffer backing stores (HeapStatistics::external_memory()) and
  // the external memory V8 accounts to the isolate, which includes each WebAssembly.Memory's
  // backing store (V8 counts those through the Managed<BackingStore> of the memory object, not
  // as ArrayBuffer backing stores).
  static size_t usedBytes(v8::Isolate& isolate, v8::HeapStatistics& stats) {
    return stats.used_heap_size() + stats.external_memory() +
        static_cast<size_t>(kj::max(isolate.GetExternalMemory(), static_cast<int64_t>(0)));
  }

  static size_t usedBytes(v8::Isolate& isolate) {
    v8::HeapStatistics stats;
    isolate.GetHeapStatistics(&stats);
    return usedBytes(isolate, stats);
  }

  // Runs on the isolate's thread between two steps of the JavaScript a CpuLimitWatchdog entry
  // watches: the same check as after a full collection, without one.
  static void checkMemoryNow(v8::Isolate* isolate, void* data) {
    auto& state = *static_cast<IsolateMemoryState*>(data);
    if (state.openJsScopes == 0) return;
    if (usedBytes(*isolate) <= state.ceiling()) return;
    state.condemn();
    state.terminateRunningJs();
  }

 public:
  // Has `entry` interrupt the JavaScript it watches to measure this isolate's memory.
  void watchMemory(CpuLimitWatchdog::Entry& entry) const {
    if (state->memoryLimitBytes == 0) return;
    entry.memoryCheck = &checkMemoryNow;
    entry.memoryCheckData = state.get();
    entry.nextMemoryCheckNanos = CpuLimitWatchdog::wallNanos() + MEMORY_CHECK_INTERVAL_NANOS;
  }

 private:
  static void afterFullGc(v8::Isolate* isolate, v8::GCType, v8::GCCallbackFlags, void* data) {
    auto& state = *static_cast<IsolateMemoryState*>(data);
    v8::HeapStatistics stats;
    isolate->GetHeapStatistics(&stats);
    size_t used = usedBytes(*isolate, stats);
    state.bytesAfterLastCollection = used;
    noteHeapLimit(state, stats.heap_size_limit() - state.heapSizeLimitOverhead);
    if (used <= state.memoryLimitBytes && !state.replaceable &&
        !state.broken.load(std::memory_order_relaxed)) {
      state.condemned.store(false, std::memory_order_relaxed);
      state.graceUsed.store(false, std::memory_order_relaxed);
    }
    if (used <= state.ceiling()) return;
    state.condemn();
    state.terminateRunningJs();
  }

  // V8 calls this when the heap reaches its limit, and before a last-resort allocation. Every call
  // terminates the JavaScript running, condemns the isolate (a second call on a condemned one
  // means the grace was used, so the isolate is broken when that invocation's scope closes), and
  // raises the limit by what grantHeapLimit() grants: room for V8's largest object past what the
  // heap spans, in the isolate's own cage. It never declines: a callback that does not raise the
  // limit ends the process. What bounds the heap instead is that no JavaScript starts on a
  // condemned isolate's new events or a broken one's.
  static size_t nearHeapLimit(void* data, size_t currentHeapLimit, size_t initialHeapLimit) {
    auto& state = *static_cast<IsolateMemoryState*>(data);
    state.terminateRunningJs();
    state.condemn();
    size_t held = kj::max(currentHeapLimit, initialHeapLimit);
    if (state.isolate != nullptr) {
      v8::HeapStatistics stats;
      state.isolate->GetHeapStatistics(&stats);
      // The committed size of every space, never less than the old generation's capacity V8
      // compares the new limit with.
      held = kj::max(held, stats.total_heap_size());
    }
    return grantHeapLimit(state, currentHeapLimit, held);
  }
};

// LimitEnforcer for one invocation (or, for a Durable Object, one IoContext, topped up at each
// event). It measures the CPU time of the thread across every enterJs() scope and reports it to
// the invocation's CpuTimeObserver, and, when the platform's settings give the Worker `limits`,
// terminates JavaScript past `cpuMs`, attributes a memory-limit hit to the invocation, and counts
// subrequests against `subrequests`. Bound to a limited isolate, it can be broken with it.
// Everything it does not measure or limit behaves as workerd's unlimited enforcer, which it
// replaces.
class MeasuringLimitEnforcer final: public LimitEnforcer, private MemoryLimitedContext {
 public:
  MeasuringLimitEnforcer(kj::Maybe<WorkerLimits> limits,
      bool isActor,
      kj::Maybe<const LimitedIsolateLimitEnforcer&> isolateEnforcer)
      : cpuLimitNanos(
            limits.map([](WorkerLimits l) { return static_cast<int64_t>(l.cpuMs) * 1'000'000; })
                .orDefault(0)),
        subrequestLimit(limits.map([](WorkerLimits l) { return l.subrequests; }).orDefault(0)),
        cpuRolloverCapNanos(
            limits.map([](WorkerLimits l) { return l.cpuRolloverCapNanos; }).orDefault(0)),
        isActor(isActor) {
    auto paf = kj::newPromiseAndFulfiller<void>();
    exceededFulfiller = kj::mv(paf.fulfiller);
    exceededPromise = paf.promise.fork();
    KJ_IF_SOME(e, isolateEnforcer) {
      memory = e.memoryState();
      memory->addContext(*this);
      eventBeganCondemned = memory->condemned.load(std::memory_order_relaxed);
    }
  }

  ~MeasuringLimitEnforcer() noexcept(false) {
    bankUnusedCpu();
    if (memory != nullptr) memory->removeContext(*this);
  }

  KJ_DISALLOW_COPY_AND_MOVE(MeasuringLimitEnforcer);

  kj::Own<void> enterJs(jsg::Lock& lock, IoContext& context) override {
    return kj::heap<JsScope>(*this, lock.v8Isolate,
        kj::tryDowncast<const LimitedIsolateLimitEnforcer>(
            context.getWorker().getIsolate().getLimitEnforcer()));
  }

  // IoContext calls this for every event delivered to a Durable Object, whose IoContext outlives
  // each event, and WebSocket and RPC-target code calls it for every message; limit-enforcer.h
  // says it does nothing when the IoContext is not an actor [upstream: cloudflare/workerd,
  // `src/workerd/io/limit-enforcer.h` `topUpActor`]. So an object's event gets a fresh CPU budget
  // and fresh subrequest counts, while a stateless Worker's WebSocket messages all draw on the
  // request's one budget, the limits being per invocation. The
  // event that ends here banks what it left unused, as an invocation does when its enforcer goes.
  void topUpActor() override {
    if (!isActor) return;
    bankUnusedCpu();
    cpuUsedNanos = 0;
    cpuDrawnNanos = 0;
    externalSubrequests = 0;
    inHouseSubrequests = 0;
    if (memory != nullptr) {
      eventBeganCondemned = memory->condemned.load(std::memory_order_relaxed);
    }
  }

  void newSubrequest(bool isInHouse) override {
    auto& count = isInHouse ? inHouseSubrequests : externalSubrequests;
    ++count;
    if (subrequestLimit > 0 && count > subrequestLimit) {
      JSG_FAIL_REQUIRE(Error, "Too many subrequests.");
    }
  }
  void newKvRequest(KvOpType op) override {}
  void newAnalyticsEngineRequest() override {}
  kj::Promise<void> limitDrain() override {
    return kj::NEVER_DONE;
  }
  kj::Promise<void> limitScheduled() override {
    return kj::NEVER_DONE;
  }
  kj::Duration getAlarmLimit() override {
    return 15 * kj::MINUTES;
  }
  // A body read whole into memory (Response.arrayBuffer(), text(), a tee) is held to the
  // isolate's memory limit, and past it fails with upstream's `Memory limit would be exceeded
  // before EOF.`, the runtime error Cloudflare documents for buffering a body over the limit.
  size_t getBufferingLimit() override {
    return memory != nullptr && memory->memoryLimitBytes > 0 ? memory->memoryLimitBytes
                                                             : kj::maxValue;
  }
  kj::Maybe<EventOutcome> getLimitsExceeded() override {
    return exceeded;
  }
  kj::Promise<void> onLimitsExceeded() override {
    return exceededPromise.addBranch();
  }
  void setCpuLimitNearlyExceededCallback(kj::Function<void(void)> cb) override {}
  void requireLimitsNotExceeded() override {
    KJ_IF_SOME(outcome, exceeded) {
      kj::throwFatalException(limitExceededException(outcome));
    }
  }
  void reportMetrics(RequestObserver& requestMetrics) override {
    // A sibling interface of the observer, not a subclass, so a cross-cast.
    if (auto* observer = dynamic_cast<CpuTimeObserver*>(&requestMetrics)) {
      observer->addCpuTime(cpuUnreportedNanos * kj::NANOSECONDS);
    }
    cpuUnreportedNanos = 0;
  }
  kj::Duration consumeTimeElapsedForPeriodicLogging() override {
    return 0 * kj::SECONDS;
  }
  size_t getSqliteMemoryUsage() const override {
    return 0;
  }

 private:
  class JsScope {
   public:
    JsScope(MeasuringLimitEnforcer& enforcer,
        v8::Isolate* isolate,
        kj::Maybe<const LimitedIsolateLimitEnforcer&> isolateEnforcer)
        : enforcer(enforcer),
          isolateEnforcer(isolateEnforcer) {
      if (jsScopeDepth++ == 0) jsEntryInstant = kj::systemPreciseCalendarClock().now();
      entry.clockReadable = pthread_getcpuclockid(pthread_self(), &entry.clock) == 0;
      entry.isolate = isolate;
      startNanos = readClockNanos(CLOCK_THREAD_CPUTIME_ID);
      KJ_IF_SOME(e, isolateEnforcer) {
        limitHitsAtStart = e.limitHits();
        e.enterInvocationJs();
        // An event that began after its isolate was condemned is a request sends to a new
        // isolate, and a broken isolate runs nothing. A Worker that cannot be built again has no
        // new isolate to send it to, and runs it.
        if (e.isBroken() ||
            (e.isReplaceable() && e.isCondemned() && enforcer.eventBeganCondemned)) {
          e.refuseInvocationJs();
        }
      }
      enforcer.ranJs = true;
      if (enforcer.cpuLimitNanos > 0) {
        kj::Maybe<int64_t> clockNow = kj::none;
        if (entry.clockReadable) clockNow = readClockNanos(entry.clock);
        if (clockNow == kj::none) entry.clockReadable = false;
        // The invocation's own remainder and what its isolate's bank holds.
        entry.deadlineNanos = cpuDeadlineFrom(clockNow,
            apsy::cpuAllowanceNanos(enforcer.cpuLimitNanos, enforcer.cpuUsedNanos,
                enforcer.cpuDrawnNanos, enforcer.cpuBank()));
      }
      KJ_IF_SOME(e, isolateEnforcer) {
        e.watchMemory(entry);
      }
      if (enforcer.cpuLimitNanos > 0 || entry.memoryCheck != nullptr) {
        CpuLimitWatchdog::get().add(entry);
        watched = true;
      }
    }

    ~JsScope() noexcept(false) {
      KJ_DEFER(if (--jsScopeDepth == 0) jsEntryInstant = kj::none);
      // First, so a full collection it asks for is charged to this invocation's CPU time.
      bool memoryHit = false;
      KJ_IF_SOME(e, isolateEnforcer) {
        e.exitInvocationJs();
        memoryHit = e.limitHits() != limitHitsAtStart;
      }
      bool cpuFired = watched && CpuLimitWatchdog::get().remove(entry);
      // An invocation whose CPU time cannot be measured has used what was left of its limit, and
      // one under a limit is over it.
      int64_t used =
          kj::max(enforcer.cpuLimitNanos - enforcer.cpuUsedNanos, static_cast<int64_t>(0));
      bool measured = false;
      KJ_IF_SOME(cpu, scopeCpuNanos(startNanos, readClockNanos(CLOCK_THREAD_CPUTIME_ID))) {
        used = cpu;
        measured = true;
      }
      if (!measured && enforcer.cpuLimitNanos > 0) {
        cpuFired = true;
      }
      enforcer.cpuUsedNanos += used;
      enforcer.cpuUnreportedNanos += used;
      if (enforcer.cpuLimitNanos > 0 && enforcer.memory != nullptr) {
        apsy::cpuDrawOverrun(enforcer.cpuLimitNanos, enforcer.cpuUsedNanos, enforcer.cpuDrawnNanos,
            enforcer.memory->cpuBankNanos);
      }
      if ((cpuFired || memoryHit) && !entry.isolate->IsExecutionTerminating()) {
        // The execution returned before V8 acted on the termination, which would otherwise stay
        // pending and terminate whatever JavaScript this isolate runs next.
        entry.isolate->CancelTerminateExecution();
      }
      if (memoryHit) {
        enforcer.markExceeded(EventOutcome::EXCEEDED_MEMORY);
      } else if (cpuFired) {
        enforcer.markExceeded(EventOutcome::EXCEEDED_CPU);
      }
    }

    KJ_DISALLOW_COPY_AND_MOVE(JsScope);

   private:
    MeasuringLimitEnforcer& enforcer;
    kj::Maybe<const LimitedIsolateLimitEnforcer&> isolateEnforcer;
    CpuLimitWatchdog::Entry entry;
    kj::Maybe<int64_t> startNanos;
    uint64_t limitHitsAtStart = 0;
    bool watched = false;
  };

  void breakForMemory() override {
    markExceeded(EventOutcome::EXCEEDED_MEMORY);
  }

  // What the isolate's bank holds; none without a limited isolate, which has no bank.
  int64_t cpuBank() const {
    return memory != nullptr ? memory->cpuBankNanos : 0;
  }

  // Banks what the event ending now left unused of its CPU limit: only an event that ran
  // JavaScript and was not ended for a limit.
  void bankUnusedCpu() {
    if (ranJs && cpuLimitNanos > 0 && exceeded == kj::none && memory != nullptr) {
      apsy::cpuCreditUnused(cpuLimitNanos, cpuUsedNanos, cpuRolloverCapNanos, memory->cpuBankNanos);
    }
    ranJs = false;
  }

  void markExceeded(EventOutcome outcome) {
    if (exceeded != kj::none) return;
    exceeded = outcome;
    exceededFulfiller->reject(limitExceededException(outcome));
  }

  int64_t cpuLimitNanos;
  uint32_t subrequestLimit;
  int64_t cpuRolloverCapNanos;
  bool isActor;
  std::shared_ptr<IsolateMemoryState> memory;
  // Whether the isolate was already condemned when the current event began.
  bool eventBeganCondemned = false;
  int64_t cpuUsedNanos = 0;
  // What the current event has taken from its isolate's CPU bank.
  int64_t cpuDrawnNanos = 0;
  // Whether the current event has entered JavaScript.
  bool ranJs = false;
  int64_t cpuUnreportedNanos = 0;
  uint32_t externalSubrequests = 0;
  uint32_t inHouseSubrequests = 0;
  kj::Maybe<EventOutcome> exceeded;
  kj::Own<kj::PromiseFulfiller<void>> exceededFulfiller;
  kj::ForkedPromise<void> exceededPromise = nullptr;
};

kj::Maybe<const LimitedIsolateLimitEnforcer&> limitedEnforcerOf(const Worker& worker) {
  return kj::tryDowncast<const LimitedIsolateLimitEnforcer>(worker.getIsolate().getLimitEnforcer());
}

}  // namespace

kj::Maybe<kj::Date> jsEntryTime() {
  return jsEntryInstant;
}

kj::Maybe<WorkerLimits> workerLimits(
    kj::Maybe<apsy::Platform::Reader> platform, kj::StringPtr service) {
  KJ_IF_SOME(p, platform) {
    for (auto settings: p.getServices()) {
      if (settings.getName() != service || !settings.hasLimits()) continue;
      auto limits = settings.getLimits();
      // The server refuses such settings before any Worker is compiled (platform.rs).
      KJ_REQUIRE(limits.getMemoryBytes() <= MAX_ISOLATE_MEMORY_LIMIT_BYTES,
          "memoryBytes is past the most an isolate's cage holds", service);
      return WorkerLimits{
        .cpuMs = limits.getCpuMs(),
        .subrequests = limits.getSubrequests(),
        .memoryBytes = limits.getMemoryBytes(),
        .cpuRolloverInitialNanos = static_cast<int64_t>(p.getCpuRolloverInitialMs()) * 1'000'000,
        .cpuRolloverCapNanos = static_cast<int64_t>(p.getCpuRolloverCapMs()) * 1'000'000,
      };
    }
  }
  return kj::none;
}

kj::Own<IsolateLimitEnforcer> newLimitedIsolateLimitEnforcer(const WorkerLimits& limits) {
  auto enforcer = kj::refcounted<LimitedIsolateLimitEnforcer>();
  enforcer->memoryState()->memoryLimitBytes = limits.memoryBytes;
  enforcer->memoryState()->cpuBankNanos = limits.cpuRolloverInitialNanos;
  return enforcer;
}

// Every Worker with a memory limit is created in a v8::IsolateGroup of its own (see
// CAGE_HEAP_BUDGET_BYTES). A V8 built with one shared cage cannot make one, and serving every
// Worker from that cage is what lets one isolate's allocation end the process, so such a build
// refuses to serve.
v8::IsolateGroup newWorkerIsolateGroup() {
  KJ_REQUIRE(v8::IsolateGroup::CanCreateNewGroups(),
      "this V8 is built with one pointer-compression cage for every isolate; build it with "
      "v8_enable_pointer_compression_shared_cage set to false");
  return v8::IsolateGroup::Create();
}

kj::Own<LimitEnforcer> newMeasuringLimitEnforcer(
    kj::Maybe<WorkerLimits> limits, bool isActor, const Worker& worker) {
  return kj::heap<MeasuringLimitEnforcer>(limits, isActor, limitedEnforcerOf(worker));
}

kj::Maybe<kj::Exception> takeAbsorbedStartupFailure(const Worker& worker) {
  KJ_IF_SOME(e, limitedEnforcerOf(worker)) {
    return e.takeAbsorbedStartupFailure();
  }
  return kj::none;
}

bool isCondemned(const Worker& worker) {
  KJ_IF_SOME(e, limitedEnforcerOf(worker)) {
    return e.isCondemned();
  }
  return false;
}

void setReplaceable(const Worker& worker) {
  KJ_IF_SOME(e, limitedEnforcerOf(worker)) {
    e.memoryState()->replaceable = true;
  }
}

kj::Exception memoryLimitExceeded() {
  return limitExceededException(EventOutcome::EXCEEDED_MEMORY);
}

// The memory state of condemned Workers this service replaced and not yet freed, broken or not,
// oldest first.
struct CondemnedGenerations::Impl {
  kj::Vector<std::shared_ptr<IsolateMemoryState>> generations;
};

CondemnedGenerations::CondemnedGenerations(): impl(kj::heap<Impl>()) {}
CondemnedGenerations::~CondemnedGenerations() noexcept(false) = default;

// A broken isolate still counts until it is freed, so one that outlives its break gets the
// next-oldest broken too.
void CondemnedGenerations::retire(const Worker& old) {
  KJ_IF_SOME(e, limitedEnforcerOf(old)) {
    kj::Vector<std::shared_ptr<IsolateMemoryState>> live;
    for (auto& generation: impl->generations) {
      if (!generation->disposed) live.add(kj::mv(generation));
    }
    live.add(e.memoryState());
    size_t excess =
        live.size() > MAX_CONDEMNED_GENERATIONS ? live.size() - MAX_CONDEMNED_GENERATIONS : 0;
    for (size_t i = 0; i < excess; i++) {
      live[i]->breakAll();
    }
    impl->generations = kj::mv(live);
  }
}

}  // namespace workerd::server
