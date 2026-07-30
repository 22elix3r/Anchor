# Anchor

Anchor is a local-first Rust CLI for isolating filesystem changes observed during
an interactive coding-agent session.

The project is under active development. Its core safety rule is that restoration
must never destroy a change that existed before the tracked session or silently
overwrite a change made afterward.

Anchor is agent-agnostic:

```console
anchor run -- codex
anchor run -- claude
anchor run -- aider
anchor run -- bash
```

Anchor does not attribute changes to a process. It reports filesystem differences
observed during a session window.

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT License

at your option.

