//! Deterministic layout engine: pure functions from (preset, pane count, area)
//! to pane rectangles. No terminal or PTY state lives here, so every geometry
//! rule is unit-testable.

use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    /// Focused pane full-screen; the others keep running hidden.
    Single,
    /// Equal-width columns.
    Dual,
    /// Master-stack: first pane 60% left, the rest stacked on the right.
    Tri,
    /// Tiled grid (2x2 for four panes).
    Quad,
    /// Equal-height rows.
    Rows,
    /// Five-pane telemetry cockpit (falls back to `Quad` for other counts).
    Cockpit,
}

impl Layout {
    pub fn parse(s: &str) -> Option<Layout> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "single" | "1" | "full" => Layout::Single,
            "dual" | "2" | "split" => Layout::Dual,
            "tri" | "3" | "master" => Layout::Tri,
            "quad" | "4" | "grid" | "tiled" => Layout::Quad,
            "rows" | "horizontal" => Layout::Rows,
            "cockpit" | "telemetry" | "5" => Layout::Cockpit,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Layout::Single => "single",
            Layout::Dual => "dual",
            Layout::Tri => "tri",
            Layout::Quad => "quad",
            Layout::Rows => "rows",
            Layout::Cockpit => "cockpit",
        }
    }

    /// The Alt+l cycle: Single -> Dual -> Tri -> Quad -> Rows -> Cockpit -> Single.
    pub fn next(self) -> Layout {
        match self {
            Layout::Single => Layout::Dual,
            Layout::Dual => Layout::Tri,
            Layout::Tri => Layout::Quad,
            Layout::Quad => Layout::Rows,
            Layout::Rows => Layout::Cockpit,
            Layout::Cockpit => Layout::Single,
        }
    }

    /// Minimum number of panes the preset needs to look like itself.
    pub fn min_panes(self) -> usize {
        match self {
            Layout::Single | Layout::Rows => 1,
            Layout::Dual => 2,
            Layout::Tri => 3,
            Layout::Quad => 4,
            Layout::Cockpit => 5,
        }
    }
}

/// Split `total` cells into `n` parts that differ by at most one cell and sum
/// to exactly `total`.
fn even_split(total: u16, n: usize) -> Vec<u16> {
    if n == 0 {
        return Vec::new();
    }
    let n16 = n as u16;
    let base = total / n16;
    let extra = total % n16;
    (0..n16).map(|i| base + u16::from(i < extra)).collect()
}

fn columns(area: Rect, n: usize) -> Vec<Rect> {
    let mut x = area.x;
    even_split(area.width, n)
        .into_iter()
        .map(|w| {
            let r = Rect::new(x, area.y, w, area.height);
            x += w;
            r
        })
        .collect()
}

fn rows(area: Rect, n: usize) -> Vec<Rect> {
    let mut y = area.y;
    even_split(area.height, n)
        .into_iter()
        .map(|h| {
            let r = Rect::new(area.x, y, area.width, h);
            y += h;
            r
        })
        .collect()
}

fn split_h(area: Rect, left_pct: u16) -> (Rect, Rect) {
    let lw = (u32::from(area.width) * u32::from(left_pct) / 100) as u16;
    (
        Rect::new(area.x, area.y, lw, area.height),
        Rect::new(area.x + lw, area.y, area.width - lw, area.height),
    )
}

fn split_v(area: Rect) -> (Rect, Rect) {
    let th = area.height / 2;
    (
        Rect::new(area.x, area.y, area.width, th),
        Rect::new(area.x, area.y + th, area.width, area.height - th),
    )
}

/// tmux-style `tiled`: ceil(sqrt(n)) columns; panes in a short last row stretch.
fn tiled(area: Rect, n: usize) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }
    let cols = (n as f64).sqrt().ceil() as usize;
    let nrows = n.div_ceil(cols);
    let mut out = Vec::with_capacity(n);
    let mut left = n;
    for row in rows(area, nrows) {
        let in_row = left.min(cols);
        out.extend(columns(row, in_row));
        left -= in_row;
    }
    out
}

/// Rectangles for `n` panes. For `Single` every pane gets the full area (only
/// the focused one is drawn), which keeps hidden PTYs sized sensibly.
pub fn compute(layout: Layout, n: usize, area: Rect) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![area];
    }
    match layout {
        Layout::Single => vec![area; n],
        Layout::Dual => columns(area, n),
        Layout::Rows => rows(area, n),
        Layout::Quad => tiled(area, n),
        Layout::Tri => {
            let (main, stack) = split_h(area, 60);
            let mut v = vec![main];
            v.extend(rows(stack, n - 1));
            v
        }
        Layout::Cockpit if n == 5 => {
            // Top: [0 | 1]. Bottom: [2 | 3 | 4] where 2+3 share the left half.
            let (top, bottom) = split_v(area);
            let (tl, tr) = split_h(top, 50);
            let (bl, br) = split_h(bottom, 50);
            let (bl1, bl2) = split_h(bl, 50);
            vec![tl, tr, bl1, bl2, br]
        }
        Layout::Cockpit => tiled(area, n),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Index of the nearest rect in `dir` from `rects[from]`, preferring the one
/// that overlaps most along the perpendicular axis.
pub fn neighbor(rects: &[Rect], from: usize, dir: Dir) -> Option<usize> {
    let cur = *rects.get(from)?;
    let overlap = |a0: u16, a1: u16, b0: u16, b1: u16| -> i32 {
        i32::from(a1.min(b1)) - i32::from(a0.max(b0))
    };
    rects
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != from)
        .filter_map(|(i, r)| {
            let (dist, ov) = match dir {
                Dir::Left if r.right() <= cur.x => (
                    i32::from(cur.x) - i32::from(r.right()),
                    overlap(cur.y, cur.bottom(), r.y, r.bottom()),
                ),
                Dir::Right if r.x >= cur.right() => (
                    i32::from(r.x) - i32::from(cur.right()),
                    overlap(cur.y, cur.bottom(), r.y, r.bottom()),
                ),
                Dir::Up if r.bottom() <= cur.y => (
                    i32::from(cur.y) - i32::from(r.bottom()),
                    overlap(cur.x, cur.right(), r.x, r.right()),
                ),
                Dir::Down if r.y >= cur.bottom() => (
                    i32::from(r.y) - i32::from(cur.bottom()),
                    overlap(cur.x, cur.right(), r.x, r.right()),
                ),
                _ => return None,
            };
            (ov > 0).then_some((dist, -ov, i))
        })
        .min()
        .map(|(_, _, i)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 200,
        height: 50,
    };

    fn covers_exactly(rects: &[Rect], area: Rect) {
        let total: u32 = rects.iter().map(|r| r.area()).sum();
        assert_eq!(total, area.area(), "rects must tile the area: {rects:?}");
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(a.intersection(*b).is_empty(), "overlap {a:?} {b:?}");
            }
        }
    }

    #[test]
    fn presets_tile_without_gaps() {
        for layout in [Layout::Dual, Layout::Tri, Layout::Quad, Layout::Rows, Layout::Cockpit] {
            for n in 1..=7 {
                covers_exactly(&compute(layout, n, AREA), AREA);
            }
        }
    }

    #[test]
    fn odd_sizes_still_tile() {
        let area = Rect::new(3, 1, 157, 41);
        for n in 1..=9 {
            covers_exactly(&compute(Layout::Quad, n, area), area);
            covers_exactly(&compute(Layout::Dual, n, area), area);
        }
        covers_exactly(&compute(Layout::Cockpit, 5, area), area);
    }

    #[test]
    fn quad_is_two_by_two() {
        let r = compute(Layout::Quad, 4, AREA);
        assert_eq!(r[0], Rect::new(0, 0, 100, 25));
        assert_eq!(r[3], Rect::new(100, 25, 100, 25));
    }

    #[test]
    fn tri_master_is_sixty_percent() {
        let r = compute(Layout::Tri, 3, AREA);
        assert_eq!(r[0].width, 120);
        assert_eq!(r[1], Rect::new(120, 0, 80, 25));
    }

    #[test]
    fn cockpit_geometry() {
        let r = compute(Layout::Cockpit, 5, AREA);
        assert_eq!(r[0], Rect::new(0, 0, 100, 25));
        assert_eq!(r[1], Rect::new(100, 0, 100, 25));
        assert_eq!(r[2], Rect::new(0, 25, 50, 25));
        assert_eq!(r[3], Rect::new(50, 25, 50, 25));
        assert_eq!(r[4], Rect::new(100, 25, 100, 25));
    }

    #[test]
    fn neighbors_in_quad() {
        let r = compute(Layout::Quad, 4, AREA);
        assert_eq!(neighbor(&r, 0, Dir::Right), Some(1));
        assert_eq!(neighbor(&r, 0, Dir::Down), Some(2));
        assert_eq!(neighbor(&r, 3, Dir::Up), Some(1));
        assert_eq!(neighbor(&r, 3, Dir::Left), Some(2));
        assert_eq!(neighbor(&r, 0, Dir::Left), None);
    }

    #[test]
    fn neighbors_in_cockpit() {
        let r = compute(Layout::Cockpit, 5, AREA);
        assert_eq!(neighbor(&r, 0, Dir::Down), Some(2));
        assert_eq!(neighbor(&r, 1, Dir::Down), Some(4));
        assert_eq!(neighbor(&r, 3, Dir::Right), Some(4));
        assert_eq!(neighbor(&r, 4, Dir::Left), Some(3));
        assert_eq!(neighbor(&r, 3, Dir::Up), Some(0));
    }

    #[test]
    fn cycle_order() {
        let mut l = Layout::Single;
        let seen: Vec<_> = (0..6).map(|_| { l = l.next(); l }).collect();
        assert_eq!(
            seen,
            [Layout::Dual, Layout::Tri, Layout::Quad, Layout::Rows, Layout::Cockpit, Layout::Single]
        );
    }
}
