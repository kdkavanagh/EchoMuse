//! Generates the ESPHome native API message types from the vendored schema.
//!
//! protox compiles `proto/api.proto` (and the `api_options.proto` it imports)
//! in Rust, so the build needs no protoc binary. prost-build turns the result
//! into message structs. Each message's numeric type, which is what the wire
//! carries, lives in the custom `(id)` message option; prost does not keep
//! custom options, so this script reads them through prost-reflect and writes
//! an `ApiMessage` impl per message plus a table of every message type.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use heck::ToUpperCamelCase;
use prost_reflect::Value;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let proto_dir = manifest_dir.join("proto");
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    println!("cargo:rerun-if-changed=proto/api.proto");
    println!("cargo:rerun-if-changed=proto/api_options.proto");

    let mut compiler = protox::Compiler::new([&proto_dir])?;
    compiler.include_imports(true);
    compiler.open_files(["api.proto"])?;
    let pool = compiler.descriptor_pool();

    // Generate Rust only for the two vendored files; descriptor.proto is
    // imported solely for the option extensions, which prost ignores.
    let mut descriptors = compiler.file_descriptor_set();
    descriptors
        .file
        .retain(|file| matches!(file.name(), "api.proto" | "api_options.proto"));
    prost_build::Config::new()
        .out_dir(&out_dir)
        .bytes(["."])
        .disable_comments(["."])
        .default_package_filename("esphome_api")
        .compile_fds(descriptors)?;

    let id_option = pool
        .get_extension_by_name("id")
        .ok_or("api_options.proto has no `id` extension")?;
    let source_option = pool
        .get_extension_by_name("source")
        .ok_or("api_options.proto has no `source` extension")?;

    let mut messages = Vec::new();
    for message in pool.all_messages() {
        if message.parent_file().name() != "api.proto" {
            continue;
        }
        let options = message.options();
        if !options.has_extension(&id_option) {
            continue;
        }
        let id = match options.get_extension(&id_option).as_ref() {
            Value::U32(id) => u16::try_from(*id)?,
            other => return Err(format!("{}: `id` is {other:?}", message.name()).into()),
        };
        let source = match options.get_extension(&source_option).as_ref() {
            Value::EnumNumber(0) => "Both",
            Value::EnumNumber(1) => "Server",
            Value::EnumNumber(2) => "Client",
            other => return Err(format!("{}: `source` is {other:?}", message.name()).into()),
        };
        messages.push((message.name().to_owned(), id, source));
    }
    messages.sort_by_key(|(_, id, _)| *id);
    for pair in messages.windows(2) {
        if pair[0].1 == pair[1].1 {
            return Err(format!("{} and {} share id {}", pair[0].0, pair[1].0, pair[0].1).into());
        }
    }

    let mut code = String::new();
    for (name, id, source) in &messages {
        let rust_name = name.to_upper_camel_case();
        writeln!(
            code,
            "impl crate::api::ApiMessage for {rust_name} {{\n    \
             const ID: u16 = {id};\n    \
             const NAME: &'static str = \"{name}\";\n    \
             const SOURCE: crate::api::MessageSource = crate::api::MessageSource::{source};\n}}"
        )?;
    }
    writeln!(
        code,
        "/// Every message type in the schema: its number, proto name and sender, by number.\n\
         pub const MESSAGE_TYPES: &[(u16, &str, crate::api::MessageSource)] = &["
    )?;
    for (name, id, source) in &messages {
        writeln!(
            code,
            "    ({id}, \"{name}\", crate::api::MessageSource::{source}),"
        )?;
    }
    writeln!(code, "];")?;
    fs::write(out_dir.join("message_ids.rs"), code)?;
    Ok(())
}
