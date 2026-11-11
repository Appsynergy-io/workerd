// The decisions the factory's CPU and memory limits (apsy-limit-enforcers.c++) rest on, apart from
// the isolate and the watchdog thread that act on them, so each can be tested alone.

#pragma once

#include <time.h>

#include <kj/common.h>

#include <atomic>
#include <cstdint>

namespace workerd::server::apsy {

// A clock's reading in nanoseconds, or none when it cannot be read. A CPU clock that cannot be
// read is counted as over its limit wherever a limit is enforced, never as zero time used: a
// Worker is never let past its limit because a clock could not tell how much it used.
inline kj::Maybe<int64_t> readClockNanos(clockid_t clock) {
  struct timespec ts;
  if (clock_gettime(clock, &ts) != 0) return kj::none;
  return static_cast<int64_t>(ts.tv_sec) * 1'000'000'000 + ts.tv_nsec;
}

// The CPU-clock reading past which an execution that starts at `now` with `remainingNanos` of
// its limit left is terminated: 0, already passed, when the clock could not be read.
inline int64_t cpuDeadlineFrom(kj::Maybe<int64_t> now, int64_t remainingNanos) {
  KJ_IF_SOME(n, now) {
    return n + (remainingNanos > 0 ? remainingNanos : 0);
  }
  return 0;
}

// Whether an execution whose thread's CPU clock reads `now` has reached `deadlineNanos`. A clock
// that could not be read has.
inline bool cpuDeadlineReached(kj::Maybe<int64_t> now, int64_t deadlineNanos) {
  KJ_IF_SOME(n, now) {
    return n >= deadlineNanos;
  }
  return true;
}

// The CPU time between a scope's two readings of its thread's CPU clock, or none when either
// could not be read: the caller then charges what was left of the limit and counts the scope as
// over it.
inline kj::Maybe<int64_t> scopeCpuNanos(kj::Maybe<int64_t> start, kj::Maybe<int64_t> end) {
  KJ_IF_SOME(s, start) {
    KJ_IF_SOME(e, end) {
      return e > s ? e - s : 0;
    }
  }
  return kj::none;
}

// CPU rollover: "Each isolate has some built-in flexibility to allow for cases where your Worker
// infrequently runs over the configured limit"
// [https://developers.cloudflare.com/workers/platform/limits/#cpu-time]. Cloudflare publishes no
// figure for it, so the figures are the platform's own: apsy.capnp `Platform.cpuRolloverInitialMs`
// and `Platform.cpuRolloverCapMs`, whose documented defaults are 50 ms each.
//
// - A fresh isolate's bank holds the initial allowance, so the first requests after a deploy or a
//   new isolate, which compile the functions they call for the first time, are not ended for
//   that compilation.
// - An invocation that ends under its limit banks what it left unused, up to the cap
//   (cpuBankCap()): its own CPU limit, and never more than `capNanos`. A bank already at or over
//   the cap (the initial allowance) gains nothing.
// - An invocation may run past its limit by what the bank holds when its JavaScript starts; what
//   it uses past the limit is taken from the bank. One that keeps running over drains the bank and
//   is then terminated at its limit.

// The most an isolate's bank holds from what invocations under `limitNanos` left unused, the
// platform's cap being `capNanos`.
inline int64_t cpuBankCap(int64_t limitNanos, int64_t capNanos) {
  return limitNanos < capNanos ? limitNanos : capNanos;
}

// What an invocation under `limitNanos` that has used `usedNanos` and taken `drawnNanos` from the
// bank may still use, the bank holding `bankNanos`: its own remainder, never negative, and the bank.
inline int64_t cpuAllowanceNanos(
    int64_t limitNanos, int64_t usedNanos, int64_t drawnNanos, int64_t bankNanos) {
  int64_t own = limitNanos + drawnNanos - usedNanos;
  return (own > 0 ? own : 0) + (bankNanos > 0 ? bankNanos : 0);
}

// Takes from `bankNanos` what an invocation has used past its limit and not yet taken, as far as
// the bank holds, adding it to `drawnNanos`.
inline void cpuDrawOverrun(
    int64_t limitNanos, int64_t usedNanos, int64_t& drawnNanos, int64_t& bankNanos) {
  int64_t over = usedNanos - limitNanos - drawnNanos;
  if (over <= 0 || bankNanos <= 0) return;
  int64_t take = over < bankNanos ? over : bankNanos;
  bankNanos -= take;
  drawnNanos += take;
}

// Banks what an invocation that ended having used `usedNanos` of `limitNanos` left unused, up to
// cpuBankCap(). An invocation at or over its limit banks nothing.
inline void cpuCreditUnused(
    int64_t limitNanos, int64_t usedNanos, int64_t capNanos, int64_t& bankNanos) {
  if (usedNanos >= limitNanos) return;
  int64_t cap = cpuBankCap(limitNanos, capNanos);
  if (bankNanos >= cap) return;
  int64_t credited = bankNanos + (limitNanos - usedNanos);
  bankNanos = credited < cap ? credited : cap;
}

// Takes `length` bytes out of what `held` may still reach under `ceiling`, in the one atomic step
// that checks them, before the allocation they are for is made: two allocations on two threads,
// or one made while another is still allocating, cannot both pass a check only one fits under.
// A zero length is never refused. False, with nothing taken, when the bytes do not fit.
inline bool reserveBytes(std::atomic<size_t>& held, size_t length, size_t ceiling) {
  if (length == 0) return true;
  size_t now = held.load(std::memory_order_relaxed);
  do {
    if (now + length < now || now + length > ceiling) return false;
  } while (!held.compare_exchange_weak(now, now + length, std::memory_order_relaxed));
  return true;
}

// Gives back what reserveBytes() took, for an allocation that failed or a store freed.
inline void releaseBytes(std::atomic<size_t>& held, size_t length) {
  held.fetch_sub(length, std::memory_order_relaxed);
}

}  // namespace workerd::server::apsy
