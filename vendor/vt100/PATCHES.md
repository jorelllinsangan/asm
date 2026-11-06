# Local patches to vt100 0.16.2

This is the crates.io release of vt100 0.16.2 with one change, wired in through
`[patch.crates-io]` in the top-level `Cargo.toml`.

## Scrollback from a top-anchored scroll region (`src/grid.rs`, `scroll_up`)

Upstream only saves a row to scrollback when *no* scroll region is set. xterm
(and Ghostty, iTerm2, kitty) also save it when the region's top margin is the
top row of the screen. Codex's inline TUI depends on that: it keeps its composer
pinned at the bottom and inserts conversation history by setting
`ESC[1;Nr` and scrolling the rows above the composer. Upstream vt100 discards
every one of those rows, so a Codex session had no scrollback at all.

The condition is now `scroll_top == 0` instead of "no region active". Regions
that start lower down (a pinned status line, as in vim or tmux) are unchanged.

Pinned by `history_scrolled_out_of_a_top_anchored_region_reaches_scrollback` and
`a_region_below_the_top_row_still_keeps_its_rows_out_of_scrollback` in
`src/client.rs`.
