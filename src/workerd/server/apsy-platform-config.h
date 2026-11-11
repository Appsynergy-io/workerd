#pragma once

// Decodes `workerd serve --platform-config`'s text (apsy.capnp `Platform`) into an encoded
// message, for the command line (cli/lib.rs) through the cxx bridge in cli/bridge.rs. Cap'n
// Proto's text format is decoded by capnp::TextCodec, which only the C++ library has.

#include <rust/cxx.h>

#include <cstdint>

namespace workerd::server::cli {

// The `Platform` struct literal `text` as an encoded message (segment table, then segments), in
// 8-byte words. Fails with the parser's message on text that is not one.
::rust::Vec<uint64_t> decode_platform_config(::rust::Str text);

}  // namespace workerd::server::cli
