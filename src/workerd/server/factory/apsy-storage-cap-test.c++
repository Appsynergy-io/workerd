#include "apsy-storage-cap.h"

#include <workerd/util/sqlite-kv.h>

#include <kj/test.h>

namespace workerd::server::apsy {
namespace {

struct GlobalInit {
  GlobalInit() {
    installSqliteCustomAllocator();
  }
};
static GlobalInit init;

constexpr uint64_t CAP = 64 * 1024;

uint64_t pageBytes(SqliteDatabase& db) {
  return db.run("PRAGMA page_count;").getInt64(0) * db.run("PRAGMA page_size;").getInt64(0);
}

// Inserts 1 KiB rows until one is refused, and answers how many fit.
int fill(SqliteDatabase& db) {
  auto row = kj::heapArray<byte>(1024, 'x');
  for (int i = 0; i < 10000; i++) {
    KJ_IF_SOME(e, kj::runCatchingExceptions([&]() {
      db.run("INSERT INTO t (data) VALUES (?);", row.asPtr());
    })) {
      KJ_EXPECT(e.getDescription().contains("database or disk is full: SQLITE_FULL"), e);
      return i;
    }
  }
  KJ_FAIL_EXPECT("no write was refused");
  return -1;
}

KJ_TEST("the page count is the byte limit over the page size, at least one page") {
  KJ_EXPECT(maxPageCountFor(10'000'000'000, 4096) == 2'441'406);
  KJ_EXPECT(maxPageCountFor(1'000'000'000, 4096) == 244'140);
  KJ_EXPECT(maxPageCountFor(CAP, 4096) == 16);
  KJ_EXPECT(maxPageCountFor(100, 4096) == 1);
  KJ_EXPECT(maxPageCountFor(CAP, 0) == 1);
}

KJ_TEST("a write past the cap is SQLITE_FULL; reads and deletes still succeed") {
  auto dir = kj::newInMemoryDirectory(kj::nullClock());
  SqliteDatabase::Vfs vfs(*dir);
  SqliteDatabase db(vfs, kj::Path({"object"}), kj::WriteMode::CREATE | kj::WriteMode::MODIFY);
  db.run("PRAGMA journal_mode=WAL;");
  capDatabaseSize(db, CAP);
  db.run("CREATE TABLE t (id INTEGER PRIMARY KEY, data BLOB);");

  int fitted = fill(db);
  KJ_EXPECT(fitted > 0);
  KJ_EXPECT(pageBytes(db) <= CAP, pageBytes(db));
  KJ_EXPECT(db.run("SELECT count(*) FROM t;").getInt64(0) == fitted);

  db.run("DELETE FROM t WHERE id % 2 = 0;");
  auto row = kj::heapArray<byte>(1024, 'y');
  db.run("INSERT INTO t (data) VALUES (?);", row.asPtr());
  KJ_EXPECT(pageBytes(db) <= CAP, pageBytes(db));
}

KJ_TEST("the cap holds a database reopened, and one already past it grows no further") {
  auto dir = kj::newInMemoryDirectory(kj::nullClock());
  SqliteDatabase::Vfs vfs(*dir);
  uint64_t before;
  {
    SqliteDatabase db(vfs, kj::Path({"object"}), kj::WriteMode::CREATE | kj::WriteMode::MODIFY);
    db.run("CREATE TABLE t (id INTEGER PRIMARY KEY, data BLOB);");
    auto row = kj::heapArray<byte>(1024, 'x');
    for (int i = 0; i < 200; i++) db.run("INSERT INTO t (data) VALUES (?);", row.asPtr());
    before = pageBytes(db);
    KJ_ASSERT(before > CAP, before);
  }
  SqliteDatabase db(vfs, kj::Path({"object"}), kj::WriteMode::MODIFY);
  capDatabaseSize(db, CAP);
  // What still fits in the pages it has is written; nothing needing a page more is.
  int fitted = fill(db);
  KJ_EXPECT(pageBytes(db) == before, pageBytes(db), before);
  KJ_EXPECT(db.run("SELECT count(*) FROM t;").getInt64(0) == 200 + fitted);
}

KJ_TEST("the key-value API on the same database is refused past the cap, as the SQL API is") {
  auto dir = kj::newInMemoryDirectory(kj::nullClock());
  SqliteDatabase::Vfs vfs(*dir);
  SqliteDatabase db(vfs, kj::Path({"object"}), kj::WriteMode::CREATE | kj::WriteMode::MODIFY);
  capDatabaseSize(db, CAP);
  SqliteKv kv(db);
  auto value = kj::heapArray<byte>(1024, 'v');
  kj::Maybe<kj::Exception> refused;
  for (int i = 0; i < 10000 && refused == kj::none; i++) {
    refused = kj::runCatchingExceptions([&]() { kv.put(kj::str("key", i), value.asPtr()); });
  }
  auto& e = KJ_ASSERT_NONNULL(refused, "no put was refused");
  // An Error the Worker sees, carrying SQLite's message, not an internal error.
  KJ_EXPECT(e.getDescription().contains("jsg.Error: database or disk is full: SQLITE_FULL"), e);
  KJ_EXPECT(pageBytes(db) <= CAP, pageBytes(db));
  KJ_EXPECT(kv.delete_("key0"));
}
}  // namespace
}  // namespace workerd::server::apsy
