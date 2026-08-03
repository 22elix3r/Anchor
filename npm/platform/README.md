# Fence platform package

This package contains one platform-specific `fence` executable. Install the
public `fence-cli` package instead of depending on this package directly. The
launcher verifies the executable digest before replacing itself with the Rust
process. Fence performs no installation-time download and has no npm lifecycle
scripts.
