// The allocator's side of a process serving many Workers: a soft limit on what tcmalloc keeps
// (apsy.capnp `Platform.mallocSoftLimit`), the collection a limited isolate is asked for while the
// process is over it, and returning what idle Workers given up freed
// (`Platform.isolateMemoryBudget`, server/lazy.rs). Without tcmalloc (`WD_USE_TCMALLOC` unset) each
// is a no-op, logged once.

#pragma once

#include <workerd/jsg/jsg.h>

#include <cstdint>

namespace workerd::server::apsy {

// Holds tcmalloc to `bytes` (MallocExtension::SetMemoryLimit, soft): what it has freed past it is
// returned as it allocates, and its background thread returns free pages at 64 MiB/s. 0 leaves
// the allocator's own behaviour [upstream: google/tcmalloc, tcmalloc/malloc_extension.h
// SetMemoryLimit, ProcessBackgroundActions].
void applyMallocSoftLimit(uint64_t bytes);

// With more than the soft limit in use through malloc, tells the isolate leaving JavaScript to
// collect at once. The C++ halves of the objects a request leaves behind (its Request, Response,
// Headers, URL) live until V8 collects their wrappers, and V8 collects by the size of its own
// heap, which does not count them: a Worker answering a few thousand requests a second held 400
// MiB of them against a 128 MiB heap. Read at most every 250 ms across the process, so a busy
// Worker asks for a collection a few times a second while the process is over the limit, and
// never while it is under.
void collectUnderMallocPressure(jsg::Lock& lock);

// Returns up to `bytes` of free memory tcmalloc keeps to the system now, rather than at its
// background release's pace (MallocExtension::ReleaseMemoryToSystem).
void releaseMemoryToSystem(size_t bytes);

}  // namespace workerd::server::apsy
