// Copyright 2026 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::borrow::Cow;
use std::iter;
use std::sync::Arc;

use bstr::BStr;
use bstr::BString;
use bstr::ByteSlice;
use bstr::ByteVec;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use itertools::Itertools;
use jj_lib::backend::BackendResult;
use jj_lib::backend::CopyId;
use jj_lib::backend::TreeValue;
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::conflicts::MaterializedFileConflictValue;
use jj_lib::diff_presentation::DiffTokenType;
use jj_lib::diff_presentation::LineCompareMode;
use jj_lib::diff_presentation::unified::DiffLineType;
use jj_lib::diff_presentation::unified::unified_diff_hunks;
use jj_lib::files;
use jj_lib::files::FromMergeHunks;
use jj_lib::files::MergeHunk;
use jj_lib::files::MergeResult;
use jj_lib::merge::Diff;
use jj_lib::merge::Merge;
use jj_lib::merge::MergedTreeValue;
use jj_lib::store::Store;
use pollster::FutureExt;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Block;
use ratatui::widgets::BorderType;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::ScrollDirection;
use ratatui::widgets::Widget;

use crate::merge_tools::builtin_select::Conflict;
use crate::merge_tools::builtin_select::SelectToolResult;
use crate::merge_tools::builtin_select::cow_bstr_to_str_lossy;
use crate::tui_util;
use crate::tui_util::ScrollableBlock;
use crate::tui_util::ScrollableItem;

pub struct HunkConflictResolution {
    edited_contents: Option<Merge<BString>>,
    selections: Vec<Option<HunkSelection>>,
}

pub struct HunkConflictViewerState {
    store: Arc<Store>,
    file_conflict: MaterializedFileConflictValue,
    edited_contents: Option<Merge<BString>>,
    hunks: Vec<ConflictHunk>,
    hunk_index: usize,
    context_after: BString,
    current_selection: HunkSelection,
    scroll_offset: usize,
    window_height: u16,
    show_diff: bool,
}

impl HunkConflictViewerState {
    pub fn new(
        store: Arc<Store>,
        file_conflict: MaterializedFileConflictValue,
        resolution: Option<&HunkConflictResolution>,
    ) -> Self {
        let edited_contents = resolution.and_then(|resolution| resolution.edited_contents.clone());
        let contents = edited_contents.as_ref().unwrap_or(&file_conflict.contents);

        let (mut hunks, context_after) = match files::merge_hunks(contents, store.merge_options()) {
            MergeResult::Resolved(contents) => (Vec::new(), contents),
            MergeResult::Conflict(hunks) => {
                let mut current_resolved = BString::default();
                let mut conflict_hunks = Vec::new();
                for hunk in hunks {
                    if let Some(resolved) = hunk.as_resolved() {
                        current_resolved.push_str(resolved);
                    } else {
                        conflict_hunks.push(ConflictHunk {
                            context_before: current_resolved,
                            contents: hunk,
                            selection: None,
                        });
                        current_resolved = BString::default();
                    }
                }
                (conflict_hunks, current_resolved)
            }
        };
        if let Some(resolution) = resolution
            && !resolution.selections.is_empty()
        {
            for (hunk, selection) in hunks.iter_mut().zip_eq(&resolution.selections) {
                hunk.selection = *selection;
            }
        }
        let hunk_index = hunks
            .iter()
            .position(|hunk| !hunk.is_resolved())
            .unwrap_or(0);
        let current_selection = hunks
            .get(hunk_index)
            .and_then(|hunk| hunk.selection)
            .unwrap_or(HunkSelection::Added(0));
        Self {
            store,
            file_conflict,
            edited_contents,
            hunks,
            hunk_index,
            context_after,
            current_selection,
            scroll_offset: 0,
            window_height: 0,
            show_diff: false,
        }
    }

    pub fn handle_press(
        &mut self,
        event: KeyEvent,
        conflict: &Conflict,
    ) -> Option<SelectToolResult> {
        match (event.code, event.modifiers) {
            // Navigate between hunks with up/down
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => self.prev_hunk(),
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => self.next_hunk(),
            // Navigate between terms with left/right
            (KeyCode::Left | KeyCode::Char('h'), KeyModifiers::NONE) => self.prev_term(),
            (KeyCode::Right | KeyCode::Char('l'), KeyModifiers::NONE) => self.next_term(),
            // Toggle current hunk selected with enter or space
            (KeyCode::Enter, KeyModifiers::NONE) | (KeyCode::Char(' '), KeyModifiers::NONE) => {
                let new_selection = Some(self.current_selection);
                if let Some(current_hunk) = self.current_hunk_mut() {
                    if current_hunk.selection == new_selection {
                        current_hunk.selection = None;
                    } else {
                        current_hunk.selection = new_selection;
                    }
                }
            }
            // Use current selection for all remaining hunks with 'a'
            (KeyCode::Char('a'), KeyModifiers::NONE) => {
                for hunk in &mut self.hunks {
                    if hunk.selection.is_none() {
                        hunk.selection = Some(self.current_selection);
                    }
                }
            }
            (KeyCode::Char('b'), KeyModifiers::NONE) => self.toggle_base(),
            (KeyCode::Char('d'), KeyModifiers::NONE) => self.toggle_diff(),
            (KeyCode::Char('e'), KeyModifiers::NONE) => {
                return Some(SelectToolResult::EditConflict {
                    file_name: conflict.file_name().to_owned(),
                    contents: self
                        .get_merged_contents()
                        .unwrap_or_else(|| self.file_contents().clone()),
                    labels: self.labels().clone(),
                });
            }
            (KeyCode::Backspace | KeyCode::Delete, KeyModifiers::NONE) => {
                if let Some(hunk) = self.current_hunk_mut() {
                    hunk.selection = None;
                    self.scroll_to_current_hunk(false);
                }
            }
            _ => {}
        }
        None
    }

    pub fn scroll_by(&mut self, direction: ScrollDirection, amount: u16) {
        tui_util::scroll_offset_by(&mut self.scroll_offset, direction, amount);
    }

    pub fn set_contents(self, new_contents: Merge<BString>) -> Self {
        let resolution = HunkConflictResolution {
            edited_contents: (new_contents != self.file_conflict.contents).then_some(new_contents),
            selections: Vec::new(),
        };
        Self::new(self.store, self.file_conflict, Some(&resolution))
    }

    pub fn render(&mut self, frame: &mut ratatui::Frame, area: Rect, conflict: &Conflict) {
        let conflict_summary = {
            let resolved_count = self.hunks.iter().filter(|hunk| hunk.is_resolved()).count();
            let total_count = self.hunks.len();
            let color = if resolved_count == total_count {
                Color::Green
            } else if resolved_count == 0 && self.edited_contents.is_none() {
                Color::Red
            } else {
                Color::Yellow
            };

            if total_count == 0 {
                "(resolved by editor)".fg(color)
            } else {
                let edited = if self.edited_contents.is_some() {
                    ", edited"
                } else {
                    ""
                };
                format!("({resolved_count}/{total_count} conflicts resolved{edited})",).fg(color)
            }
        };

        let block = Block::bordered().title(
            Line::from(vec![
                " ".into(),
                conflict.file_name().into(),
                " ".into(),
                conflict_summary,
                " ".into(),
            ])
            .bold(),
        );

        let new_window_height = block.inner(area).height;
        if self.window_height != new_window_height {
            self.window_height = new_window_height;
            self.scroll_to_current_hunk(true);
        }

        let scrollable_items = self
            .hunks
            .iter()
            .enumerate()
            .flat_map(|(index, hunk)| hunk.scrollable_items(self, index))
            .chain(iter::once(context_hunk(self.context_after.to_str_lossy())));

        let scrollable_block = ScrollableBlock::new(scrollable_items).block(block);
        let mut scroll_offset = self.scroll_offset;
        frame.render_stateful_widget(scrollable_block, area, &mut scroll_offset);
        // We have to update this after rendering to prevent a lifetime issue
        self.scroll_offset = scroll_offset;
    }

    pub fn prev_hunk(&mut self) {
        // If no hunks are present (when resolved by editor), scroll instead
        if self.hunks.is_empty() {
            self.scroll_by(ScrollDirection::Backward, 1);
            return;
        }
        if self.hunk_index > 0 {
            self.hunk_index -= 1;
            if let Some(selection) = self.current_hunk().unwrap().selection {
                self.current_selection = selection;
            }
        }
        self.scroll_to_current_hunk(true);
    }

    pub fn next_hunk(&mut self) {
        // If no hunks are present (when resolved by editor), scroll instead
        if self.hunks.is_empty() {
            self.scroll_by(ScrollDirection::Forward, 1);
            return;
        }
        if self.hunk_index + 1 < self.hunks.len() {
            self.hunk_index += 1;
            if let Some(selection) = self.current_hunk().unwrap().selection {
                self.current_selection = selection;
            }
        }
        self.scroll_to_current_hunk(true);
    }

    pub fn prev_term(&mut self) {
        if let Some(current_hunk) = self.current_hunk() {
            if !current_hunk.is_resolved() {
                self.current_selection = self.current_selection.prev(self.num_sides());
            }
            self.scroll_to_current_hunk(false);
        }
    }

    pub fn next_term(&mut self) {
        if let Some(current_hunk) = self.current_hunk() {
            if !current_hunk.is_resolved() {
                self.current_selection = self.current_selection.next(self.num_sides());
            }
            self.scroll_to_current_hunk(false);
        }
    }

    pub fn toggle_base(&mut self) {
        if let Some(current_hunk) = self.current_hunk()
            && !current_hunk.is_resolved()
        {
            self.current_selection = self.current_selection.toggle_add_remove(self.num_sides());
            self.scroll_to_current_hunk(false);
        }
    }

    pub fn toggle_diff(&mut self) {
        if let Some(current_hunk) = self.current_hunk()
            && !current_hunk.is_resolved()
        {
            self.show_diff = !self.show_diff;
            self.scroll_to_current_hunk(false);
        }
    }

    pub fn scroll_to_current_hunk(&mut self, enforce_context: bool) {
        let Some(current_hunk) = self.current_hunk() else {
            return;
        };
        let start_offset: usize = self
            .hunks
            .iter()
            .take(self.hunk_index)
            .enumerate()
            .flat_map(|(index, hunk)| hunk.scrollable_items(self, index))
            .chain(iter::once(current_hunk.context_scrollable_item()))
            .map(|item| item.height())
            .sum();
        let end_offset = start_offset
            + current_hunk
                .hunk_scrollable_item(self, self.hunk_index)
                .height();

        let context = if enforce_context {
            let available_space =
                usize::from(self.window_height).saturating_sub(end_offset - start_offset);
            (available_space / 2).min(5)
        } else {
            0
        };

        let align_start_offset = start_offset.saturating_sub(context);
        let align_end_offset = (end_offset + context).saturating_sub(self.window_height.into());

        if align_start_offset < align_end_offset {
            self.scroll_offset = align_start_offset;
        } else {
            self.scroll_offset = self
                .scroll_offset
                .clamp(align_end_offset, align_start_offset);
        }
    }

    fn current_hunk(&self) -> Option<&ConflictHunk> {
        self.hunks.get(self.hunk_index)
    }

    fn current_hunk_mut(&mut self) -> Option<&mut ConflictHunk> {
        self.hunks.get_mut(self.hunk_index)
    }

    fn file_contents(&self) -> &Merge<BString> {
        self.edited_contents
            .as_ref()
            .unwrap_or(&self.file_conflict.contents)
    }

    fn num_sides(&self) -> usize {
        self.file_contents().num_sides()
    }

    fn labels(&self) -> &ConflictLabels {
        &self.file_conflict.labels
    }

    fn get_merged_contents(&self) -> Option<Merge<BString>> {
        if !self.hunks.iter().any(|hunk| hunk.is_resolved()) {
            return self.edited_contents.clone();
        };
        Some(FromMergeHunks::from_hunks(
            self.hunks
                .iter()
                .flat_map(|hunk| {
                    [
                        MergeHunk::resolved(hunk.context_before.as_bstr().into()),
                        hunk.resolved_contents().map_or_else(
                            || MergeHunk::Borrowed(hunk.contents.map(|term| term.as_bstr())),
                            MergeHunk::resolved,
                        ),
                    ]
                })
                .chain(iter::once(MergeHunk::resolved(
                    self.context_after.as_bstr().into(),
                ))),
        ))
    }

    pub fn confirm(
        &self,
        conflict: &Conflict,
    ) -> BackendResult<Option<(MergedTreeValue, HunkConflictResolution)>> {
        // If no changes were made, we shouldn't record any resolution.
        let Some(merged_contents) = self.get_merged_contents() else {
            return Ok(None);
        };
        let simplified_file_ids = merged_contents
            .try_map_async(async |term| {
                self.store
                    .write_file(&conflict.path, &mut &term[..])
                    .await
                    .map(Some)
            })
            .block_on()?;
        let new_file_ids = if simplified_file_ids.is_resolved()
            || simplified_file_ids.num_sides() == self.file_conflict.unsimplified_ids.num_sides()
        {
            simplified_file_ids
        } else {
            self.file_conflict
                .unsimplified_ids
                .clone()
                .update_from_simplified(simplified_file_ids)
        };
        // Since deletions are resolved using the other view, we will never have an
        // executable bit conflict.
        let executable = self.file_conflict.executable.unwrap_or(false);
        // TODO: if the conflict is only partially resolved, we may want to preserve the
        // executable bit from the original terms.
        let resolved_value = new_file_ids.map(|id| {
            id.as_ref().map(|id| {
                TreeValue::File {
                    id: id.clone(),
                    executable,
                    // TODO: allow selecting copy ID
                    copy_id: self
                        .file_conflict
                        .copy_id
                        .clone()
                        .unwrap_or_else(CopyId::placeholder),
                }
            })
        });
        Ok(Some((
            resolved_value,
            HunkConflictResolution {
                edited_contents: self.edited_contents.clone(),
                selections: self.hunks.iter().map(|hunk| hunk.selection).collect_vec(),
            },
        )))
    }
}

struct ConflictHunk {
    context_before: BString,
    contents: Merge<BString>,
    selection: Option<HunkSelection>,
}

impl ConflictHunk {
    fn resolved_contents(&self) -> Option<Cow<'_, BStr>> {
        if let Some(selection) = self.selection {
            Some(selection.select_from(&self.contents))
        } else {
            None
        }
    }

    fn is_resolved(&self) -> bool {
        self.selection.is_some()
    }

    fn context_scrollable_item(&self) -> ScrollableItem<'_> {
        context_hunk(self.context_before.to_str_lossy())
    }

    fn hunk_scrollable_item<'a>(
        &'a self,
        state: &'a HunkConflictViewerState,
        index: usize,
    ) -> ScrollableItem<'a> {
        let num_hunks = state.hunks.len();
        let current_hunk_selection = (state.hunk_index == index).then_some(state.current_selection);

        // TODO: trailing newline handling
        if let Some(selected) = self.resolved_contents() {
            let text = content_to_text(selected);
            ScrollableItem::from_widget(text.height() + 2, move || {
                let style = if current_hunk_selection.is_some() {
                    Style::new().blue()
                } else {
                    Style::new()
                };
                let block = Block::new()
                    .borders(Borders::TOP | Borders::BOTTOM)
                    .border_style(Style::new().dim())
                    .border_type(BorderType::LightDoubleDashed);
                Paragraph::new(text).block(block).style(style)
            })
        } else if let Some(selection) = current_hunk_selection {
            let diff = selection.to_diff().filter(|_| state.show_diff);
            let (text, diff_base_height) = if let Some(diff) = diff {
                let removed = self.contents.get_remove(diff.before).unwrap().as_bstr();
                let added = self.contents.get_add(diff.after).unwrap().as_bstr();
                (diff_to_text(removed, added), 1)
            } else {
                (content_to_text(selection.select_from(&self.contents)), 0)
            };
            let height = text.height() + usize::from(diff_base_height) + 5;
            ScrollableItem::from_render(height, move |area, buf| {
                let block = Block::new()
                    .borders(Borders::TOP | Borders::BOTTOM)
                    .border_type(BorderType::Thick)
                    .style(Style::new().blue());

                let [header_area, body_area] = Layout::default()
                    .constraints([Constraint::Length(diff_base_height + 2), Constraint::Min(1)])
                    .areas(block.inner(area));

                block.render(area, buf);

                let [header_top_area, mut header_bottom_area] = Layout::default()
                    .constraints([Constraint::Length(1), Constraint::Min(1)])
                    .areas(header_area);

                let header_top_text = format!("🞃 Conflict {}/{}", index + 1, num_hunks);

                let [left_area, right_area] = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Length(tui_util::to_u16(header_top_text.len())),
                        Constraint::Min(10),
                    ])
                    .areas(header_top_area);

                header_top_text.bold().render(left_area, buf);

                let selections = selection.list_reachable(self.contents.num_sides());
                tui_util::render_horizontal_list(
                    selections,
                    &selection,
                    HunkSelection::position_str,
                    right_area,
                    buf,
                );

                let label = selection.get_label(state.labels());
                let conflict_label_text = if let Some(diff) = diff {
                    let base_label = state.labels().get_remove_or_default(diff.before);
                    Text::from(vec![
                        Line::from(vec![Span::from("diff from: "), base_label.into()]),
                        Line::from(vec![Span::from("       to: "), label.into()]),
                    ])
                } else {
                    label.into()
                };

                // Don't use the first 2 columns since we want the text to align.
                header_bottom_area.x += 2;
                header_bottom_area.width = header_bottom_area.width.saturating_sub(2);
                conflict_label_text.render(header_bottom_area, buf);

                let body = Paragraph::new(text).block(Block::bordered().borders(Borders::TOP));
                body.render(body_area, buf);
            })
        } else {
            ScrollableItem::from_widget(3, move || {
                Paragraph::new(format!("🞂 Conflict {}/{}", index + 1, num_hunks).bold())
                    .block(
                        Block::bordered()
                            .borders(Borders::TOP | Borders::BOTTOM)
                            .border_type(BorderType::Thick),
                    )
                    .style(Style::new().red())
            })
        }
    }

    fn scrollable_items<'a>(
        &'a self,
        state: &'a HunkConflictViewerState,
        index: usize,
    ) -> impl IntoIterator<Item = ScrollableItem<'a>> {
        let context_item = self.context_scrollable_item();
        let hunk_item = self.hunk_scrollable_item(state, index);
        [context_item, hunk_item]
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HunkSelection {
    Added(usize),
    Removed(usize),
    AllAdded,
}

impl HunkSelection {
    pub fn select_from<'a>(&self, contents: &'a Merge<BString>) -> Cow<'a, BStr> {
        match self {
            Self::Added(index) => Cow::Borrowed(contents.get_add(*index).unwrap().as_bstr()),
            Self::Removed(index) => Cow::Borrowed(contents.get_remove(*index).unwrap().as_bstr()),
            Self::AllAdded => Cow::Owned(
                contents
                    .as_ref()
                    .simplify()
                    .adds()
                    .map(|&hunk| {
                        let mut hunk = Cow::from(hunk);
                        if hunk.last().is_some_and(|&ch| ch != b'\n') {
                            // TODO: use appropriate line endings
                            hunk.to_mut().push(b'\n');
                        }
                        hunk
                    })
                    .unique()
                    .join("")
                    .into(),
            ),
        }
    }

    pub fn get_label<'a>(&self, labels: &'a ConflictLabels) -> Cow<'a, str> {
        match self {
            Self::Added(index) => labels.get_add_or_default(*index),
            Self::Removed(index) => labels.get_remove_or_default(*index),
            Self::AllAdded => "(combine all sides)".into(),
        }
    }

    pub fn prev(&self, num_sides: usize) -> Self {
        match *self {
            Self::Added(n) => Self::Added(n.saturating_sub(1)),
            Self::Removed(n) => Self::Removed(n.saturating_sub(1)),
            Self::AllAdded => Self::Added(num_sides - 1),
        }
    }

    pub fn next(&self, num_sides: usize) -> Self {
        match *self {
            Self::Added(n) if n < num_sides - 1 => Self::Added(n + 1),
            Self::Added(_) => Self::AllAdded,
            Self::Removed(n) if n < num_sides - 2 => Self::Removed(n + 1),
            _ => *self,
        }
    }

    pub fn toggle_add_remove(&self, num_sides: usize) -> Self {
        match self {
            Self::Added(n) => Self::Removed(n.saturating_sub(1)),
            Self::Removed(n) => Self::Added(n + 1),
            Self::AllAdded => Self::Removed(num_sides - 2),
        }
    }

    pub fn to_diff<'a>(&self) -> Option<Diff<usize>> {
        let &Self::Added(add_index) = self else {
            return None;
        };
        let remove_index = add_index.saturating_sub(1);
        Some(Diff::new(remove_index, add_index))
    }

    pub fn list_reachable(&self, num_sides: usize) -> Vec<Self> {
        match self {
            Self::Added(_) | Self::AllAdded => (0..num_sides)
                .map(Self::Added)
                .chain(iter::once(Self::AllAdded))
                .collect_vec(),
            Self::Removed(_) => (0..num_sides - 1).map(Self::Removed).collect_vec(),
        }
    }

    pub fn position_str(&self) -> String {
        match self {
            Self::Added(n) => (n + 1).to_string(),
            Self::Removed(n) => format!("b{}", n + 1),
            Self::AllAdded => "all".to_owned(),
        }
    }
}

fn context_hunk(hunk: Cow<'_, str>) -> ScrollableItem<'_> {
    text_hunk_or_empty(hunk, Style::new())
}

fn text_hunk_or_empty(hunk: Cow<'_, str>, style: Style) -> ScrollableItem<'_> {
    if hunk.is_empty() {
        ScrollableItem::empty()
    } else {
        ScrollableItem::from_text(Text::raw(hunk).style(style))
    }
}

fn content_to_text(content: Cow<'_, BStr>) -> Text<'_> {
    if content.is_empty() {
        "(deleted)".dim().into()
    } else {
        Text::raw(cow_bstr_to_str_lossy(content))
    }
}

fn diff_to_text<'a>(removed: &'a BStr, added: &'a BStr) -> Text<'a> {
    let diff = unified_diff_hunks(
        Diff::new(removed, added),
        usize::MAX,
        LineCompareMode::Exact,
    );

    if diff
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .all(|&(diff_type, _)| diff_type == DiffLineType::Context)
    {
        return "(no changes)".dim().fg(Color::Reset).into();
    }

    let mut text = Text::default();
    for hunk in diff {
        for (diff_type, tokens) in hunk.lines {
            let (sigil, style) = match diff_type {
                DiffLineType::Added => ("+", Style::new().green()),
                DiffLineType::Removed => ("-", Style::new().red()),
                DiffLineType::Context => (" ", Style::new().fg(Color::Reset)),
            };
            let mut line = Line::default().style(style);
            line.push_span(sigil);
            for (token_type, bytes) in tokens {
                // TODO: use colors configuration to allow customization
                let style = match token_type {
                    DiffTokenType::Different => Style::new().underlined(),
                    DiffTokenType::Matching => Style::new(),
                };
                line.push_span(Span::styled(bytes.to_str_lossy(), style));
            }
            text.push_line(line);
        }
    }
    text
}
