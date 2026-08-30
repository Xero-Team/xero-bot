# vendored: vercel_runtime 2.4.0
#
# Upstream: https://github.com/vercel/vercel/tree/main/crates/vercel_runtime
# License: Apache-2.0
#
# Why vendored: upstream 2.4.0 fails to compile on non-unix targets —
# `std::env` is imported under `#[cfg(unix)]` but `env::var("VERCEL_DEV_PORT")`
# is called unconditionally in `run()`. The one-line fix moves `use std::env;`
# to the crate root (see src/lib.rs). Behavior is otherwise identical, and on
# Linux (Vercel's runtime) the compiled result is unchanged.
#
# If upstream publishes a fix, remove this directory and the
# [patch.crates-io] section in Cargo.toml.
