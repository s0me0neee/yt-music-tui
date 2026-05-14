# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build          # compile
cargo run            # run (opens browser for Google login on first use)
cargo check          # fast type-check without linking
cargo test           # run tests
cargo test <name>    # run a single test by name
```

## Credential Setup

Before running, two files must exist in the project root:

- `clientid` — plain text file containing the OAuth 2.0 client ID (e.g. `xxxx.apps.googleusercontent.com`)
- `secret/<client_id>.json` — JSON file with `{"client_id": "...", "client_secret": "..."}` downloaded from Google Cloud Console

These are intentionally not env vars. `auth::load_client()` reads them at startup.

## Architecture

The app is a Rust TUI (ratatui) that plays YouTube Music. It is early-stage; `main.rs` currently just runs the auth + playlist fetch flow as a proof of concept.

**Auth flow (`src/auth.rs`):**
- `load_client()` reads credentials from the local files above
- `login()` runs OAuth 2.0 Authorization Code + PKCE: opens the browser, spins up a local TCP server on `127.0.0.1:8080` to capture the redirect, then POSTs to Google's token endpoint to exchange the code for an access token
- The access token is returned as a plain `String` (no persistence yet)

**API (`src/api.rs`):**
- Stateless functions that take an `&str` access token and call the YouTube Data API v3
- Currently only `fetch_playlists()` is implemented

**TUI (`src/app.rs`):**
- Placeholder ratatui loop — not yet wired into `main.rs`

## Dependency notes

Cargo resolves versions more loosely than expected in edition 2024. When adding or updating dependencies, pin exact versions in `Cargo.toml` and check `cargo tree` to confirm the resolved version before writing code against a specific API. When a crate API error appears, search `docs.rs/<crate>/<version>` before attempting fixes — reqwest 0.13 gates `.form()` and `.query()` behind separate feature flags (`"form"`, `"query"`), not included in `"json"`.
