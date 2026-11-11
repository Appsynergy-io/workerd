#include "apsy-storage-cap.h"

#include <kj/debug.h>

namespace workerd::server::apsy {

// SQLite takes a max_page_count up to 4294967294 [upstream: sqlite, pragma.html#pragma_max_page_count].
static constexpr uint64_t SQLITE_MAX_PAGE_COUNT = 4'294'967'294;

uint64_t maxPageCountFor(uint64_t maxBytes, uint64_t pageSize) {
  if (pageSize == 0) return 1;
  return kj::min(kj::max(maxBytes / pageSize, uint64_t(1)), SQLITE_MAX_PAGE_COUNT);
}

void capDatabaseSize(SqliteDatabase& db, uint64_t maxBytes) {
  if (maxBytes == 0) return;
  uint64_t pageSize =
      db.run({.regulator = SqliteDatabase::TRUSTED}, "PRAGMA page_size;").getInt64(0);
  uint64_t pages = maxPageCountFor(maxBytes, pageSize);
  db.run({.regulator = SqliteDatabase::TRUSTED}, kj::str("PRAGMA max_page_count = ", pages, ";"));
  // SQLite keeps a database already larger at its current page count rather than below it.
  uint64_t held =
      db.run({.regulator = SqliteDatabase::TRUSTED}, "PRAGMA max_page_count;").getInt64(0);
  uint64_t current =
      db.run({.regulator = SqliteDatabase::TRUSTED}, "PRAGMA page_count;").getInt64(0);
  KJ_REQUIRE(
      held <= kj::max(pages, current), "the database's page limit did not take", held, pages);
}

}  // namespace workerd::server::apsy
