# Configuration

Anchor resolves capture policy in this order:

1. built-in safe defaults;
2. optional user configuration;
3. optional project restrictions;
4. explicit `anchor run` flags.

On Unix the user file is
`$XDG_CONFIG_HOME/anchor/config.toml`, falling back to
`$HOME/.config/anchor/config.toml`. On Windows it is
`%APPDATA%/Anchor/config.toml`. `ANCHOR_CONFIG_FILE` selects an explicit user
file.

The project file is `.anchor/config.toml` at the worktree root. A project file
must be a regular file, not a symlink. It can lower limits or disable risky
options, but cannot raise limits or enable degraded capture, mount traversal,
or command-argument recording. Explicit command-line flags are direct user
authorization and are applied last.

```toml
[capture]
max_files = 250000
max_total_bytes = 2147483648
max_file_bytes = 268435456
allow_degraded = false
cross_mounts = false

[privacy]
record_command_arguments = false
```

Unknown keys, non-UTF-8 files, files over 1 MiB, zero limits, and malformed TOML
are refused before the wrapped command starts. The resolved policy is stored in
the session record, so review and restore do not depend on mutable configuration.

Equivalent explicit overrides are:

```console
anchor run \
  --max-files 500000 \
  --max-total-bytes 4294967296 \
  --max-file-bytes 536870912 \
  --allow-degraded \
  --cross-mounts \
  --record-arguments \
  -- command argument
```

`--allow-degraded` weakens rollback completeness. A degraded manifest remains
visible as degraded and is not eligible for automatic full rollback.
