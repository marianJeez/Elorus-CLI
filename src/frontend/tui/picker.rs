//! A table to pick one row of, in a card in place of the editor: `/models` picks the model or
//! preset for the rest of the session (`/presets` the presets alone, with their rules), `/resume`
//! a session to go on with. ↑↓ move, → or Enter picks, Esc or ← closes; typing narrows the list.

use std::path::{Path, PathBuf};

use crate::frontend::tui::boxed::{self, boxed};
use crate::frontend::tui::card::{Card, Next};
use crate::frontend::tui::keymap::Action;
use crate::frontend::tui::rows;
use crate::frontend::tui::ui;
use crate::io::term::{KeyCode, KeyEvent, KeyModifiers};
use crate::models::Catalog;
use crate::models::preset::Preset;
use crate::models::preset_table as presets;
use crate::models::search;
use crate::models::table as models;
use crate::session::list::Summary;
use crate::session::table as sessions;
use crate::text::styled::{Line, Style};
use crate::text::table::{self, Align, GAP};
use crate::text::template::fill;

pub struct Picker {
    /// What each row picks and how it is drawn, in row order.
    entries: Vec<Entry>,
    /// The table's column titles, over the rows.
    header: Vec<Line>,
    aligns: Vec<Align>,
    drop: Vec<usize>,
    /// The row the selection is on, among those the query leaves.
    selected: usize,
    /// What was typed to narrow the rows; empty shows them all.
    query: String,
    /// The model or preset in use, which the picker opens on and returns to when the query clears.
    current: Option<String>,
    /// Sessions are not searched: they carry no model's words to match.
    filterable: bool,
    note: Option<String>,
    title: &'static str,
    hint: &'static str,
}

struct Entry {
    pick: Pick,
    row: Vec<Line>,
    /// What a typed query is matched against; labels have none and a query hides them.
    search: Option<String>,
}

enum Pick {
    /// A model or a preset, by the name either is chosen by.
    Model(String),
    /// Write a new preset to edit.
    NewPreset,
    Session(PathBuf),
    /// A row to read, not to pick: a heading, a preset's rule.
    Label,
}

/// What `/models` lists.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Show {
    /// The models, then the presets.
    All,
    /// The presets alone, each with its rules.
    Presets,
}

impl Picker {
    /// The models then the presets `listed`, or those alone with their rules and where they live
    /// (`dir`), opened on the one in use.
    pub fn models(
        catalog: &Catalog,
        listed: &[Preset],
        dir: &Path,
        current: &str,
        show: Show,
    ) -> Self {
        let words = &ui::get().picker;
        let mut entries: Vec<Entry> = Vec::new();
        match show {
            Show::All => {
                for model in &catalog.models {
                    entries.push(Entry {
                        pick: Pick::Model(model.id.clone()),
                        row: models::row(model, current),
                        search: Some(search::words(model)),
                    });
                }
                entries.push(Entry {
                    pick: Pick::Label,
                    row: presets::heading(),
                    search: None,
                });
            }
            Show::Presets => {}
        }
        for preset in listed {
            entries.push(Entry {
                pick: Pick::Model(preset.id.clone()),
                row: presets::row(preset, current),
                search: Some(format!("{} {}", preset.id, preset.description)),
            });
            if show == Show::Presets {
                for rule in presets::rule_rows(preset) {
                    entries.push(Entry {
                        pick: Pick::Label,
                        row: rule,
                        search: None,
                    });
                }
            }
        }
        entries.push(Entry {
            pick: Pick::NewPreset,
            row: presets::new_row(),
            search: None,
        });
        let (header, aligns, drop, note) = match show {
            Show::All => (
                models::header(),
                models::aligns(),
                models::drop_order(),
                models::source_note(catalog),
            ),
            Show::Presets => (
                presets::header(),
                Vec::new(),
                Vec::new(),
                presets::folder_note(dir, listed.is_empty()),
            ),
        };
        let mut picker = Self {
            entries,
            header,
            aligns,
            drop,
            selected: 0,
            query: String::new(),
            current: Some(current.to_owned()),
            filterable: true,
            note: Some(note),
            title: match show {
                Show::All => &words.title,
                Show::Presets => &words.presets_title,
            },
            hint: &words.hint,
        };
        picker.reselect();
        picker
    }

    /// Sessions to go on with, newest first.
    pub fn sessions(list: Vec<Summary>) -> Self {
        let words = &ui::get().resume;
        let mut rows = sessions::rows(&list).into_iter();
        let header = rows.next().unwrap_or_default();
        let entries = list
            .into_iter()
            .zip(rows)
            .map(|(summary, row)| Entry {
                pick: Pick::Session(summary.path),
                row,
                search: None,
            })
            .collect();
        Self {
            entries,
            header,
            aligns: sessions::aligns(),
            drop: sessions::drop_order(),
            selected: 0,
            query: String::new(),
            current: None,
            filterable: false,
            note: None,
            title: &words.title,
            hint: &words.hint,
        }
    }

    /// Opens narrowed to `query`, as `/models --search` asks.
    pub fn with_query(mut self, query: &str) -> Self {
        let query = query.trim();
        if self.filterable && !query.is_empty() {
            self.query = query.to_owned();
            self.reselect();
        }
        self
    }

    /// The entries the query leaves, by their place in `entries`.
    fn visible(&self) -> Vec<usize> {
        (0..self.entries.len())
            .filter(|&i| {
                if self.query.is_empty() {
                    return true;
                }
                let entry = &self.entries[i];
                entry.pick.pickable()
                    && entry
                        .search
                        .as_deref()
                        .is_some_and(|text| search::matches_words(text, &self.query))
            })
            .collect()
    }

    /// Puts the selection on what is in use, else the first row that can be picked.
    fn reselect(&mut self) {
        let visible = self.visible();
        self.selected = self
            .current
            .as_deref()
            .and_then(|id| {
                visible
                    .iter()
                    .position(|&i| matches!(&self.entries[i].pick, Pick::Model(name) if name == id))
            })
            .or_else(|| {
                visible
                    .iter()
                    .position(|&i| self.entries[i].pick.pickable())
            })
            .unwrap_or(0);
    }

    /// Moves the selection a row, over any that cannot be picked; at an end it stays.
    fn step(&mut self, action: Action) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let mut at = self.selected.min(visible.len() - 1);
        loop {
            let next = rows::step(at, action, visible.len(), false);
            if next == at {
                self.selected = at;
                return;
            }
            at = next;
            if self.entries[visible[at]].pick.pickable() {
                self.selected = at;
                return;
            }
        }
    }
}

impl Pick {
    fn pickable(&self) -> bool {
        !matches!(self, Self::Label)
    }
}

impl Picker {
    /// Adds a typed character to the query; control keys and modifiers are not text.
    fn type_key(&mut self, key: &KeyEvent) {
        let KeyCode::Char(c) = key.code else {
            return;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT)
            || c.is_control()
        {
            return;
        }
        self.query.push(c);
        self.reselect();
    }
}

impl Card for Picker {
    fn key(&mut self, action: Option<Action>, key: &KeyEvent) -> Next {
        if self.filterable && !self.query.is_empty() {
            match action {
                Some(Action::Backspace) => {
                    self.query.pop();
                    self.reselect();
                    return Next::Stay;
                }
                Some(Action::Escape) => {
                    self.query.clear();
                    self.reselect();
                    return Next::Stay;
                }
                _ => {}
            }
        }
        match action {
            Some(step @ (Action::Up | Action::Down)) => self.step(step),
            Some(Action::Right | Action::Submit) => {
                let visible = self.visible();
                return match visible
                    .get(self.selected)
                    .and_then(|&i| self.entries.get(i))
                {
                    Some(entry) => match &entry.pick {
                        Pick::Model(id) => Next::UseModel(id.clone()),
                        Pick::NewPreset => Next::NewPreset,
                        Pick::Session(path) => Next::Resume(path.clone()),
                        Pick::Label => Next::Stay,
                    },
                    None => Next::Stay,
                };
            }
            Some(Action::Left | Action::Escape) => return Next::Close(None),
            None if self.filterable => self.type_key(key),
            _ => {}
        }
        Next::Stay
    }

    fn insert(&mut self, text: &str) {
        if !self.filterable {
            return;
        }
        let words: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if words.trim().is_empty() {
            return;
        }
        self.query.push_str(&words);
        self.reselect();
    }

    fn render(&self, width: usize) -> Vec<Line> {
        let words = &ui::get().picker;
        let inner = boxed::inner(width).saturating_sub(rows::MARK);
        let visible = self.visible();
        let mut body = Vec::new();
        if self.filterable {
            let mut line = Line::styled(&words.filter, Style::fg("muted"));
            line.push(&self.query, Style::default());
            line.push(&words.cursor, Style::fg("accent"));
            body.push(line);
        }
        let mut cells = Vec::with_capacity(visible.len() + 1);
        cells.push(self.header.clone());
        cells.extend(visible.iter().map(|&i| self.entries[i].row.clone()));
        let mut lines = table::plain(&cells, &self.aligns, &self.drop, inner, GAP).into_iter();
        if let Some(header) = lines.next() {
            let mut line = Line::plain(" ".repeat(rows::MARK));
            line.extend(header);
            body.push(line);
        }
        body.extend(rows::marked(lines, self.selected));
        if visible.is_empty() && !self.query.is_empty() {
            let none = fill(&words.no_match, &[("query", &self.query)]);
            body.push(Line::styled(none, Style::fg("muted")));
        }
        // The presets sit under the models, often past the window: say there is more.
        let hidden = rows::below(visible.len(), self.selected);
        if hidden > 0 {
            let more = ui::get().text.more_lines.of(hidden);
            body.push(Line::styled(more, Style::fg("muted")));
        }
        if let Some(note) = &self.note {
            body.push(Line::default());
            body.push(Line::styled(note, Style::fg("muted")));
        }
        let title = Line::styled(self.title, Style::default().bold());
        // As wide as the list needs, not the whole terminal.
        let needed = boxed::outer(body.iter().map(Line::width).max().unwrap_or(0));
        let mut out = boxed(title, &body, needed.min(width));
        out.push(boxed::hint(self.hint, width));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::tui::keymap::press;
    use crate::models::preset;
    use crate::models::{self as catalogs, Source};

    const DIR: &str = "/home/u/.elorus/presets";

    fn key(picker: &mut Picker, action: Action) -> Next {
        picker.key(Some(action), &press(KeyCode::Null))
    }

    fn catalog() -> Catalog {
        Catalog {
            models: catalogs::snapshot(),
            source: Source::Snapshot,
            fetched_at: None,
            note: None,
        }
    }

    #[test]
    fn opens_on_the_current_model_and_picks_with_enter_or_right() {
        let catalog = catalog();
        let second = catalog.models[1].id.clone();
        let mut picker = Picker::models(&catalog, &[], DIR.as_ref(), &second, Show::All);
        assert!(matches!(key(&mut picker, Action::Submit), Next::UseModel(id) if id == second));
        key(&mut picker, Action::Up);
        let first = catalog.models[0].id.clone();
        assert!(matches!(key(&mut picker, Action::Right), Next::UseModel(id) if id == first));
        assert!(matches!(
            key(&mut picker, Action::Escape),
            Next::Close(None)
        ));
    }

    #[test]
    fn rows_line_up_inside_the_box_at_any_width() {
        let catalog = catalog();
        let picker = Picker::models(&catalog, &[], DIR.as_ref(), "elorus-2", Show::All);
        for width in [50, 80, 140] {
            let lines = picker.render(width);
            assert!(lines.iter().all(|l| l.width() <= width), "width {width}");
            let text: Vec<String> = lines.iter().map(Line::text).collect();
            assert!(text[1].contains("search"), "{text:?}");
            assert!(text[2].contains("model"), "{text:?}");
            assert!(
                text.iter().any(|l| l.starts_with("│ › ●  elorus-2")),
                "{text:?}"
            );
        }
    }

    fn listed() -> Vec<Preset> {
        let text = "description = \"flash\"\n[[rule]]\nwhen = { agent = [\"subagent\"] }\nmodel = \"a\"\n[[rule]]\nmodel = \"b\"";
        vec![preset::parsed("cheap", text)]
    }

    #[test]
    fn presets_follow_the_models_under_a_heading_the_selection_steps_over() {
        let catalog = catalog();
        let last = catalog.models.last().unwrap().id.clone();
        let mut picker = Picker::models(&catalog, &listed(), DIR.as_ref(), &last, Show::All);
        let text = |picker: &Picker| -> Vec<String> {
            picker.render(140).iter().map(Line::text).collect()
        };
        // Past the window, the rows below are counted.
        let shown = text(&picker);
        assert!(
            shown.iter().any(|l| l.contains("… 3 more lines")),
            "{shown:?}"
        );
        key(&mut picker, Action::Down);
        let shown = text(&picker);
        let at = |word: &str| shown.iter().position(|l| l.contains(word)).unwrap();
        assert!(
            at(&last) < at("presets") && at("presets") < at("cheap"),
            "{shown:?}"
        );
        assert!(
            shown.iter().any(|l| l.contains("… 1 more line")),
            "{shown:?}"
        );
        // The rules are /presets' to show.
        assert!(!shown.iter().any(|l| l.contains("otherwise")), "{shown:?}");
        assert!(matches!(key(&mut picker, Action::Submit), Next::UseModel(id) if id == "cheap"));
        key(&mut picker, Action::Down);
        assert!(matches!(key(&mut picker, Action::Submit), Next::NewPreset));
        key(&mut picker, Action::Down);
        assert!(matches!(key(&mut picker, Action::Submit), Next::NewPreset));
        key(&mut picker, Action::Up);
        key(&mut picker, Action::Up);
        assert!(matches!(key(&mut picker, Action::Submit), Next::UseModel(id) if id == last));
    }

    #[test]
    fn presets_alone_show_their_rules_and_open_on_the_one_in_use() {
        let mut picker =
            Picker::models(&catalog(), &listed(), DIR.as_ref(), "cheap", Show::Presets);
        let text: Vec<String> = picker.render(120).iter().map(Line::text).collect();
        assert!(text.iter().any(|l| l.contains("› ●  cheap")), "{text:?}");
        assert!(
            text.iter().any(|l| l.contains("agent subagent")),
            "{text:?}"
        );
        assert!(text.iter().any(|l| l.contains(DIR)), "{text:?}");
        assert!(!text.iter().any(|l| l.contains("elorus-2")), "{text:?}");
        assert!(matches!(key(&mut picker, Action::Submit), Next::UseModel(id) if id == "cheap"));
        key(&mut picker, Action::Down);
        assert!(matches!(key(&mut picker, Action::Submit), Next::NewPreset));
    }

    #[test]
    fn typing_narrows_the_list_and_escape_clears_before_it_closes() {
        let mut picker = Picker::models(&catalog(), &listed(), DIR.as_ref(), "elorus-2", Show::All);
        picker.insert("cheap");
        let text: Vec<String> = picker.render(140).iter().map(Line::text).collect();
        assert!(text.iter().any(|l| l.contains("cheap")), "{text:?}");
        assert!(!text.iter().any(|l| l.contains("elorus-2")), "{text:?}");
        assert!(
            matches!(key(&mut picker, Action::Escape), Next::Stay),
            "{text:?}"
        );
        let text: Vec<String> = picker.render(140).iter().map(Line::text).collect();
        assert!(text.iter().any(|l| l.contains("elorus-2")), "{text:?}");
        assert!(matches!(
            key(&mut picker, Action::Escape),
            Next::Close(None)
        ));
    }

    #[test]
    fn a_query_with_no_match_says_so() {
        let mut picker = Picker::models(&catalog(), &[], DIR.as_ref(), "elorus-2", Show::All);
        picker.insert("nothing-matches-this");
        let text: Vec<String> = picker.render(140).iter().map(Line::text).collect();
        assert!(
            text.iter()
                .any(|l| l.contains("No model or preset matches")),
            "{text:?}"
        );
        assert!(matches!(key(&mut picker, Action::Submit), Next::Stay));
    }

    #[test]
    fn sessions_list_newest_first_and_pick_a_file() {
        let at = |s: &str| Some(s.parse().unwrap());
        let list = vec![
            Summary {
                path: "/s/new.jsonl".into(),
                modified: at("2026-10-16T09:00:00Z"),
                first_prompt: "fix the parser\nand its tests".into(),
                prompts: 3,
            },
            Summary {
                path: "/s/old.jsonl".into(),
                modified: at("2026-10-15T09:00:00Z"),
                first_prompt: "explain".into(),
                prompts: 1,
            },
        ];
        let mut picker = Picker::sessions(list);
        for width in [50, 80, 140] {
            let lines = picker.render(width);
            assert!(lines.iter().all(|l| l.width() <= width), "width {width}");
            let text: Vec<String> = lines.iter().map(Line::text).collect();
            assert!(
                text.iter().any(|l| l.contains("fix the parser")),
                "{text:?}"
            );
            assert!(!text.iter().any(|l| l.contains("its tests")), "{text:?}");
        }
        key(&mut picker, Action::Down);
        assert!(
            matches!(key(&mut picker, Action::Submit), Next::Resume(path) if path.ends_with("old.jsonl"))
        );
    }
}
