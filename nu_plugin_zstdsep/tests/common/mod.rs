//! A compressed fixture, and a nushell to evaluate against it.
#![allow(dead_code)]
// `ShellError` is large by design and is what every nushell call returns; see the note in
// `src/lib.rs`.
#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};

use nu_plugin_test_support::PluginTest;
use nu_plugin_zstdsep::ZstdsepPlugin;
use nu_protocol::{ShellError, Span, Value, engine::Command};
use seekzstdsep::{CompressOptions, compress_to_seekable_zst_with_opts};

/// Records per frame the fixture is compressed into, so that the tests span more than one frame
/// and can tell a per-frame read from a whole-file one.
pub const RECORDS_PER_FRAME: usize = 20;
pub const RECORDS: usize = 105;

/// One JSON object per line, `seq` counting up, so any record can be identified by its index.
pub fn fixture_body() -> Vec<u8> {
    (0..RECORDS)
        .map(|i| {
            format!(
                "{{\"seq\":{i},\"lvl\":\"{}\",\"msg\":\"m{i}\",\"inner\":{{\"n\":{}}}}}\n",
                if i % 3 == 0 { "error" } else { "info" },
                i * 2
            )
        })
        .collect::<String>()
        .into_bytes()
}

/// Compresses [`fixture_body`] into `dir` under `name`, in frames of [`RECORDS_PER_FRAME`].
pub fn compress_fixture(dir: &Path, name: &str) -> PathBuf {
    compress_body(dir, name, fixture_body())
}

/// Compresses `body` into `dir` under `name`, in frames of [`RECORDS_PER_FRAME`].
pub fn compress_body(dir: &Path, name: &str, body: Vec<u8>) -> PathBuf {
    let out_path = dir.join(name);
    let mut input = std::io::Cursor::new(body);
    let mut sink = std::io::sink();

    compress_to_seekable_zst_with_opts(
        &mut input,
        &mut sink,
        4096,
        true,
        b"\n",
        None,
        Some(CompressOptions {
            out_dir: Some(dir.to_path_buf()),
            out_path: Some(out_path.clone()),
            max_of_separator: Some(RECORDS_PER_FRAME),
            ..Default::default()
        }),
    )
    .expect("Failed to compress the fixture");

    out_path
}

/// A nushell with the plugin registered and the builtins the tests pipe through.
pub fn nu() -> PluginTest {
    let mut test =
        PluginTest::new("zstdsep", ZstdsepPlugin::default().into()).expect("Failed to start");
    for decl in builtins() {
        test.add_decl(decl).expect("Failed to add a builtin");
    }
    test
}

fn builtins() -> Vec<Box<dyn Command>> {
    vec![
        Box::new(nu_command::Get),
        Box::new(nu_command::First),
        Box::new(nu_command::Last),
        Box::new(nu_command::Length),
        Box::new(nu_command::Where),
        Box::new(nu_command::Select),
        Box::new(nu_command::Columns),
        Box::new(nu_command::FromTsv),
        Box::new(nu_command::ToTsv),
    ]
}

/// Evaluates `source` and returns the one value it produced.
pub fn eval(test: &mut PluginTest, source: &str) -> Result<Value, ShellError> {
    test.eval(source)?.into_value(Span::test_data())
}
