# Code Style & Conventions

## Error Handling
- Use `color_eyre::Result` as return type
- `.context("description")` on all I/O operations and Command invocations
- `bail!("message")` for control flow errors
- `eyre!("message")` for error construction
- Multi-line error messages for user-facing failures
- stderr captured on Command failures

## Logging
- `info!()` — user-facing lifecycle events only
- `debug!()` — operational details (paths, config, steps)
- `warn!()` — non-fatal issues, cleanup failures
- Structured fields: `info!(image = %opts.image, "message")`

## Command Execution
- Always use `.output()` (not `.status()`) to capture stderr
- Add `.context("description")` on spawn/output
- Check `output.status.success()`, include stderr in error

## Process Cleanup
- RAII guards with Drop for cleanup
- `debug!()` in Drop, `warn!()` for failures (not `info!()`)

## Platform Separation
- `#[cfg(target_os = "linux")]` / `#[cfg(target_os = "macos")]`
- Cross-platform code in shared modules (ssh_options.rs, cpio.rs, common_opts.rs)

## Workspace Lints
- `unsafe_code = "deny"`
- `unused_must_use = "deny"`
- `missing_docs = "deny"` (not enforced on macOS WIP)
