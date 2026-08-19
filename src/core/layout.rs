#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridMetrics {
    pub card_width: f64,
    pub card_height: f64,
    pub horizontal_spacing: f64,
    pub vertical_spacing: f64,
    pub horizontal_padding: f64,
    pub top_inset: f64,
    pub bottom_inset: f64,
}

impl Default for GridMetrics {
    fn default() -> Self {
        Self {
            card_width: 280.0,
            card_height: 218.0,
            horizontal_spacing: 14.0,
            vertical_spacing: 14.0,
            horizontal_padding: 40.0,
            top_inset: 112.0,
            bottom_inset: 40.0,
        }
    }
}

impl GridMetrics {
    #[must_use]
    pub fn columns(self, viewport_width: f64) -> usize {
        let available = (viewport_width - self.horizontal_padding * 2.0).max(self.card_width);
        (((available + self.horizontal_spacing) / (self.card_width + self.horizontal_spacing))
            .floor() as usize)
            .max(1)
    }

    #[must_use]
    pub const fn rows(self, item_count: usize, columns: usize) -> usize {
        if item_count == 0 {
            0
        } else {
            let columns = if columns == 0 { 1 } else { columns };
            item_count.div_ceil(columns)
        }
    }

    #[must_use]
    pub fn content_height(self, item_count: usize, columns: usize) -> f64 {
        let rows = self.rows(item_count, columns);
        if rows == 0 {
            return self.top_inset + self.bottom_inset;
        }
        self.top_inset
            + rows as f64 * self.card_height
            + rows.saturating_sub(1) as f64 * self.vertical_spacing
            + self.bottom_inset
    }

    #[must_use]
    pub fn card_origin(
        self,
        index: usize,
        item_count: usize,
        viewport_width: f64,
        scroll_offset: f64,
    ) -> (f64, f64) {
        let columns = self.columns(viewport_width);
        let row = index / columns;
        let column = index % columns;
        let items_in_row = (item_count - row * columns).min(columns);
        let row_width = items_in_row as f64 * self.card_width
            + items_in_row.saturating_sub(1) as f64 * self.horizontal_spacing;
        let left = (viewport_width - row_width) * 0.5;
        (
            left + column as f64 * (self.card_width + self.horizontal_spacing),
            self.top_inset + row as f64 * (self.card_height + self.vertical_spacing)
                - scroll_offset,
        )
    }

    #[must_use]
    pub fn reveal_offset(
        self,
        selected_index: usize,
        columns: usize,
        viewport_height: f64,
        current_offset: f64,
        item_count: usize,
    ) -> f64 {
        if item_count == 0 {
            return 0.0;
        }
        let row = selected_index.min(item_count - 1) / columns.max(1);
        let top = self.top_inset + row as f64 * (self.card_height + self.vertical_spacing);
        let bottom = top + self.card_height;
        let visible_top = current_offset + self.top_inset;
        let visible_bottom = current_offset + viewport_height - self.bottom_inset;
        let desired = if top < visible_top {
            (top - self.top_inset).max(0.0)
        } else if bottom > visible_bottom {
            bottom - viewport_height + self.bottom_inset
        } else {
            current_offset
        };
        let max_offset = (self.content_height(item_count, columns) - viewport_height).max(0.0);
        desired.clamp(0.0, max_offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_never_drop_below_one() {
        let metrics = GridMetrics::default();
        assert_eq!(metrics.columns(100.0), 1);
        assert_eq!(metrics.columns(1920.0), 6);
    }

    #[test]
    fn incomplete_last_row_is_centered() {
        let metrics = GridMetrics::default();
        let (x, _) = metrics.card_origin(4, 5, 1280.0, 0.0);
        assert!((x - 500.0).abs() < 0.001);
    }

    #[test]
    fn selection_is_revealed_and_clamped() {
        let metrics = GridMetrics::default();
        let columns = metrics.columns(1000.0);
        let offset = metrics.reveal_offset(49, columns, 700.0, 0.0, 50);
        let max = metrics.content_height(50, columns) - 700.0;
        assert!(offset > 0.0);
        assert!(offset <= max);
    }
}
