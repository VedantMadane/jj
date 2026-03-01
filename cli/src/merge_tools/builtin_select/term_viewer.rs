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

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use itertools::Itertools;
use jj_lib::backend::BackendResult;
use jj_lib::backend::TreeValue;
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::conflicts::get_file_contents;
use jj_lib::merge::Merge;
use jj_lib::merge::MergedTreeValue;
use jj_lib::merged_tree::MergedTree;
use pollster::FutureExt;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Text;
use ratatui::widgets::Block;
use ratatui::widgets::BorderType;
use ratatui::widgets::ScrollDirection;

use crate::merge_tools::builtin_select::Conflict;
use crate::merge_tools::builtin_select::bstring_to_string_lossy;
use crate::tui_util;
use crate::tui_util::ScrollableBlock;
use crate::tui_util::render_horizontal_list;

#[derive(Copy, Clone, Default)]
pub struct TermConflictResolution {
    add_or_remove_index: usize,
    show_bases: bool,
}

impl TermConflictResolution {
    fn next(&mut self, num_sides: usize) {
        if self.show_bases {
            self.add_or_remove_index = (self.add_or_remove_index + 1).min(num_sides - 2);
        } else {
            self.add_or_remove_index = (self.add_or_remove_index + 1).min(num_sides - 1);
        }
    }

    fn prev(&mut self) {
        self.add_or_remove_index = self.add_or_remove_index.saturating_sub(1);
    }

    fn toggle_base(&mut self) {
        if self.show_bases {
            self.add_or_remove_index += 1;
        } else {
            self.add_or_remove_index = self.add_or_remove_index.saturating_sub(1);
        }
        self.show_bases = !self.show_bases;
    }
}

pub struct TermConflictViewerState {
    simplified_conflict: MergedTreeValue,
    simplified_labels: ConflictLabels,
    contents: Merge<String>,
    current_resolution: TermConflictResolution,
    scroll_offset: usize,
}

impl TermConflictViewerState {
    pub fn new(
        tree: &MergedTree,
        conflict: &Conflict,
        resolution: Option<&TermConflictResolution>,
    ) -> BackendResult<Self> {
        let store = tree.store();
        let (simplified_labels, simplified_conflict) =
            tree.labels().simplify_with(&conflict.unsimplified_conflict);
        let contents = simplified_conflict
            .try_map_async(async |value| match value {
                None => Ok(String::new()),
                Some(TreeValue::File { id, .. }) => {
                    let contents = get_file_contents(store, &conflict.path, Some(id)).await?;
                    Ok(bstring_to_string_lossy(contents))
                }
                Some(TreeValue::Symlink(id)) => store.read_symlink(&conflict.path, id).await,
                Some(TreeValue::Tree(id)) => {
                    let tree = store.get_tree_async(conflict.path.clone(), id).await?;
                    Ok(tree
                        .data()
                        .names()
                        .map(|name| format!("• {}\n", name.as_internal_str()))
                        .join("\n"))
                }
                Some(TreeValue::GitSubmodule(id)) => Ok(id.to_string()),
            })
            .block_on()?;
        Ok(Self {
            simplified_conflict,
            simplified_labels,
            contents,
            current_resolution: resolution.copied().unwrap_or_default(),
            scroll_offset: 0,
        })
    }

    pub fn handle_press(&mut self, event: KeyEvent) {
        match (event.code, event.modifiers) {
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.scroll_by(ScrollDirection::Backward, 1);
            }
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => {
                self.scroll_by(ScrollDirection::Forward, 1);
            }
            (KeyCode::Left | KeyCode::Char('h'), KeyModifiers::NONE) => {
                self.current_resolution.prev();
            }
            (KeyCode::Right | KeyCode::Char('l'), KeyModifiers::NONE) => {
                self.current_resolution.next(self.contents.num_sides());
            }
            (KeyCode::Char('b'), KeyModifiers::NONE) => {
                self.current_resolution.toggle_base();
            }
            _ => {}
        }
    }

    pub fn scroll_by(&mut self, direction: ScrollDirection, amount: u16) {
        tui_util::scroll_offset_by(&mut self.scroll_offset, direction, amount);
    }

    pub fn render(&mut self, frame: &mut ratatui::Frame, area: Rect, conflict: &Conflict) {
        let [header_area, content_area] = Layout::default()
            .constraints([Constraint::Length(4), Constraint::Fill(1)])
            .areas(area);

        let header_block = Block::bordered().border_type(BorderType::Thick);
        let header_inner_area = header_block.inner(header_area);
        frame.render_widget(header_block, header_area);

        let num_sides = self.contents.num_sides();
        if self.current_resolution.show_bases {
            render_horizontal_list(
                (0..num_sides - 1).collect_vec(),
                &self.current_resolution.add_or_remove_index,
                |index| format!("b{}", index + 1),
                header_inner_area,
                frame.buffer_mut(),
            );
        } else {
            render_horizontal_list(
                (0..num_sides).collect_vec(),
                &self.current_resolution.add_or_remove_index,
                |index| (index + 1).to_string(),
                header_inner_area,
                frame.buffer_mut(),
            );
        }

        let term = self.current_term();
        let label = self
            .simplified_labels
            .get_or_default(self.current_term_index());

        let header_text = Text::from(vec![
            conflict.file_name().bold().into(),
            label_with_file_type(label, term),
        ]);
        frame.render_widget(header_text, header_inner_area);

        let content = self
            .contents
            .get(self.current_term_index())
            .unwrap()
            .as_str();
        let bottom_text = match term {
            None => "(deleted)".dim().into(),
            Some(_) if content.is_empty() => "(empty)".dim().into(),
            Some(TreeValue::File { .. }) => content.into(),
            Some(_) => Text::from(content).dim(),
        };
        let scrollable_block = ScrollableBlock::from_text(bottom_text).block(Block::bordered());

        frame.render_stateful_widget(scrollable_block, content_area, &mut self.scroll_offset);
    }

    fn current_term_index(&self) -> usize {
        let index = self.current_resolution.add_or_remove_index;
        if self.current_resolution.show_bases {
            2 * index + 1
        } else {
            2 * index
        }
    }

    fn current_term(&self) -> &Option<TreeValue> {
        &self
            .simplified_conflict
            .get(self.current_term_index())
            .unwrap()
    }

    pub fn confirm(&self) -> (MergedTreeValue, TermConflictResolution) {
        (
            Merge::resolved(self.current_term().clone()),
            self.current_resolution,
        )
    }
}

pub fn label_with_file_type<'a>(label: Cow<'a, str>, term: &Option<TreeValue>) -> Line<'a> {
    let file_type = match term {
        None => "[deleted]".red(),
        Some(TreeValue::File {
            executable: false, ..
        }) => return label.into(),
        Some(TreeValue::File {
            executable: true, ..
        }) => "[executable]".yellow(),
        Some(TreeValue::Symlink(_)) => "[symlink]".yellow(),
        Some(TreeValue::Tree(_)) => "[directory]".yellow(),
        Some(TreeValue::GitSubmodule(_)) => "[git submodule]".yellow(),
    };
    Line::from(vec![label.into(), " ".into(), file_type])
}
