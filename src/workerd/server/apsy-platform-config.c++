#include "apsy-platform-config.h"

#include <workerd/server/apsy.capnp.h>

#include <kj-rs/kj-rs.h>

#include <capnp/message.h>
#include <capnp/serialize-text.h>
#include <capnp/serialize.h>

using namespace kj_rs;

namespace workerd::server::cli {

::rust::Vec<uint64_t> decode_platform_config(::rust::Str text) {
  capnp::MallocMessageBuilder message;
  capnp::TextCodec codec;
  // The codec takes a NUL-terminated string.
  auto copy = kj::str(kj::ArrayPtr<const char>(text.data(), text.size()));
  message.adoptRoot(codec.decode<apsy::Platform>(copy, message.getOrphanage()));
  auto words = capnp::messageToFlatArray(message);
  return kj::arrayPtr(reinterpret_cast<const uint64_t*>(words.begin()), words.size())
      .as<RustCopy>();
}

}  // namespace workerd::server::cli
