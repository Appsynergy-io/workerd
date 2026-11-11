// A Durable Object's commits made durable off the event loop, many commits to one sync
// [000 requests per second"].
//
// Upstream opens each object's SQLite database in WAL mode at SQLite's default synchronous=FULL,
// so every COMMIT waits for the write-ahead log's fsync on the thread that runs every Worker of
// the process: one object's writes then run at the disk's fsync rate, and every other Worker of
// the process waits through each one.

#pragma once

#include <workerd/util/sqlite.h>

#include <kj/async.h>
#include <kj/filesystem.h>
#include <kj/string.h>

namespace workerd::server::apsy {

// Sets `db` to write a commit to its write-ahead log without syncing it (PRAGMA
// synchronous=NORMAL, which syncs only at a checkpoint), so the commit returns without waiting on
// the disk. A committed transaction is durable once a CommitSync::sync() begun after it resolves.
// The setting belongs to the connection, so it is made again after a reset.
void deferCommitSync(SqliteDatabase& db);

// The durability of one database's commits: sync() is ActorSqlite's commitCallback, whose promise
// the object's output gate waits on, so no response that follows a write leaves before the write
// is on disk, as upstream's synchronous commit guaranteed.
//
// The write-ahead log's fdatasync runs on a small pool of threads, never on the event loop. At
// most one sync of a database is in flight; every commit made while it runs waits for the next
// one, which covers them all, so a busy object pays one sync per round rather than per commit.
class CommitSync {
 public:
  // `directory` holds the database; `walName` is its write-ahead log's file name in it.
  CommitSync(const kj::Directory& directory, kj::String walName);
  ~CommitSync() noexcept(false);
  KJ_DISALLOW_COPY_AND_MOVE(CommitSync);

  // Resolves once every transaction committed before the call is on disk; rejects when the sync
  // failed, which breaks the object's output gate as a failed commit does.
  kj::Promise<void> sync();

  // How many syncs of the log this has begun.
  uint64_t syncsBegun() const {
    return begun;
  }

  // How many times a sync has synced the log's directory: once per new log file.
  uint64_t directorySyncs() const;

  // How many syncs of any database in the process are running on the pool now. One counts until
  // its result is queued for the event loop that waits on it.
  static uint64_t inFlight();

  struct Target;

 private:
  struct Round {
    kj::ForkedPromise<void> done;
    kj::Own<kj::PromiseFulfiller<void>> fulfiller;
  };

  kj::Own<const Target> target;
  // The commits that no sync begun so far covers.
  kj::Maybe<Round> next;
  kj::Maybe<kj::Promise<void>> running;
  bool syncing = false;
  uint64_t begun = 0;

  kj::Promise<void> loop();
};

}  // namespace workerd::server::apsy
