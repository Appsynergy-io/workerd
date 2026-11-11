@0xad7e138564a99866;
# The platform's settings for one workerd process: what upstream's `Config` (workerd.capnp) has
# no field for. They live in a schema of their own, with ordinals of their own, so that upstream
# adding a field to `Config` or `Worker` never collides with one of these.
#
# `workerd serve` reads them with `--platform-config=<file>` or `--platform-config-fd=<fd>`, as
# text: one `Platform` struct literal in Cap'n Proto's text format, for example
#
#     (services = [(name = "main", limits = (cpuMs = 50, subrequests = 50))])
#
# Without either option none of these settings applies, and the process runs every Worker as
# upstream workerd does.

using Cxx = import "/capnp/c++.capnp";
$Cxx.namespace("workerd::server::apsy");

struct Platform {
  services @0 :List(Service);
  # Settings of the config's Workers, by service name. Each name must be a Worker service of the
  # config; a service named nowhere here runs as upstream's.

  cpuRolloverInitialMs @1 :UInt32 = 50;
  # CPU time a fresh isolate of a Worker with `limits` may run past its limit before it has
  # banked any, in milliseconds: the first requests after a deploy or a new isolate compile the
  # functions they call. Cloudflare documents that each isolate has "some built-in flexibility to
  # allow for cases where your Worker infrequently runs over the configured limit" and publishes
  # no figure, so 50 ms is the platform's own
  # [https://developers.cloudflare.com/workers/platform/limits/#cpu-time].

  cpuRolloverCapMs @2 :UInt32 = 50;
  # The most an isolate banks from invocations that ended under their CPU limit, in
  # milliseconds, and never more than one invocation's limit. The platform's own, as above.

  durableObjectMaxQueuedRequests @3 :UInt32 = 1000;
  # The requests stubs may have unanswered at one Durable Object before the next is refused with
  # "Durable Object is overloaded. Too many requests queued.", an exception with `.overloaded`
  # set: "A Durable Object that receives too many requests will, after attempting to queue them,
  # return an overloaded error to the caller"
  # [https://developers.cloudflare.com/durable-objects/platform/limits/]. Cloudflare publishes no
  # queue length, so 1,000 is the platform's own: "An individual Object has a soft limit of 1,000
  # requests per second" (same page), and a queue of 1,000 is about a second of that rate. An
  # object's own alarms and hibernated WebSocket events are not counted. 0: no limit. Without
  # settings the queue is unbounded, as upstream's.

  struct Service {
    name @0 :Text;
    # The service's name in the config.

    limits @1 :Limits;
    # Set: the Worker's startup gets 1 s of CPU time (#worker-startup-time) and each invocation is
    # held to `limits`. Unset: no limit, as upstream. CPU and wall time are measured either way.

    rebuildGroup @2 :Text;
    # Workers naming the same group share one allowance for building again an isolate condemned
    # past `limits.memoryBytes` (server/rebuild.rs), beside each Worker's own allowance and the
    # process's, so that the Workers of one group cannot keep another group's from being rebuilt.
    # Unset or empty: the Worker is in no group, and only its own and the process's apply.

    experimentalCompatibilityFlags @3 :List(Text);
    # Compatibility flags annotated `$experimental` (io/compatibility-date.capnp) that this Worker
    # may enable without `--experimental`: compileCompatibilityFlags()'s
    # `allowedExperimentalFlags`. A flag not listed here is refused as upstream refuses it without
    # `--experimental`; one listed that the Worker does not enable has no effect. With
    # `--experimental` every such flag is admitted, as upstream. Unset: none.
  }

  struct Limits {
    # The limits Cloudflare documents per invocation
    # [https://developers.cloudflare.com/workers/platform/limits/].

    cpuMs @0 :UInt32;
    # CPU time one invocation may use, in milliseconds (#cpu-time). Past it, JavaScript execution
    # is terminated and the invocation's outcome is `exceededCpu`. 0 means unlimited.

    subrequests @1 :UInt32;
    # Subrequests one invocation may make (#subrequests). 0 means unlimited.

    memoryBytes @2 :UInt64;
    # Memory the Worker's isolate may hold, its JavaScript heap and its ArrayBuffer and
    # WebAssembly memory together: "Each isolate can consume up to 128 MB of memory, including the
    # JavaScript heap and WebAssembly allocations" (#memory): Cloudflare's limit is 134217728.
    # The isolate gets an isolate group, and so a pointer-compression cage, of its own. An
    # invocation that takes the isolate past it ends as `exceededMemory` (Error 1102), and the
    # next request gets a new isolate. At most 1 GiB, which the isolate's cage leaves room for. 0
    # means no limit, as upstream.

    durableObjectStorageBytes @3 :UInt64;
    # Bytes one Durable Object's SQLite database may hold, for each namespace the Worker declares:
    # its `max_page_count` is set to this over its page size each time it is opened or reset, so
    # a write needing a page past it fails with `database or disk is full: SQLITE_FULL` while
    # reads and deletes succeed. The key-value storage API writes to the same database and is
    # refused the same way. 0 means unlimited. Cloudflare's figure is 10 GB per object
    # [https://developers.cloudflare.com/durable-objects/platform/limits/].
  }
}
