#![allow(dead_code)]

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tabs::TabId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabBarVisibility {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabBarPosition {
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabBarAlignment {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabBarCloseButtonVisibility {
    Always,
    Hover,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabBarEntry {
    pub id: TabId,
    pub index: usize,
    pub label: String,
    pub active: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TabBarState {
    pub tabs: Vec<TabBarEntry>,
    pub hit_regions: Vec<(TabId, usize, usize)>,
    pub close_regions: Vec<(TabId, usize, usize)>,
    pub hovered_tab: Option<TabId>,
    pub row: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabBarPoint {
    pub row: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabBarRect {
    pub row: usize,
    pub start_column: usize,
    pub end_column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabBarSegment {
    pub text: String,
    pub active: bool,
    pub tab_id: Option<TabId>,
    pub start_column: usize,
    pub end_column: usize,
    pub is_close_button: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabBarLayout {
    pub visible: bool,
    pub position: TabBarPosition,
    pub reserved_rows: usize,
    pub row: Option<usize>,
    pub full_row_background: Option<TabBarRect>,
    pub segments: Vec<TabBarSegment>,
    pub hit_regions: Vec<(TabId, usize, usize)>,
    pub close_regions: Vec<(TabId, usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabBarLayoutInput<'a> {
    pub tabs: &'a [TabBarEntry],
    pub columns: usize,
    pub screen_lines: usize,
    pub search_lines: usize,
    pub message_lines: usize,
    pub visibility: TabBarVisibility,
    pub position: TabBarPosition,
    pub alignment: TabBarAlignment,
    pub close_button_visibility: TabBarCloseButtonVisibility,
    pub hovered_tab: Option<TabId>,
    pub show_indices: bool,
    pub max_width: Option<usize>,
    pub min_width: usize,
}

pub fn layout_tab_bar(input: TabBarLayoutInput<'_>) -> TabBarLayout {
    let visible = match input.visibility {
        TabBarVisibility::Never => false,
        TabBarVisibility::Always => true,
        TabBarVisibility::Auto => input.tabs.len() >= 2,
    };

    let reserved_rows = usize::from(visible);
    let row = visible.then_some(match input.position {
        TabBarPosition::Bottom => input.screen_lines + input.search_lines + input.message_lines,
    });

    if !visible || input.columns == 0 || input.tabs.is_empty() {
        return TabBarLayout {
            visible,
            position: input.position,
            reserved_rows,
            row,
            full_row_background: row.map(|row| TabBarRect {
                row,
                start_column: 0,
                end_column: input.columns,
            }),
            segments: Vec::new(),
            hit_regions: Vec::new(),
            close_regions: Vec::new(),
        };
    }

    let mut prepared = Vec::with_capacity(input.tabs.len());
    for tab in input.tabs {
        let prefix =
            if input.show_indices { format!("{}: ", tab.index + 1) } else { String::new() };
        prepared.push(prepare_tab_segment(
            tab,
            &format!("{}{}", prefix, tab.label),
            input.max_width,
            input.min_width,
            input.close_button_visibility,
            input.hovered_tab,
        ));
    }

    let mut segments = Vec::new();
    let mut hit_regions = Vec::new();
    let mut close_regions = Vec::new();

    let content_width = prepared.iter().map(|(_, text, _)| width(text)).sum::<usize>();
    if content_width <= input.columns {
        let start_column = match input.alignment {
            TabBarAlignment::Left => 0,
            TabBarAlignment::Center => input.columns.saturating_sub(content_width) / 2,
            TabBarAlignment::Right => input.columns.saturating_sub(content_width),
        };
        push_tab_segments(
            &prepared,
            start_column,
            input.columns,
            &mut segments,
            &mut hit_regions,
            &mut close_regions,
        );
    } else {
        let active = prepared.iter().position(|(tab, ..)| tab.active).unwrap_or(0);
        let (first, last) = visible_overflow_range(&prepared, active, input.columns);
        let show_indicators = input.columns >= 3;
        let hidden_left = show_indicators && first > 0;
        let hidden_right = show_indicators && last < prepared.len();

        let mut column = 0;
        if hidden_left {
            push_overflow_indicator("…", column, &mut segments);
            column += 1;
        }
        push_tab_segments(
            &prepared[first..last],
            column,
            input.columns.saturating_sub(usize::from(hidden_right)),
            &mut segments,
            &mut hit_regions,
            &mut close_regions,
        );
        column = segments.iter().map(|segment| segment.end_column).max().unwrap_or(column);
        if hidden_right && column < input.columns {
            push_overflow_indicator("…", input.columns - 1, &mut segments);
        }
    }

    TabBarLayout {
        visible,
        position: input.position,
        reserved_rows,
        row,
        full_row_background: row.map(|row| TabBarRect {
            row,
            start_column: 0,
            end_column: input.columns,
        }),
        segments,
        hit_regions,
        close_regions,
    }
}

impl From<crate::config::window::TabBarVisibility> for TabBarVisibility {
    fn from(visibility: crate::config::window::TabBarVisibility) -> Self {
        match visibility {
            crate::config::window::TabBarVisibility::Auto => Self::Auto,
            crate::config::window::TabBarVisibility::Always => Self::Always,
            crate::config::window::TabBarVisibility::Never => Self::Never,
        }
    }
}

impl From<crate::config::window::TabBarPosition> for TabBarPosition {
    fn from(position: crate::config::window::TabBarPosition) -> Self {
        match position {
            crate::config::window::TabBarPosition::Bottom => Self::Bottom,
        }
    }
}

impl From<crate::config::window::TabBarAlignment> for TabBarAlignment {
    fn from(alignment: crate::config::window::TabBarAlignment) -> Self {
        match alignment {
            crate::config::window::TabBarAlignment::Left => Self::Left,
            crate::config::window::TabBarAlignment::Center => Self::Center,
            crate::config::window::TabBarAlignment::Right => Self::Right,
        }
    }
}

impl From<crate::config::window::TabBarCloseButton> for TabBarCloseButtonVisibility {
    fn from(close_button: crate::config::window::TabBarCloseButton) -> Self {
        match close_button {
            crate::config::window::TabBarCloseButton::Always => Self::Always,
            crate::config::window::TabBarCloseButton::Hover => Self::Hover,
            crate::config::window::TabBarCloseButton::Never => Self::Never,
        }
    }
}

fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

type PreparedTab<'a> = (&'a TabBarEntry, String, Option<usize>);

fn visible_overflow_range(
    tabs: &[PreparedTab<'_>],
    active: usize,
    columns: usize,
) -> (usize, usize) {
    let mut first = active;
    let mut last = active + 1;

    loop {
        let mut changed = false;
        if last < tabs.len() && overflow_range_width(tabs, first, last + 1, columns) <= columns {
            last += 1;
            changed = true;
        }
        if first > 0 && overflow_range_width(tabs, first - 1, last, columns) <= columns {
            first -= 1;
            changed = true;
        }

        if !changed {
            break;
        }
    }

    (first, last)
}

fn overflow_range_width(
    tabs: &[PreparedTab<'_>],
    first: usize,
    last: usize,
    columns: usize,
) -> usize {
    let indicator_width =
        if columns >= 3 { usize::from(first > 0) + usize::from(last < tabs.len()) } else { 0 };
    tabs[first..last].iter().map(|(_, text, _)| width(text)).sum::<usize>() + indicator_width
}

fn push_tab_segments(
    tabs: &[PreparedTab<'_>],
    start_column: usize,
    end_column_limit: usize,
    segments: &mut Vec<TabBarSegment>,
    hit_regions: &mut Vec<(TabId, usize, usize)>,
    close_regions: &mut Vec<(TabId, usize, usize)>,
) {
    let mut column = start_column;
    for (tab, text, close_start) in tabs {
        let start = column;
        let available = end_column_limit.saturating_sub(start);
        if available == 0 {
            break;
        }

        let text = clip_text(text, available);
        let end = start + text.width();
        if end == start {
            break;
        }

        hit_regions.push((tab.id, start, end));
        segments.push(TabBarSegment {
            text,
            active: tab.active,
            tab_id: Some(tab.id),
            start_column: start,
            end_column: end,
            is_close_button: false,
        });
        if let Some(close_start) = close_start.map(|close_start| start + close_start)
            && close_start < end
        {
            close_regions.push((tab.id, close_start, close_start + 1));
            segments.push(TabBarSegment {
                text: "×".into(),
                active: tab.active,
                tab_id: Some(tab.id),
                start_column: close_start,
                end_column: close_start + 1,
                is_close_button: true,
            });
        }

        column = end;
        if column >= end_column_limit {
            break;
        }
    }
}

fn push_overflow_indicator(text: &str, column: usize, segments: &mut Vec<TabBarSegment>) {
    segments.push(TabBarSegment {
        text: text.into(),
        active: false,
        tab_id: None,
        start_column: column,
        end_column: column + 1,
        is_close_button: false,
    });
}

fn prepare_tab_segment<'a>(
    tab: &'a TabBarEntry,
    label: &str,
    max_width: Option<usize>,
    min_width: usize,
    close_button_visibility: TabBarCloseButtonVisibility,
    hovered_tab: Option<TabId>,
) -> (&'a TabBarEntry, String, Option<usize>) {
    let max_width = max_width.unwrap_or(usize::MAX);
    let min_width = min_width.min(max_width);
    let reserve_close_button = matches!(
        close_button_visibility,
        TabBarCloseButtonVisibility::Always | TabBarCloseButtonVisibility::Hover
    );
    let render_close_button =
        matches!(close_button_visibility, TabBarCloseButtonVisibility::Always)
            || matches!(close_button_visibility, TabBarCloseButtonVisibility::Hover)
                && hovered_tab == Some(tab.id);

    let close_width = if reserve_close_button { 2 } else { 0 };
    let label_width = max_width.saturating_sub(2 + close_width);
    let label = clip_label(label, label_width);
    let mut text = String::new();

    push_space(&mut text, max_width);
    text.push_str(&clip_text(&label, max_width.saturating_sub(width(&text))));

    let mut close_start = None;
    if reserve_close_button {
        push_space(&mut text, max_width);
        if width(&text) < max_width {
            close_start = render_close_button.then_some(width(&text));
            text.push(if render_close_button { '×' } else { ' ' });
        }
    }

    push_space(&mut text, max_width);
    while width(&text) < min_width {
        text.push(' ');
    }

    (tab, text, close_start)
}

fn push_space(text: &mut String, max_width: usize) {
    if width(text) < max_width {
        text.push(' ');
    }
}

fn clip_label(text: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut result = String::new();
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        width += ch_width;
        result.push(ch);
    }
    if result.is_empty() && !text.is_empty() {
        result.push('…');
    }
    result
}

fn clip_text(text: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut result = String::new();
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        width += ch_width;
        result.push(ch);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tabs(labels: &[(&str, bool)]) -> Vec<TabBarEntry> {
        labels
            .iter()
            .enumerate()
            .map(|(i, (label, active))| TabBarEntry {
                id: TabId::new(i as u64),
                index: i,
                label: (*label).into(),
                active: *active,
            })
            .collect()
    }

    #[test]
    fn auto_visibility_depends_on_tab_count() {
        let layout = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[("one", true)]),
            columns: 20,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Auto,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Left,
            close_button_visibility: TabBarCloseButtonVisibility::Never,
            hovered_tab: None,
            show_indices: false,
            max_width: None,
            min_width: 0,
        });
        assert!(!layout.visible);

        let layout = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[("one", true), ("two", false)]),
            columns: 20,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Auto,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Left,
            close_button_visibility: TabBarCloseButtonVisibility::Never,
            hovered_tab: None,
            show_indices: false,
            max_width: None,
            min_width: 0,
        });
        assert!(layout.visible);
    }

    #[test]
    fn always_and_never_visibility() {
        assert!(
            layout_tab_bar(TabBarLayoutInput {
                tabs: &tabs(&[("one", true)]),
                columns: 20,
                screen_lines: 10,
                search_lines: 0,
                message_lines: 0,
                visibility: TabBarVisibility::Always,
                position: TabBarPosition::Bottom,
                alignment: TabBarAlignment::Left,
                close_button_visibility: TabBarCloseButtonVisibility::Never,
                hovered_tab: None,
                show_indices: false,
                max_width: None,
                min_width: 0
            })
            .visible
        );
        assert!(
            !layout_tab_bar(TabBarLayoutInput {
                tabs: &tabs(&[("one", true), ("two", false)]),
                columns: 20,
                screen_lines: 10,
                search_lines: 0,
                message_lines: 0,
                visibility: TabBarVisibility::Never,
                position: TabBarPosition::Bottom,
                alignment: TabBarAlignment::Left,
                close_button_visibility: TabBarCloseButtonVisibility::Never,
                hovered_tab: None,
                show_indices: false,
                max_width: None,
                min_width: 0
            })
            .visible
        );
    }

    #[test]
    fn bottom_row_accounts_for_search_and_message_lines() {
        let layout = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[("one", true), ("two", false)]),
            columns: 40,
            screen_lines: 24,
            search_lines: 1,
            message_lines: 2,
            visibility: TabBarVisibility::Always,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Left,
            close_button_visibility: TabBarCloseButtonVisibility::Never,
            hovered_tab: None,
            show_indices: false,
            max_width: None,
            min_width: 0,
        });
        assert_eq!(layout.row, Some(27));
        assert_eq!(layout.full_row_background.unwrap().row, 27);
    }

    #[test]
    fn min_and_max_width_clip_labels() {
        let layout = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[("abcdef", true), ("ghijkl", false)]),
            columns: 20,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Always,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Left,
            close_button_visibility: TabBarCloseButtonVisibility::Never,
            hovered_tab: None,
            show_indices: false,
            max_width: Some(3),
            min_width: 2,
        });
        assert!(layout.segments.iter().all(|segment| segment.text.width() <= 3));
        assert!(layout.segments.iter().all(|segment| segment.text.width() >= 2));
    }

    #[test]
    fn narrow_terminal_still_produces_layout() {
        let layout = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[("one", true), ("two", false)]),
            columns: 1,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Always,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Left,
            close_button_visibility: TabBarCloseButtonVisibility::Never,
            hovered_tab: None,
            show_indices: false,
            max_width: None,
            min_width: 0,
        });
        assert!(layout.visible);
        assert_eq!(layout.full_row_background.unwrap().end_column, 1);
        assert!(layout.segments.iter().all(|segment| segment.end_column <= 1));
    }

    #[test]
    fn close_button_stays_within_visible_columns() {
        let layout = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[("long-title", true)]),
            columns: 1,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Always,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Left,
            close_button_visibility: TabBarCloseButtonVisibility::Always,
            hovered_tab: None,
            show_indices: false,
            max_width: None,
            min_width: 0,
        });

        assert!(layout.segments.iter().all(|segment| segment.end_column <= 1));
        assert!(!layout.segments.iter().any(|segment| segment.is_close_button));
    }

    #[test]
    fn unicode_labels_preserve_width() {
        let layout = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[("界面", true), ("wide🙂", false)]),
            columns: 20,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Always,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Left,
            close_button_visibility: TabBarCloseButtonVisibility::Never,
            hovered_tab: None,
            show_indices: false,
            max_width: Some(4),
            min_width: 0,
        });
        assert!(layout.segments[0].text.width() <= 4);
    }

    #[test]
    fn close_and_label_hit_regions_exist() {
        let layout = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[("one", true), ("two", false)]),
            columns: 40,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Always,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Left,
            close_button_visibility: TabBarCloseButtonVisibility::Always,
            hovered_tab: None,
            show_indices: false,
            max_width: None,
            min_width: 0,
        });

        let hovered = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[("one", true), ("two", false)]),
            columns: 40,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Always,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Left,
            close_button_visibility: TabBarCloseButtonVisibility::Hover,
            hovered_tab: Some(TabId::new(1)),
            show_indices: false,
            max_width: None,
            min_width: 0,
        });
        assert!(hovered.segments.iter().any(|segment| segment.is_close_button));
        assert_eq!(hovered.close_regions.len(), 1);
        assert_eq!(layout.hit_regions.len(), 2);
        assert_eq!(layout.close_regions.len(), 2);
        assert!(layout.segments.iter().any(|segment| segment.is_close_button));
    }

    #[test]
    fn hover_close_button_does_not_shift_layout() {
        let unhovered = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[("one", true), ("two", false)]),
            columns: 40,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Always,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Center,
            close_button_visibility: TabBarCloseButtonVisibility::Hover,
            hovered_tab: None,
            show_indices: false,
            max_width: None,
            min_width: 0,
        });
        let hovered = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[("one", true), ("two", false)]),
            columns: 40,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Always,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Center,
            close_button_visibility: TabBarCloseButtonVisibility::Hover,
            hovered_tab: Some(TabId::new(1)),
            show_indices: false,
            max_width: None,
            min_width: 0,
        });

        assert_eq!(unhovered.hit_regions, hovered.hit_regions);
    }

    #[test]
    fn alignment_moves_start_column() {
        let left = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[("one", true)]),
            columns: 30,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Always,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Left,
            close_button_visibility: TabBarCloseButtonVisibility::Never,
            hovered_tab: None,
            show_indices: false,
            max_width: None,
            min_width: 0,
        });
        let center = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[("one", true)]),
            columns: 30,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Always,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Center,
            close_button_visibility: TabBarCloseButtonVisibility::Never,
            hovered_tab: None,
            show_indices: false,
            max_width: None,
            min_width: 0,
        });
        let right = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[("one", true)]),
            columns: 30,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Always,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Right,
            close_button_visibility: TabBarCloseButtonVisibility::Never,
            hovered_tab: None,
            show_indices: false,
            max_width: None,
            min_width: 0,
        });
        assert!(left.segments[0].start_column < center.segments[0].start_column);
        assert!(center.segments[0].start_column < right.segments[0].start_column);
    }

    #[test]
    fn overflow_keeps_active_tab_visible() {
        let layout = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[
                ("one", false),
                ("two", false),
                ("three", true),
                ("four", false),
                ("five", false),
                ("six", false),
            ]),
            columns: 12,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Always,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Left,
            close_button_visibility: TabBarCloseButtonVisibility::Never,
            hovered_tab: None,
            show_indices: false,
            max_width: Some(5),
            min_width: 0,
        });

        assert!(layout.hit_regions.iter().any(|(id, ..)| *id == TabId::new(2)));
        assert!(!layout.hit_regions.iter().any(|(id, ..)| *id == TabId::new(0)));
        assert!(!layout.hit_regions.iter().any(|(id, ..)| *id == TabId::new(5)));
        assert!(layout.segments.iter().all(|segment| segment.end_column <= 12));
    }

    #[test]
    fn overflow_marks_hidden_tabs() {
        let layout = layout_tab_bar(TabBarLayoutInput {
            tabs: &tabs(&[
                ("one", false),
                ("two", false),
                ("three", true),
                ("four", false),
                ("five", false),
                ("six", false),
            ]),
            columns: 12,
            screen_lines: 10,
            search_lines: 0,
            message_lines: 0,
            visibility: TabBarVisibility::Always,
            position: TabBarPosition::Bottom,
            alignment: TabBarAlignment::Left,
            close_button_visibility: TabBarCloseButtonVisibility::Never,
            hovered_tab: None,
            show_indices: false,
            max_width: Some(5),
            min_width: 0,
        });

        let overflow_columns = layout
            .segments
            .iter()
            .filter(|segment| segment.tab_id.is_none() && segment.text == "…")
            .map(|segment| segment.start_column)
            .collect::<Vec<_>>();

        assert_eq!(overflow_columns, vec![0, 11]);
    }
}
