use nu_plugin::{MsgPackSerializer, serve_plugin};
use nu_plugin_zstdsep::ZstdsepPlugin;

fn main() {
    serve_plugin(&ZstdsepPlugin::default(), MsgPackSerializer)
}
