#include "apsy-limits.h"

#include <pthread.h>

#include <kj/test.h>

#include <thread>
#include <vector>

namespace workerd::server::apsy {
namespace {

// No clock has this id, so clock_gettime() fails on it with EINVAL.
constexpr clockid_t UNREADABLE_CLOCK = 1000;

KJ_TEST("a clock that cannot be read has no reading, never zero") {
  KJ_EXPECT(readClockNanos(UNREADABLE_CLOCK) == kj::none);
  KJ_EXPECT(readClockNanos(CLOCK_MONOTONIC) != kj::none);

  clockid_t own;
  KJ_ASSERT(pthread_getcpuclockid(pthread_self(), &own) == 0);
  KJ_EXPECT(readClockNanos(own) != kj::none);
}

KJ_TEST("an unreadable CPU clock is over the limit, however much of it is left") {
  // The watchdog's check on an execution whose clock cannot be read.
  KJ_EXPECT(cpuDeadlineReached(readClockNanos(UNREADABLE_CLOCK), INT64_MAX));
  KJ_EXPECT(cpuDeadlineReached(kj::none, INT64_MAX));

  // A scope whose clock cannot be read at its start is given a deadline already passed.
  int64_t deadline = cpuDeadlineFrom(readClockNanos(UNREADABLE_CLOCK), 30'000'000'000);
  KJ_EXPECT(cpuDeadlineReached(int64_t(0), deadline));

  // A readable clock is held to the time left, and to none past it.
  KJ_EXPECT(cpuDeadlineFrom(int64_t(100), 50) == 150);
  KJ_EXPECT(cpuDeadlineFrom(int64_t(100), -5) == 100);
  KJ_EXPECT(!cpuDeadlineReached(int64_t(149), 150));
  KJ_EXPECT(cpuDeadlineReached(int64_t(150), 150));
}

KJ_TEST("a scope whose start or end cannot be read measured nothing") {
  KJ_EXPECT(scopeCpuNanos(int64_t(10), int64_t(25)) == int64_t(15));
  KJ_EXPECT(scopeCpuNanos(int64_t(25), int64_t(10)) == int64_t(0));
  KJ_EXPECT(scopeCpuNanos(kj::none, int64_t(25)) == kj::none);
  KJ_EXPECT(scopeCpuNanos(int64_t(10), readClockNanos(UNREADABLE_CLOCK)) == kj::none);
}

constexpr int64_t MS = 1'000'000;

// The documented defaults of apsy.capnp `Platform.cpuRolloverInitialMs` and `cpuRolloverCapMs`.
constexpr int64_t CPU_ROLLOVER_INITIAL_NANOS = 50 * MS;
constexpr int64_t CPU_ROLLOVER_CAP_NANOS = 50 * MS;

// One invocation's JavaScript in one scope, as JsScope accounts it: allowed what
// cpuAllowanceNanos() gives at entry, it runs `want` or is terminated at the allowance; then the
// overrun is drawn and, the invocation ending, what it left unused is banked. True when it ran to
// the end.
static bool runInvocation(int64_t limit, int64_t want, int64_t& bank) {
  int64_t used = 0, drawn = 0;
  int64_t allowance = cpuAllowanceNanos(limit, used, drawn, bank);
  bool terminated = want > allowance;
  used += terminated ? allowance : want;
  cpuDrawOverrun(limit, used, drawn, bank);
  if (!terminated) cpuCreditUnused(limit, used, CPU_ROLLOVER_CAP_NANOS, bank);
  return !terminated;
}

KJ_TEST("one request over the limit among many under it is not terminated") {
  int64_t bank = CPU_ROLLOVER_INITIAL_NANOS;
  // The initial allowance spent by a heavy first request...
  KJ_EXPECT(runInvocation(10 * MS, 10 * MS + CPU_ROLLOVER_INITIAL_NANOS, bank));
  KJ_EXPECT(bank == 0, bank);
  // ...and refilled by requests well under the limit, up to the limit.
  for (int i = 0; i < 20; i++) KJ_EXPECT(runInvocation(10 * MS, 1 * MS, bank));
  KJ_EXPECT(bank == 10 * MS, bank);
  KJ_EXPECT(runInvocation(10 * MS, 12 * MS, bank));
  KJ_EXPECT(bank == 8 * MS, bank);
}

KJ_TEST("a Worker over its limit every time drains the bank and is then terminated at the limit") {
  int64_t bank = CPU_ROLLOVER_INITIAL_NANOS;
  int passed = 0;
  for (int i = 0; i < 10; i++) {
    if (runInvocation(10 * MS, 15 * MS, bank)) passed++;
  }
  // 50 ms of allowance pays for ten 5 ms overruns and no more.
  KJ_EXPECT(passed == 10, passed);
  KJ_EXPECT(bank == 0, bank);
  for (int i = 0; i < 10; i++) {
    int64_t used = 0, drawn = 0;
    KJ_EXPECT(cpuAllowanceNanos(10 * MS, used, drawn, bank) == 10 * MS);
    KJ_EXPECT(!runInvocation(10 * MS, 15 * MS, bank));
  }
  // A terminated invocation banks nothing, so it stays terminated.
  KJ_EXPECT(bank == 0, bank);
}

KJ_TEST("the bank never holds more than the cap from unused time") {
  int64_t bank = 0;
  for (int i = 0; i < 100; i++) runInvocation(10 * MS, 0, bank);
  KJ_EXPECT(bank == 10 * MS);
  bank = 0;
  for (int i = 0; i < 100; i++) runInvocation(30'000 * MS, 1 * MS, bank);
  KJ_EXPECT(bank == CPU_ROLLOVER_CAP_NANOS);
  // An initial allowance over the cap is not topped up past itself, nor cut down.
  bank = CPU_ROLLOVER_INITIAL_NANOS;
  runInvocation(5 * MS, 1 * MS, bank);
  KJ_EXPECT(bank == CPU_ROLLOVER_INITIAL_NANOS);
}

KJ_TEST("an invocation's later scopes are allowed only the bank left after its earlier overrun") {
  int64_t bank = 20 * MS, used = 0, drawn = 0;
  // A first scope uses 25 ms of a 10 ms limit: 15 ms taken from the bank.
  used += 25 * MS;
  cpuDrawOverrun(10 * MS, used, drawn, bank);
  KJ_EXPECT(drawn == 15 * MS && bank == 5 * MS);
  // Drawing again takes nothing already taken.
  cpuDrawOverrun(10 * MS, used, drawn, bank);
  KJ_EXPECT(drawn == 15 * MS && bank == 5 * MS);
  KJ_EXPECT(cpuAllowanceNanos(10 * MS, used, drawn, bank) == 5 * MS);
  // An overrun past what the bank held is not charged twice and never makes the bank negative.
  used += 9 * MS;
  cpuDrawOverrun(10 * MS, used, drawn, bank);
  KJ_EXPECT(bank == 0 && drawn == 20 * MS);
  KJ_EXPECT(cpuAllowanceNanos(10 * MS, used, drawn, bank) == 0);
  cpuCreditUnused(10 * MS, used, CPU_ROLLOVER_CAP_NANOS, bank);
  KJ_EXPECT(bank == 0);
}

KJ_TEST("bytes are taken when checked, so an allocation in flight counts against the next") {
  std::atomic<size_t> held{0};
  // The first allocation passes and holds its bytes before it is made...
  KJ_EXPECT(reserveBytes(held, 60, 100));
  KJ_EXPECT(held.load() == 60);
  // ...so a second asked for while the first is still allocating does not also fit.
  KJ_EXPECT(!reserveBytes(held, 60, 100));
  KJ_EXPECT(held.load() == 60);
  KJ_EXPECT(reserveBytes(held, 40, 100));
  KJ_EXPECT(held.load() == 100);
  releaseBytes(held, 60);
  KJ_EXPECT(held.load() == 40);
  KJ_EXPECT(reserveBytes(held, 0, 0));
  KJ_EXPECT(held.load() == 40);
}

KJ_TEST("a length that would wrap the count is refused") {
  std::atomic<size_t> held{SIZE_MAX - 4};
  KJ_EXPECT(!reserveBytes(held, 10, SIZE_MAX));
  KJ_EXPECT(held.load() == SIZE_MAX - 4);
}

KJ_TEST("threads reserving at once never take more than the ceiling together") {
  constexpr size_t CEILING = 1000;
  constexpr int THREADS = 8;
  constexpr int ATTEMPTS = 20000;
  for (int round = 0; round < 20; round++) {
    std::atomic<size_t> held{0};
    std::atomic<size_t> granted{0};
    std::atomic<bool> go{false};
    std::vector<std::thread> threads;
    for (int t = 0; t < THREADS; t++) {
      threads.emplace_back([&]() {
        while (!go.load()) {}
        for (int i = 0; i < ATTEMPTS; i++) {
          if (reserveBytes(held, 1, CEILING)) granted.fetch_add(1);
        }
      });
    }
    go.store(true);
    for (auto& thread: threads) thread.join();
    KJ_EXPECT(granted.load() == CEILING, granted.load(), round);
    KJ_EXPECT(held.load() == CEILING, held.load(), round);
  }
}

}  // namespace
}  // namespace workerd::server::apsy
