// See apsy-memory.h.

#include "apsy-memory.h"

#include <kj/debug.h>
#include <kj/thread.h>
#include <kj/time.h>

#include <atomic>

#if WD_USE_TCMALLOC
#include <tcmalloc/malloc_extension.h>
#endif

namespace workerd::server::apsy {

namespace {
// apsy.capnp `Platform.mallocSoftLimit`, and when a Worker last read the process's malloc use
// against it.
std::atomic<uint64_t> softLimitBytes{0};
std::atomic<int64_t> pressureReadAtNanos{0};
constexpr int64_t PRESSURE_READ_EVERY_NANOS = 250'000'000;
constexpr uint64_t BACKGROUND_RELEASE_BYTES_PER_SECOND = 64ull << 20;
}  // namespace

void applyMallocSoftLimit(uint64_t bytes) {
  if (bytes == 0) return;
#if WD_USE_TCMALLOC
  softLimitBytes.store(bytes, std::memory_order_relaxed);
  tcmalloc::MallocExtension::SetMemoryLimit(bytes, tcmalloc::MallocExtension::LimitKind::kSoft);
  if (tcmalloc::MallocExtension::NeedsProcessBackgroundActions()) {
    tcmalloc::MallocExtension::SetBackgroundReleaseRate(
        static_cast<tcmalloc::MallocExtension::BytesPerSecond>(
            BACKGROUND_RELEASE_BYTES_PER_SECOND));
    kj::Thread([]() { tcmalloc::MallocExtension::ProcessBackgroundActions(); }).detach();
  }
#else
  KJ_LOG(WARNING, "mallocSoftLimit is set but this workerd was built without tcmalloc; ignored");
#endif
}

void collectUnderMallocPressure(jsg::Lock& lock) {
#if WD_USE_TCMALLOC
  auto limit = softLimitBytes.load(std::memory_order_relaxed);
  if (limit == 0) return;
  int64_t now =
      (kj::systemPreciseMonotonicClock().now() - kj::origin<kj::TimePoint>()) / kj::NANOSECONDS;
  auto last = pressureReadAtNanos.load(std::memory_order_relaxed);
  if (now - last < PRESSURE_READ_EVERY_NANOS ||
      !pressureReadAtNanos.compare_exchange_strong(last, now, std::memory_order_relaxed)) {
    return;
  }
  auto inUse = tcmalloc::MallocExtension::GetNumericProperty("generic.current_allocated_bytes");
  if (inUse.has_value() && *inUse > limit) {
    lock.v8Isolate->MemoryPressureNotification(v8::MemoryPressureLevel::kCritical);
  }
#else
  (void)lock;
#endif
}

void releaseMemoryToSystem(size_t bytes) {
#if WD_USE_TCMALLOC
  tcmalloc::MallocExtension::ReleaseMemoryToSystem(bytes);
#else
  (void)bytes;
#endif
}

}  // namespace workerd::server::apsy
