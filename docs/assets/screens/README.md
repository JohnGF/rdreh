# Screenshot Assets

Screenshots for the rdreh GitHub Pages landing site.

## Specifications
- **Format:** PNG
- **Dimensions:** 1600 × 1000 pixels (16:10). The site shows them with `object-fit: cover`, so any other ratio gets cropped.

## Files
- `hero.png`: agents workspace (quad). One Claude Code working, one finished, status bar `Finished: 3/4`.
- `cockpit.png`: telemetry workspace in the 5-pane cockpit layout (git log, tokens, tests, status, diff).
- `states.png`: zoomed strips of a working pane, a finished pane and the status bar count.
- `tokens.png`: zoom on the `rdr tokens --watch` pane.
- `sessions.png`: detach with `Ctrl+b d`, then `rdr list` / `rdr counts` show everything still running.
- `layouts.png`: editor workspace in the tri layout (nvim, Claude Code, shell).
- `paste.png`: three copied files pasted as one line of paths; Claude Code attaches them as images and `ls -l` resolves them.

Still needed (optional): `terminals.png`.

## How they were made
A demo session (`rdr --session demo`, its own config) ran inside a detached tmux at 160×48 cells.
`tmux capture-pane -e` was rendered to HTML on a fixed cell grid (Noto Sans Mono, Nord colors),
screenshotted with headless Chromium at 2x, then cropped and resized with ImageMagick.
No desktop screenshot is involved, so nothing outside the demo session can end up in an image.
