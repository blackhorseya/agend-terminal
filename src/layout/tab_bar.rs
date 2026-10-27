//! Tab-bar geometry — the single source of truth for where each tab (and the
//! trailing `[+]` button) sits when the bar wraps onto multiple rows.
//!
//! `render_tab_bar` (paint) and `tab_bar_hit_test` (mouse) both consume
//! [`tab_bar_geometry`], so they cannot drift on widths or wrap points (the
//! #777 misaligned-click class of bug). The function is pure: it only sees the
//! per-tab widths (`*` dot + label) and the available terminal width.

/// Upper bound on tab-bar rows. Tabs that would need a row past this cap are
/// hidden (not drawn, not clickable) so the bar can never eat the pane area.
pub const MAX_TAB_BAR_ROWS: u16 = 3;

/// Width of the trailing ` [+] ` button (leading and trailing space included).
pub const NEW_TAB_WIDTH: u16 = 5;

/// Placement of one tab-bar element. `width` is the visible width: a label
/// wider than the bar is clipped to the bar width and owns its row alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabBarSlot {
    pub row: u16,
    pub col: u16,
    pub width: u16,
}

/// What a tab-bar cell resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabBarHit {
    Tab(usize),
    NewTab,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabBarGeometry {
    /// One entry per tab, in order. `None` = hidden by [`MAX_TAB_BAR_ROWS`].
    pub tabs: Vec<Option<TabBarSlot>>,
    pub new_tab: TabBarSlot,
    /// Number of rows the bar occupies (`1..=MAX_TAB_BAR_ROWS`).
    pub rows: u16,
}

impl TabBarGeometry {
    pub fn hit(&self, row: u16, col: u16) -> Option<TabBarHit> {
        let inside = |s: &TabBarSlot| {
            s.row == row && col >= s.col && u32::from(col) < u32::from(s.col) + u32::from(s.width)
        };
        if let Some(i) = self
            .tabs
            .iter()
            .position(|s| s.as_ref().is_some_and(inside))
        {
            return Some(TabBarHit::Tab(i));
        }
        inside(&self.new_tab).then_some(TabBarHit::NewTab)
    }
}

/// Lay out tabs of the given widths (each already includes the `*` dot) in a
/// bar `width` columns wide. Tabs are separated by one column and wrap only
/// between tabs; ` [+] ` follows the last tab with no separator.
///
/// - A tab wider than the bar is clipped to `width` and owns its row alone.
/// - Rows are capped at [`MAX_TAB_BAR_ROWS`]; tabs past the cap are hidden
///   (`None`), and trailing tabs on the last row are dropped as needed so the
///   `[+]` button always stays visible and clickable.
pub fn tab_bar_geometry(tab_widths: &[u16], width: u16) -> TabBarGeometry {
    let width = width.max(1);
    let mut tabs: Vec<Option<TabBarSlot>> = Vec::with_capacity(tab_widths.len());
    let (mut row, mut x) = (0u16, 0u16);
    let mut capped = false;

    for &w in tab_widths {
        if capped {
            tabs.push(None);
            continue;
        }
        let w = w.min(width);
        // `x == 0` means the row is empty: no separator, and a clipped tab fits.
        if x > 0 && u32::from(x) + 1 + u32::from(w) > u32::from(width) {
            row += 1;
            x = 0;
        }
        if row >= MAX_TAB_BAR_ROWS {
            capped = true;
            tabs.push(None);
            continue;
        }
        let col = if x == 0 { 0 } else { x + 1 }; // fits: x + 1 + w <= width <= u16::MAX
        tabs.push(Some(TabBarSlot { row, col, width: w }));
        x = col + w;
    }
    if capped {
        row = MAX_TAB_BAR_ROWS - 1;
        x = tabs
            .iter()
            .flatten()
            .filter(|s| s.row == row)
            .map(|s| s.col.saturating_add(s.width))
            .max()
            .unwrap_or(0);
    }

    let new_w = NEW_TAB_WIDTH.min(width);
    if u32::from(x) + u32::from(new_w) > u32::from(width) {
        if row + 1 < MAX_TAB_BAR_ROWS {
            row += 1;
            x = 0;
        } else {
            // Last allowed row is full: hide trailing tabs until [+] fits.
            while u32::from(x) + u32::from(new_w) > u32::from(width) {
                let Some(last) = tabs
                    .iter_mut()
                    .rev()
                    .find(|s| s.is_some_and(|s| s.row == row))
                else {
                    x = 0;
                    break;
                };
                *last = None;
                x = tabs
                    .iter()
                    .flatten()
                    .filter(|s| s.row == row)
                    .map(|s| s.col.saturating_add(s.width))
                    .max()
                    .unwrap_or(0);
            }
        }
    }
    TabBarGeometry {
        tabs,
        new_tab: TabBarSlot {
            row,
            col: x,
            width: new_w,
        },
        rows: row + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(row: u16, col: u16, width: u16) -> Option<TabBarSlot> {
        Some(TabBarSlot { row, col, width })
    }

    /// Wide enough: identical to the pre-wrap single-row layout (regression).
    #[test]
    fn single_row_matches_legacy_layout() {
        let g = tab_bar_geometry(&[4, 7, 4], 200);
        assert_eq!(g.rows, 1);
        assert_eq!(g.tabs, vec![slot(0, 0, 4), slot(0, 5, 7), slot(0, 13, 4)]);
        // ` [+] ` directly after the last tab, no separator.
        assert_eq!(
            g.new_tab,
            TabBarSlot {
                row: 0,
                col: 17,
                width: 5
            }
        );
    }

    #[test]
    fn no_tabs_only_new_tab_button() {
        let g = tab_bar_geometry(&[], 80);
        assert_eq!(g.rows, 1);
        assert!(g.tabs.is_empty());
        assert_eq!(g.hit(0, 0), Some(TabBarHit::NewTab));
    }

    /// Exactly full row (tabs + ` [+] ` == width) stays on one row.
    #[test]
    fn exactly_full_row_does_not_wrap() {
        // 4 + 1 + 4 + 5 = 14
        let g = tab_bar_geometry(&[4, 4], 14);
        assert_eq!(g.rows, 1);
        assert_eq!(g.new_tab.row, 0);
        assert_eq!(g.new_tab.col, 9);
        // one column narrower pushes only [+] to row 1
        let g = tab_bar_geometry(&[4, 4], 13);
        assert_eq!(g.rows, 2);
        assert_eq!(g.tabs, vec![slot(0, 0, 4), slot(0, 5, 4)]);
        assert_eq!(
            g.new_tab,
            TabBarSlot {
                row: 1,
                col: 0,
                width: 5
            }
        );
    }

    /// A tab that does not fit moves whole to the next row; no label is split.
    #[test]
    fn wraps_between_tabs_to_second_row() {
        // width 12: first two tabs (4+1+4=9) fit; third (needs 1+4) -> 14 > 12
        let g = tab_bar_geometry(&[4, 4, 4], 12);
        assert_eq!(g.tabs, vec![slot(0, 0, 4), slot(0, 5, 4), slot(1, 0, 4)]);
        assert_eq!(g.new_tab.row, 1);
        assert_eq!(g.new_tab.col, 4);
        assert_eq!(g.rows, 2);
    }

    /// A single label wider than the bar is clipped and owns its row.
    #[test]
    fn overlong_label_is_clipped_and_alone_on_its_row() {
        let g = tab_bar_geometry(&[4, 50, 4], 20);
        assert_eq!(g.tabs[0], slot(0, 0, 4));
        assert_eq!(g.tabs[1], slot(1, 0, 20), "clipped to bar width");
        assert_eq!(g.tabs[2], slot(2, 0, 4), "next tab cannot share the row");
        assert_eq!(g.rows, 3);
    }

    /// Tabs needing a row beyond the cap are hidden; [+] must stay reachable.
    #[test]
    fn rows_are_capped_and_new_tab_button_stays_visible() {
        let widths = vec![10u16; 12];
        let g = tab_bar_geometry(&widths, 24);
        assert_eq!(g.rows, MAX_TAB_BAR_ROWS);
        assert!(g.tabs.iter().any(|s| s.is_none()), "overflow tabs hidden");
        assert!(g.tabs.iter().flatten().all(|s| s.row < MAX_TAB_BAR_ROWS));
        assert!(g.new_tab.row < MAX_TAB_BAR_ROWS);
        assert!(g.new_tab.col + g.new_tab.width <= 24);
        // no visible slot overlaps [+] on its row
        for s in g.tabs.iter().flatten() {
            if s.row == g.new_tab.row {
                assert!(s.col + s.width <= g.new_tab.col);
            }
        }
        assert_eq!(g.hit(g.new_tab.row, g.new_tab.col), Some(TabBarHit::NewTab));
    }

    #[test]
    fn hit_resolves_row_and_column() {
        let g = tab_bar_geometry(&[4, 4, 4], 12);
        assert_eq!(g.hit(0, 0), Some(TabBarHit::Tab(0)));
        assert_eq!(g.hit(0, 5), Some(TabBarHit::Tab(1)));
        assert_eq!(g.hit(1, 0), Some(TabBarHit::Tab(2)));
        assert_eq!(g.hit(1, 3), Some(TabBarHit::Tab(2)));
        assert_eq!(g.hit(1, 4), Some(TabBarHit::NewTab));
        assert_eq!(g.hit(0, 4), None, "separator column");
        assert_eq!(g.hit(2, 0), None, "below the bar");
    }

    /// r2: sums near `u16::MAX` (the pre-first-render width) must not overflow.
    #[test]
    fn extreme_widths_do_not_overflow() {
        let g = tab_bar_geometry(&[u16::MAX, u16::MAX, 5], u16::MAX);
        assert_eq!(g.tabs[0], slot(0, 0, u16::MAX));
        assert_eq!(g.tabs[1], slot(1, 0, u16::MAX), "cannot share the row");
        assert!(g.rows <= MAX_TAB_BAR_ROWS);
        assert_eq!(g.hit(0, u16::MAX - 1), Some(TabBarHit::Tab(0)));
        let g = tab_bar_geometry(&[u16::MAX - 2, 3, 3], u16::MAX);
        assert!(g.rows <= MAX_TAB_BAR_ROWS);
    }

    #[test]
    fn zero_width_does_not_panic() {
        let g = tab_bar_geometry(&[4, 4], 0);
        assert!(g.rows >= 1 && g.rows <= MAX_TAB_BAR_ROWS);
    }
}
