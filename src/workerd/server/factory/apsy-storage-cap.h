// The storage one Durable Object's SQLite database may hold. Upstream workerd sets no size on it,
// so SQLite's default maximum page
// count lets one object grow until the node's disk is full.

#pragma once

#include <workerd/util/sqlite.h>

#include <cstdint>

namespace workerd::server::apsy {

// The page count a database of `pageSize`-byte pages may reach so that its pages together hold no
// more than `maxBytes`: at least one page, so the database stays readable.
uint64_t maxPageCountFor(uint64_t maxBytes, uint64_t pageSize);

// Holds `db` to `maxBytes` of pages (PRAGMA max_page_count), past which a write that needs another
// page fails with SQLITE_FULL while reads and deletes still succeed. A database already larger
// keeps its size and grows no further. The setting belongs to the connection, so it is made again
// each time the database is opened or reset. 0 holds it to nothing, as upstream does.
void capDatabaseSize(SqliteDatabase& db, uint64_t maxBytes);

}  // namespace workerd::server::apsy
