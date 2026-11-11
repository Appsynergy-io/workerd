#include "apsy-commit-sync.h"

#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <kj/debug.h>
#include <kj/mutex.h>
#include <kj/thread.h>

#include <atomic>
#include <cerrno>
#include <deque>

namespace workerd::server::apsy {

void deferCommitSync(SqliteDatabase& db) {
  db.run({.regulator = SqliteDatabase::TRUSTED}, "PRAGMA synchronous=NORMAL;");
}

namespace {

// See CommitSync::inFlight().
std::atomic<uint64_t> syncsInFlight{0};

// The threads every CommitSync of the process syncs on. They are started on first use and live
// as long as the process: a sync in flight when it exits has answered nothing yet, so no response
// waited on it.
class SyncPool {
 public:
  static SyncPool& get() {
    static SyncPool* pool = new SyncPool();
    return *pool;
  }

  void submit(kj::Function<void()> job) {
    state.lockExclusive()->jobs.push_back(kj::mv(job));
  }

 private:
  // Syncs of different files proceed in parallel on the disk; one database has at most one in
  // flight, so a few threads serve many objects.
  static constexpr uint THREADS = 4;

  struct State {
    std::deque<kj::Function<void()>> jobs;
  };
  kj::MutexGuarded<State> state;

  SyncPool() {
    for (uint i = 0; i < THREADS; i++) {
      kj::heap<kj::Thread>([this]() { run(); })->detach();
    }
  }

  [[noreturn]] void run() {
    for (;;) {
      auto job = state.when([](const State& s) { return !s.jobs.empty(); }, [](State& s) {
        auto job = kj::mv(s.jobs.front());
        s.jobs.pop_front();
        return job;
      });
      job();
    }
  }
};

}  // namespace

struct CommitSync::Target: public kj::AtomicRefcounted {
  // The database's directory, duplicated so a sync in flight outlives the CommitSync that began
  // it. Null when the directory has no descriptor (an in-memory one), which holds nothing a sync
  // could make durable.
  kj::AutoCloseFd directory;
  kj::String walName;
  // The log file (device and inode) the directory was last synced for, and how many times it
  // has been synced. SQLite at synchronous=FULL syncs the directory on the first sync of a log it
  // has just created, so that the file's entry survives a power loss; at NORMAL it never does, so
  // this does it once per new log: after a reset, and after the object was evicted and its log
  // checkpointed and removed.
  mutable std::atomic<uint64_t> syncedDevice{0};
  mutable std::atomic<uint64_t> syncedInode{0};
  mutable std::atomic<uint64_t> directorySyncs{0};

  Target(kj::AutoCloseFd directory, kj::String walName)
      : directory(kj::mv(directory)),
        walName(kj::mv(walName)) {}

  // Runs on a pool thread. The log is opened by name each time: a reset (deleteAll()) replaces
  // the file, and fdatasync() syncs the file, whichever descriptor names it.
  kj::Maybe<kj::Exception> syncNow() const {
    int fd;
    do {
      fd = ::openat(directory.get(), walName.cStr(), O_RDONLY | O_CLOEXEC);
    } while (fd < 0 && errno == EINTR);
    if (fd < 0) {
      // No log: its last commit was checkpointed into the database, and a checkpoint syncs.
      if (errno == ENOENT) return kj::none;
      return KJ_EXCEPTION(FAILED, "opening the write-ahead log to sync it failed", walName, errno);
    }
    kj::AutoCloseFd log(fd);
    int result;
    do {
      result = ::fdatasync(fd);
    } while (result < 0 && errno == EINTR);
    if (result < 0) {
      return KJ_EXCEPTION(FAILED, "syncing the write-ahead log failed", walName, errno);
    }
    struct stat st;
    if (::fstat(fd, &st) < 0) {
      return KJ_EXCEPTION(FAILED, "reading the write-ahead log's inode failed", walName, errno);
    }
    if (static_cast<uint64_t>(st.st_dev) != syncedDevice.load() ||
        static_cast<uint64_t>(st.st_ino) != syncedInode.load()) {
      do {
        result = ::fsync(directory.get());
      } while (result < 0 && errno == EINTR);
      if (result < 0) {
        return KJ_EXCEPTION(
            FAILED, "syncing the write-ahead log's directory failed", walName, errno);
      }
      syncedDevice.store(st.st_dev);
      syncedInode.store(st.st_ino);
      directorySyncs.fetch_add(1);
    }
    return kj::none;
  }
};

CommitSync::CommitSync(const kj::Directory& directory, kj::String walName) {
  kj::AutoCloseFd dup;
  KJ_IF_SOME(fd, directory.getFd()) {
    int copy;
    KJ_SYSCALL(copy = ::fcntl(fd, F_DUPFD_CLOEXEC, 0));
    dup = kj::AutoCloseFd(copy);
  }
  target = kj::atomicRefcounted<Target>(kj::mv(dup), kj::mv(walName));
}

CommitSync::~CommitSync() noexcept(false) {}

uint64_t CommitSync::directorySyncs() const {
  return target->directorySyncs.load();
}

uint64_t CommitSync::inFlight() {
  return syncsInFlight.load(std::memory_order_acquire);
}

kj::Promise<void> CommitSync::sync() {
  if (target->directory.get() < 0) return kj::READY_NOW;
  if (next == kj::none) {
    auto paf = kj::newPromiseAndFulfiller<void>();
    next = Round{paf.promise.fork(), kj::mv(paf.fulfiller)};
  }
  auto waiter = KJ_ASSERT_NONNULL(next).done.addBranch();
  if (!syncing) {
    syncing = true;
    running = loop().eagerlyEvaluate(nullptr);
  }
  return waiter;
}

kj::Promise<void> CommitSync::loop() {
  for (;;) {
    kj::Maybe<Round> current = kj::mv(next);
    next = kj::none;
    Round& round = KJ_UNWRAP_OR(current, break);
    auto paf = kj::newPromiseAndCrossThreadFulfiller<void>();
    ++begun;
    syncsInFlight.fetch_add(1, std::memory_order_relaxed);
    SyncPool::get().submit(
        [target = kj::atomicAddRef(*target), fulfiller = kj::mv(paf.fulfiller)]() mutable {
      // After the result is queued for the event loop, so a reader that sees none in flight
      // finds the result by polling the loop.
      KJ_DEFER(syncsInFlight.fetch_sub(1, std::memory_order_release));
      KJ_IF_SOME(e, target->syncNow()) {
        fulfiller->reject(kj::mv(e));
      } else {
        fulfiller->fulfill();
      }
    });
    try {
      co_await paf.promise;
      round.fulfiller->fulfill();
    } catch (...) {
      round.fulfiller->reject(kj::getCaughtExceptionAsKj());
    }
  }
  syncing = false;
}

}  // namespace workerd::server::apsy
