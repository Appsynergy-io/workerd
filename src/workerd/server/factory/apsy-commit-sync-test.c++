#include "apsy-commit-sync.h"

#include <kj/async-io.h>
#include <kj/test.h>

#include <cstdlib>

namespace workerd::server::apsy {
namespace {

struct GlobalInit {
  GlobalInit() {
    installSqliteCustomAllocator();
  }
};
static GlobalInit init;

// A directory on disk, since what is synced is a file there.
class TempDir {
 public:
  TempDir() {
    const char* base = getenv("TEST_TMPDIR");
    auto name = kj::str(base != nullptr ? base : "/var/tmp", "/apsy-commit-sync-test.XXXXXX");
    KJ_ASSERT(mkdtemp(name.begin()) != nullptr, name);
    dir =
        disk->getRoot().openSubdir(disk->getCurrentPath().evalNative(name), kj::WriteMode::MODIFY);
  }
  const kj::Directory& operator*() {
    return *dir;
  }

 private:
  kj::Own<kj::Filesystem> disk = kj::newDiskFilesystem();
  kj::Own<const kj::Directory> dir;
};

KJ_TEST("a commit does not sync; a sync begun after it resolves") {
  auto io = kj::setupAsyncIo();
  TempDir dir;
  SqliteDatabase::Vfs vfs(*dir);
  SqliteDatabase db(
      vfs, kj::Path({"object.sqlite"}), kj::WriteMode::CREATE | kj::WriteMode::MODIFY);
  db.run("PRAGMA journal_mode=WAL;");
  deferCommitSync(db);
  KJ_EXPECT(db.run("PRAGMA synchronous;").getInt64(0) == 1, "NORMAL");

  CommitSync sync(*dir, kj::str("object.sqlite-wal"));
  db.run("CREATE TABLE t (v INTEGER);");
  db.run("INSERT INTO t VALUES (1);");
  KJ_EXPECT((*dir).exists(kj::Path({"object.sqlite-wal"})));
  sync.sync().wait(io.waitScope);
  KJ_EXPECT(sync.syncsBegun() == 1);
}

KJ_TEST("commits made while a sync runs share the next one") {
  auto io = kj::setupAsyncIo();
  TempDir dir;
  SqliteDatabase::Vfs vfs(*dir);
  SqliteDatabase db(
      vfs, kj::Path({"object.sqlite"}), kj::WriteMode::CREATE | kj::WriteMode::MODIFY);
  db.run("PRAGMA journal_mode=WAL;");
  deferCommitSync(db);
  db.run("CREATE TABLE t (v INTEGER);");

  CommitSync sync(*dir, kj::str("object.sqlite-wal"));
  kj::Vector<kj::Promise<void>> waiting;
  for (int i = 0; i < 100; i++) {
    db.run("INSERT INTO t VALUES (?);", static_cast<int64_t>(i));
    waiting.add(sync.sync());
  }
  kj::joinPromises(waiting.releaseAsArray()).wait(io.waitScope);
  // The first commit's sync, then one for the 99 made while it ran.
  KJ_EXPECT(sync.syncsBegun() == 2, sync.syncsBegun());

  sync.sync().wait(io.waitScope);
  KJ_EXPECT(sync.syncsBegun() == 3, sync.syncsBegun());
}

KJ_TEST("the first sync of a new log syncs its directory, once") {
  auto io = kj::setupAsyncIo();
  TempDir dir;
  SqliteDatabase::Vfs vfs(*dir);
  CommitSync sync(*dir, kj::str("object.sqlite-wal"));
  {
    SqliteDatabase db(
        vfs, kj::Path({"object.sqlite"}), kj::WriteMode::CREATE | kj::WriteMode::MODIFY);
    db.run("PRAGMA journal_mode=WAL;");
    deferCommitSync(db);
    db.run("CREATE TABLE t (v INTEGER);");
    sync.sync().wait(io.waitScope);
    KJ_EXPECT(sync.directorySyncs() == 1, sync.directorySyncs());
    db.run("INSERT INTO t VALUES (1);");
    sync.sync().wait(io.waitScope);
    KJ_EXPECT(sync.directorySyncs() == 1, "the same log's directory is not synced again");
  }
  // Closing the database checkpoints and removes its log, as an evicted object's is; the next
  // write makes a new one, whose entry in the directory must be synced too.
  KJ_EXPECT(!(*dir).exists(kj::Path({"object.sqlite-wal"})));
  SqliteDatabase db(vfs, kj::Path({"object.sqlite"}), kj::WriteMode::MODIFY);
  db.run("PRAGMA journal_mode=WAL;");
  deferCommitSync(db);
  db.run("INSERT INTO t VALUES (2);");
  sync.sync().wait(io.waitScope);
  KJ_EXPECT(sync.directorySyncs() == 2, sync.directorySyncs());
}

KJ_TEST("a database with no log yet, or in memory, has nothing to sync") {
  auto io = kj::setupAsyncIo();
  TempDir dir;
  CommitSync onDisk(*dir, kj::str("absent.sqlite-wal"));
  onDisk.sync().wait(io.waitScope);

  auto memory = kj::newInMemoryDirectory(kj::nullClock());
  CommitSync inMemory(*memory, kj::str("object.sqlite-wal"));
  inMemory.sync().wait(io.waitScope);
  KJ_EXPECT(inMemory.syncsBegun() == 0);
  KJ_EXPECT(onDisk.directorySyncs() == 0);
}

}  // namespace
}  // namespace workerd::server::apsy
