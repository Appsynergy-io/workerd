// Copyright (c) 2026 Cloudflare, Inc.
// Licensed under the Apache 2.0 license found in the LICENSE file or at:
//     https://opensource.org/licenses/Apache-2.0

// The listeners' shims: the Rust rewriter's edits applied to kj headers, a response whose
// headers are edited on the way out, the response of a request an HTTP socket received (which
// also answers the platform's errors), the `ConnectResponse` of a raw TCP socket, and a UDP flow
// as a `DatagramChannel` for `connect()` events.

#include "worker-factory-impl.h"

#include <workerd/api/sockets.h>
#include <workerd/jsg/exception.h>
#include <workerd/util/exception.h>
#include <workerd/util/stream-utils.h>
#include <workerd/util/strings.h>
#include <workerd/util/websocket-error-handler.h>

#include <kj/compat/http.h>

namespace workerd::server {

namespace {

// A `HeaderEdit` with its header looked up in the table.
struct Edit {
  kj::HttpHeaderId id;
  kj::Maybe<kj::String> value;
};

kj::Array<Edit> resolveEdits(
    const kj::HttpHeaderTable& table, ::rust::Slice<const HeaderEdit> edits) {
  return KJ_MAP(edit, edits) -> Edit {
    auto name = kj::str(edit.name);
    return Edit{
      .id = KJ_REQUIRE_NONNULL(table.stringToId(name), "header is not in the header table", name),
      .value = edit.value.map([](const ::rust::String& value) { return kj::str(value); }),
    };
  };
}

// A response applying a socket's or external server's `injectResponseHeaders` before forwarding
// to the response it wraps.
class RewritingResponse final: public kj::HttpService::Response {
 public:
  RewritingResponse(kj::HttpService::Response& inner, kj::Array<Edit> edits)
      : inner(inner),
        edits(kj::mv(edits)) {}

  kj::Own<kj::AsyncOutputStream> send(uint statusCode,
      kj::StringPtr statusText,
      const kj::HttpHeaders& headers,
      kj::Maybe<uint64_t> expectedBodySize = kj::none) override {
    return inner.send(statusCode, statusText, rewrite(headers), expectedBodySize);
  }

  kj::Own<kj::WebSocket> acceptWebSocket(const kj::HttpHeaders& headers) override {
    return inner.acceptWebSocket(rewrite(headers));
  }

 private:
  kj::HttpService::Response& inner;
  kj::Array<Edit> edits;

  kj::HttpHeaders rewrite(const kj::HttpHeaders& headers) {
    auto rewritten = headers.cloneShallow();
    for (auto& edit: edits) {
      KJ_IF_SOME(value, edit.value) {
        rewritten.setPtr(edit.id, value);
      } else {
        rewritten.unset(edit.id);
      }
    }
    return rewritten;
  }
};

// The response of a request an HTTP socket received. Besides the socket's own
// `injectResponseHeaders`, the Worker's own `cf-error-type` and `cf-error-origin` are dropped:
// those name an error the platform generated, which only answerError sends, writing
// to the connection directly rather than through the edits.
class ListenerResponse final: public kj::HttpService::Response {
 public:
  ListenerResponse(
      kj::HttpService::Response& inner, const kj::HttpHeaderTable& table, kj::Array<Edit> edits)
      : inner(inner),
        table(table),
        edits(kj::mv(edits)) {}

  kj::Own<kj::AsyncOutputStream> send(uint statusCode,
      kj::StringPtr statusText,
      const kj::HttpHeaders& headers,
      kj::Maybe<uint64_t> expectedBodySize = kj::none) override {
    sent = true;
    return inner.send(statusCode, statusText, rewrite(headers), expectedBodySize);
  }

  kj::Own<kj::WebSocket> acceptWebSocket(const kj::HttpHeaders& headers) override {
    sent = true;
    return inner.acceptWebSocket(rewrite(headers));
  }

  // Answers `exception`, the request's failure, as the platform error it is, if nothing was sent
  // yet. Returns the error's number, or 0 for an exception that is not one (or a response already
  // begun), which the caller answers as before.
  //
  // - A Worker that exceeds its CPU time or memory limit is answered with Error 1102, `Worker
  //   exceeded resource limits`, as a 503 carrying `cf-error-type: 1102` and, as
  //   `cf-error-origin`, the system the error-header table names for 1102.
  // - An exception the Worker's JavaScript threw and did not catch -- one a Durable Object or a
  //   service it called threw into it among them -- is the invocation status `Worker threw
  //   exception`, Error 1101, a runtime JavaScript exception carrying `cf-error-type:
  //   1101`, as a 500 with the same two headers.
  kj::Promise<uint32_t> answerError(const kj::Exception& exception) {
    if (sent) co_return 0;
    uint32_t error;
    uint status;
    kj::StringPtr statusText;
    kj::StringPtr body;
    if (exception.getDetail(CPU_LIMIT_DETAIL_ID) != kj::none ||
        exception.getDetail(MEMORY_LIMIT_DETAIL_ID) != kj::none) {
      error = 1102;
      status = 503;
      statusText = "Service Unavailable"_kj;
      body = "Worker exceeded resource limits"_kj;
    } else if (jsg::isTunneledException(exception.getDescription())) {
      error = 1101;
      status = 500;
      statusText = "Internal Server Error"_kj;
      body = "Worker threw exception"_kj;
    } else {
      co_return 0;
    }
    sent = true;
    kj::HttpHeaders headers(table);
    headers.setPtr(kj::HttpHeaderId::CONTENT_TYPE, "text/plain; charset=utf-8");
    auto errorType = kj::str(error);
    headers.addPtrPtr("cf-error-type", errorType);
    headers.addPtrPtr("cf-error-origin", "Workers runtime");
    auto out = inner.send(status, statusText, headers, body.size());
    co_await out->write(body.asBytes());
    co_return error;
  }

 private:
  kj::HttpService::Response& inner;
  const kj::HttpHeaderTable& table;
  kj::Array<Edit> edits;
  bool sent = false;

  static bool isPlatformErrorHeader(kj::StringPtr name) {
    return strcaseeq(name, "cf-error-type"_kj) || strcaseeq(name, "cf-error-origin"_kj);
  }

  kj::HttpHeaders rewrite(const kj::HttpHeaders& headers) {
    auto rewritten = headers.cloneShallow();
    for (auto& edit: edits) {
      KJ_IF_SOME(value, edit.value) {
        rewritten.setPtr(edit.id, value);
      } else {
        rewritten.unset(edit.id);
      }
    }
    bool posing = false;
    rewritten.forEach(
        [&](kj::StringPtr name, kj::StringPtr) { posing = posing || isPlatformErrorHeader(name); });
    if (!posing) return rewritten;
    kj::HttpHeaders kept(table);
    rewritten.forEach([&](kj::StringPtr name, kj::StringPtr value) {
      if (!isPlatformErrorHeader(name)) kept.addPtrPtr(name, value);
    });
    kept.takeOwnership(kj::mv(rewritten));
    return kept;
  }
};

// The connect() answer of a raw TCP connection: there is no HTTP response to write.
class NullConnectResponse final: public kj::HttpService::ConnectResponse {
 public:
  void accept(uint statusCode, kj::StringPtr statusText, const kj::HttpHeaders& headers) override {}
  kj::Own<kj::AsyncOutputStream> reject(uint statusCode,
      kj::StringPtr statusText,
      const kj::HttpHeaders& headers,
      kj::Maybe<uint64_t> expectedBodySize = kj::none) override {
    return newNullOutputStream();
  }
};

// A Rust UDP flow as the DatagramChannel of a UdpConnectCustomEvent.
class UdpFlowChannel final: public DatagramChannel {
 public:
  explicit UdpFlowChannel(::rust::Box<UdpFlow> flow): flow(kj::mv(flow)) {}

  kj::Promise<kj::Maybe<kj::Array<kj::byte>>> receive() override {
    auto datagram = co_await flow->receive();
    if (datagram.ended) co_return kj::none;
    co_return kj::heapArray<kj::byte>(kj::from<Rust>(datagram.data));
  }

  kj::Promise<void> send(kj::ArrayPtr<const kj::byte> datagram) override {
    co_await flow->send(datagram.as<Rust>());
  }

 private:
  ::rust::Box<UdpFlow> flow;
};

}  // namespace

kj::Own<kj::HttpHeaders> edit_headers(const kj::HttpHeaderTable& table,
    const kj::HttpHeaders& headers,
    ::rust::Slice<const HeaderEdit> edits,
    ::rust::Slice<const HeaderEdit> injected) {
  auto edited = kj::heap(headers.clone());
  for (auto slice: {edits, injected}) {
    for (auto& edit: resolveEdits(table, slice)) {
      KJ_IF_SOME(value, edit.value) {
        edited->set(edit.id, kj::mv(value));
      } else {
        edited->unset(edit.id);
      }
    }
  }
  return edited;
}

kj::Own<kj::HttpService::Response> new_rewriting_response(kj::HttpService::Response& inner,
    const kj::HttpHeaderTable& table,
    ::rust::Slice<const HeaderEdit> edits) {
  return kj::heap<RewritingResponse>(inner, resolveEdits(table, edits));
}

kj::Own<kj::HttpService::Response> new_listener_response(kj::HttpService::Response& inner,
    const kj::HttpHeaderTable& table,
    ::rust::Slice<const HeaderEdit> edits) {
  return kj::heap<ListenerResponse>(inner, table, resolveEdits(table, edits));
}

kj::Promise<uint32_t> listener_response_answer_error(
    kj::HttpService::Response& response, const AbortReason& error) {
  auto& listener = KJ_REQUIRE_NONNULL(
      kj::tryDowncast<ListenerResponse>(response), "not a response new_listener_response() made");
  // Raising the error across the bridge is the bridge's own KjError -> kj::Exception conversion.
  auto exception = KJ_ASSERT_NONNULL(kj::runCatchingExceptions([&]() { error.raise(); }));
  co_return co_await listener.answerError(exception);
}

kj::Own<kj::HttpService::ConnectResponse> new_null_connect_response() {
  return kj::heap<NullConnectResponse>();
}

kj::Own<kj::WebSocketErrorHandler> new_jsgify_websocket_errors() {
  return kj::heap<JsgifyWebSocketErrors>();
}

kj::Own<WorkerInterface::CustomEvent> new_udp_connect_event(
    ::rust::Str address, ::rust::Box<UdpFlow> flow) {
  auto channel = kj::heap<UdpFlowChannel>(kj::mv(flow));
  return kj::heap<api::UdpConnectCustomEvent>(kj::str(address), *channel).attach(kj::mv(channel));
}

}  // namespace workerd::server
