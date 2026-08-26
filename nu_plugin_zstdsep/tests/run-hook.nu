# Run `tests/hook.nu` against a freshly registered plugin, in a directory of its own.
#
#     nu nu_plugin_zstdsep/tests/run-hook.nu [path-to-nu_plugin_zstdsep]
#
# `plugin add` and `plugin use` cannot happen in one script — `plugin use` is resolved at parse
# time — so registering the plugin and running the tests are two invocations of `nu`.
#
# The scratch directory is `XDG_CONFIG_HOME` as well as the working directory, which keeps the
# caller's `config.nu` and autoload files out of the run. `--no-config-file` would do that too, but
# it also clears `$nu.plugin-path`, and then `plugin use` has nowhere to look.

const TESTS = (path self | path dirname)

# Where `cargo build` puts the plugin, release before debug.
def plugin-binary []: nothing -> path {
    let target = (^cargo metadata --format-version 1 --no-deps | from json | get target_directory)
    let candidates = ([release, debug] | each {|profile| $target | path join $profile nu_plugin_zstdsep })
    let built = ($candidates | where {|bin| $bin | path exists })
    if ($built | is-empty) {
        error make { msg: $"no plugin binary at ($candidates | str join ' or '): cargo build -p nu_plugin_zstdsep first" }
    }
    $built | first
}

def main [binary?: path] {
    let binary = ($binary | default (plugin-binary) | path expand)
    let scratch = (mktemp --directory --tmpdir zstdsep-hook-XXXXXX)
    let registry = ($scratch | path join plugins.msgpackz)
    with-env { XDG_CONFIG_HOME: $scratch } {
        cd $scratch
        ^nu --plugin-config $registry --commands $"plugin add --plugin-config ($registry) ($binary)"
        ^nu --plugin-config $registry ($TESTS | path join hook.nu)
    }
    cd $env.HOME
    rm --recursive --force $scratch
}
