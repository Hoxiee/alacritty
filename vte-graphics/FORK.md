# Local parser fork

Vendored from `vte-graphics` 0.15.0, source commit
`22c7c29c0303398baa71db8798473ee704f9717c` in
https://github.com/ayosec/vte-graphics.

The original Apache-2.0 and MIT licenses are preserved. This workspace copy adds
streaming APC callbacks for Kitty graphics, a bounded OSC buffer, and cancellation
handling. The normal text parser retains its fast ground-state path.

Keep this source local: the published parser discards APC strings and cannot
provide Kitty graphics support. Run its tests with `cargo test -p vte-graphics
--features ansi` and `cargo test -p vte-graphics --no-default-features` when updating.
