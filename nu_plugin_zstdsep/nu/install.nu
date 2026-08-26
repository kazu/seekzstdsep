# Link `zstdsep-hook.nu` into nushell's autoload directory, or take it back out.
#
#     nu nu_plugin_zstdsep/nu/install.nu
#     nu nu_plugin_zstdsep/nu/install.nu --uninstall
#
# A link rather than a copy: the hook tracks the plugin's flags, and a checkout that moves ahead of
# an installed copy is a hook that forwards flags the plugin no longer has.

const HOOK = (path self "zstdsep-hook.nu")

# What the autoload directory holds under our name, or null. `path exists` would not do: it follows
# a link, so a dangling one reads as absent and would be overwritten silently.
def installed [link: path]: nothing -> any {
    let dir = ($link | path dirname)
    if not ($dir | path exists) { return null }
    let found = (ls --long --all $dir | where name == $link)
    if ($found | is-empty) { null } else { $found | first }
}

def main [--uninstall] {
    let link = ($nu.default-config-dir | path join autoload ($HOOK | path basename))
    let current = (installed $link)

    if $uninstall {
        if $current == null {
            print $"nothing installed at ($link)"
            return
        }
        if $current.type != symlink {
            error make { msg: $"($link) is a file, not a link this script made: remove it yourself" }
        }
        rm $link
        print $"removed ($link)"
        return
    }

    if $current != null and $current.type != symlink {
        error make { msg: $"($link) already exists and is not a link: move it aside first" }
    }
    mkdir ($link | path dirname)
    if $current != null { rm $link }
    ^ln --symbolic $HOOK $link
    print $"($link) -> ($HOOK)"
    print "start a new nushell to pick it up; autoload is read for the REPL only"
}
