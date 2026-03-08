# Autosprint

a lightweight autosprint

## Prerequisites
you need Rust installed (cargo + rustc). Windows is required for the hook/input behavior.

## How to Build
run build.cmd

## Runtime Flags
- `--latency balanced` (default): lower CPU usage with fast response.
- `--latency ultra`: most aggressive response profile, can use more CPU while moving.
- `--help`: print command usage.

Examples:
- `target\release\autosprint-mcbe.exe --latency balanced`
- `target\release\autosprint-mcbe.exe --latency ultra`
