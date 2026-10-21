Describe the user-visible change and the relevant graphics protocol behavior.

Validation:

- [ ] `cargo test --workspace --locked`
- [ ] `cargo test -p alacritty_terminal --no-default-features --locked`
- [ ] `cargo +1.85.0 clippy --workspace --all-targets --locked`
- [ ] `cargo +nightly fmt --all -- --check`
- [ ] Graphics changes: run `scripts/graphics-smoke.py`
