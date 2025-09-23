# git-pick

`git-pick` is a command line tool that reproduces a commit from one Git repository in another repository by copying the changed files. It is useful when you need to selectively synchronise changes across repositories that are not directly related, while keeping full control over which paths are mapped or skipped.

## Features

- Replays a single commit from a source repository into a destination repository.
- Supports mapping paths from the source repository to different destinations.
- Allows skipping paths that should not be synchronised.
- Fails fast when the destination repository is dirty or when merge commits are provided.

## Installation

```bash
cargo install --path .
```

Alternatively, build the binary directly from the repository:

```bash
cargo build --release
```

The resulting executable can be found at `target/release/git-pick`.

## Usage

```
git-pick --source <PATH> --dest <PATH> --commit <OID> [--map <SRC=DEST> ...] [--skip <PATH> ...]
```

- `--source` – path to the repository containing the commit to copy.
- `--dest` – path to the destination repository where the commit should be applied.
- `--commit` – the commit hash from the source repository to replay.
- `--map` – optional path mappings in the form `source=destination` that rewrite the commit paths.
- `--skip` – optional paths that should be ignored during synchronisation.

### Example

```bash
git-pick \
  --source /path/to/source/repo \
  --dest /path/to/destination/repo \
  --commit a1b2c3d4 \
  --map "crates/foo=packages/foo" \
  --skip "crates/foo/tests"
```

This command reproduces commit `a1b2c3d4` from the source repository in the destination repository, mapping files under `crates/foo` to `packages/foo` and skipping files under `crates/foo/tests`.

## Development

This repository uses Rust 1.77 or newer (Edition 2024). Before opening a pull request, ensure the following checks pass locally:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all --locked
```

Continuous integration runs the same commands on every push and pull request, so keeping them green locally will help avoid surprises in CI.
