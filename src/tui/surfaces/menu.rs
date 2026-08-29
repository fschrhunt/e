//! The footer picker band.
//!
//! Menus render between the composer and the status row, the reference
//! inline-picker shape:
//!
//! ```text
//!   ── divider ─────────────────────────────
//!   Commands 7 · Type to filter          1–7
//!
//!     /login   sign in to a provider
//!     /model   list or switch models       ← selected row filled, no caret
//!   ── divider ─────────────────────────────
//!   ↑↓ Navigate     Enter Use     Esc Close   (rides the status row)
//! ```
//!
//! Selection splits by surface — the model pickers brighten (bold ink), the
//! rest fill the row (`selectedBg` behind `selectedText`); unselected rows
//! stay dim, no caret either way. The band holds only its rows: at most
//! MAX_VISIBLE, shrinking with a short match list instead of blank-padding.

use crate::tui::markdown::visible_width;
use crate::tui::theme::Theme;

#[derive(Clone)]
pub struct MenuItem {
    pub label: String,
    pub description: String,
    /// Right-aligned dim metadata.
    pub meta: String,
    pub value: String,
}

impl MenuItem {
    pub fn new(label: &str, description: &str, value: &str) -> Self {
        MenuItem {
            label: label.into(),
            description: description.into(),
            meta: String::new(),
            value: value.into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    Commands,
    Files,
    Models,
    Sessions,
    Skills,
    /// The scoped-models multi-select: Space toggles, Enter closes.
    Scoped,
    /// /tree: pick an earlier point in this session to rewind to.
    Tree,
}

pub struct Menu {
    pub kind: MenuKind,
    pub title: &'static str,
    pub hint: &'static str,
    items: Vec<MenuItem>,
    filtered: Vec<usize>,
    pub selected: usize,
    window_start: usize,
    query: String,
}

pub const HINT_USE: &str = "↑↓ Navigate     Enter Use     Esc Close";
pub const HINT_SCOPED: &str =
    "↑↓ Navigate     Space Toggle     Ctrl+X Reset     Enter Done     Esc Close";
/// The reference keeps six selectable rows below the header.
const MAX_VISIBLE: usize = 6;

/// The char indices `fuzzy_score`'s walk would hit — the spans the picker
/// brightens so a filtered row shows why it matched. None when the query is
/// not a subsequence of the candidate.
pub fn fuzzy_positions(query: &str, candidate: &str) -> Option<Vec<usize>> {
    if query.is_empty() {
        return Some(Vec::new());
    }
    let lower = |c: char| c.to_lowercase().next().unwrap_or(c);
    let chars: Vec<char> = candidate.chars().map(lower).collect();
    let mut positions = Vec::with_capacity(query.chars().count());
    let mut from = 0usize;
    for needle in query.chars().map(lower) {
        let at = chars[from..].iter().position(|&c| c == needle)? + from;
        positions.push(at);
        from = at + 1;
    }
    Some(positions)
}

/// Subsequence fuzzy match; lower score is better, None is no match.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(usize::MAX / 2); // neutral: keep original order
    }
    let candidate_lower = candidate.to_lowercase();
    let query_lower = query.to_lowercase();
    let mut score = 0usize;
    let mut position = 0usize;
    let mut last_hit: Option<usize> = None;
    for needle in query_lower.chars() {
        let found = candidate_lower[position..].find(needle)?;
        let at = position + found;
        // Contiguity bonus: gaps cost, adjacency is free.
        if let Some(last) = last_hit {
            score += at - last - 1;
        } else {
            score += at; // earlier first hits rank higher
        }
        last_hit = Some(at);
        position = at + needle.len_utf8();
    }
    Some(score)
}

impl Menu {
    pub fn new(
        kind: MenuKind,
        title: &'static str,
        hint: &'static str,
        items: Vec<MenuItem>,
    ) -> Self {
        let mut menu = Menu {
            kind,
            title,
            hint,
            items,
            filtered: Vec::new(),
            selected: 0,
            window_start: 0,
            query: String::new(),
        };
        menu.refilter();
        menu
    }

    pub fn set_query(&mut self, query: &str) {
        if self.query != query {
            self.query = query.to_string();
            self.refilter();
        }
    }

    fn refilter(&mut self) {
        let mut scored: Vec<(usize, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let best = fuzzy_score(&self.query, &item.label)
                    .into_iter()
                    .chain(fuzzy_score(&self.query, &item.description))
                    .min()?;
                Some((best, i))
            })
            .collect();
        scored.sort_by_key(|(score, i)| (*score, *i));
        self.filtered = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
        self.window_start = 0;
    }

    pub fn len(&self) -> usize {
        self.filtered.len()
    }

    pub fn is_empty(&self) -> bool {
        self.filtered.is_empty()
    }

    /// Move the selection to the item with this value, if present.
    pub fn select_value(&mut self, value: &str) {
        if let Some(pos) = self
            .filtered
            .iter()
            .position(|&i| self.items[i].value == value)
        {
            self.selected = pos;
        }
    }

    pub fn for_each_item(&mut self, mut f: impl FnMut(&mut MenuItem)) {
        for item in &mut self.items {
            f(item);
        }
    }

    pub fn current_mut(&mut self) -> Option<&mut MenuItem> {
        let idx = *self.filtered.get(self.selected)?;
        self.items.get_mut(idx)
    }

    pub fn current(&self) -> Option<&MenuItem> {
        self.filtered.get(self.selected).map(|&i| &self.items[i])
    }

    pub fn step(&mut self, delta: isize) {
        let n = self.filtered.len();
        if n == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(n as isize) as usize;
        if self.selected < self.window_start {
            self.window_start = self.selected;
        }
        if self.selected >= self.window_start + MAX_VISIBLE {
            self.window_start = self.selected - MAX_VISIBLE + 1;
        }
    }

    /// What an empty result set says, in the reference's own words per
    /// surface.
    fn empty_notice(&self) -> &'static str {
        match self.kind {
            MenuKind::Commands => "no matching slash commands",
            MenuKind::Files => "no matching files",
            MenuKind::Skills => "No skills found.",
            _ => "Nothing found.",
        }
    }

    /// The row's dress as an open/close pair, so matched spans can reset
    /// and reopen mid-run. The reference splits its pickers: the model
    /// surfaces signal by brightness alone, every other picker fills the
    /// row — selection background, selection ink; unselected rows are dim.
    fn row_style(&self, theme: &Theme, selected: bool) -> (String, String) {
        if !selected {
            let open = theme.fg_prefix("dim").to_string();
            let close = if open.is_empty() { "" } else { "\x1b[39m" };
            return (open, close.into());
        }
        let bright = || {
            let fg = theme.fg_prefix("userMessageText");
            let close = if fg.is_empty() {
                "\x1b[22m"
            } else {
                "\x1b[39m\x1b[22m"
            };
            (format!("\x1b[1m{fg}"), close.to_string())
        };
        match self.kind {
            MenuKind::Models | MenuKind::Scoped => bright(),
            _ => {
                let bg = theme.bg_prefix("selectedBg");
                let fg = theme.fg_prefix("selectedText");
                if bg.is_empty() && fg.is_empty() {
                    bright()
                } else {
                    (format!("{bg}{fg}"), "\x1b[0m".into())
                }
            }
        }
    }

    pub fn render(&self, theme: &Theme, width: usize) -> Vec<String> {
        let count = self.filtered.len();
        let window_end = (self.window_start + MAX_VISIBLE).min(count);

        // The reference header is uniformly dim — `Commands 7 · Type to
        // filter` before the first keystroke, the bare noun and count once a
        // filter is live (the composer already shows the typed query).
        let mut header_plain = if self.query.is_empty() {
            format!("{} {count} · Type to filter", self.title)
        } else {
            format!("{} {count}", self.title)
        };
        if count > MAX_VISIBLE {
            // Scroll range, right-aligned with the reference's one-column
            // margin.
            let range = format!("{}–{}", self.window_start + 1, window_end);
            let pad =
                width.saturating_sub(1 + visible_width(&header_plain) + range.chars().count());
            if pad > 1 {
                header_plain.push_str(&" ".repeat(pad));
                header_plain.push_str(&range);
            }
        }
        let header = theme.fg("dim", &header_plain);

        let mut body: Vec<String> = Vec::new();
        if count == 0 {
            body.push(theme.fg("dim", &format!("  {}", self.empty_notice())));
        }
        let label_width = self.filtered[self.window_start..window_end]
            .iter()
            .map(|&i| visible_width(&self.items[i].label))
            .max()
            .unwrap_or(0)
            .min(36);
        for slot in self.window_start..window_end {
            let item = &self.items[self.filtered[slot]];
            let selected = slot == self.selected;
            let (open, close) = self.row_style(theme, selected);
            // The label, with the query's matched chars brightened: each hit
            // resets to bold and reopens the row's own dress after — the
            // reference's way of showing why a filtered row matched.
            let marks = if self.query.is_empty() {
                Vec::new()
            } else {
                fuzzy_positions(&self.query, &item.label).unwrap_or_default()
            };
            let mut content = String::new();
            for (i, c) in item.label.chars().enumerate() {
                if marks.contains(&i) {
                    content.push_str(&format!("\x1b[1m{c}\x1b[0m{open}"));
                } else {
                    content.push(c);
                }
            }
            content.push_str(&" ".repeat(label_width.saturating_sub(visible_width(&item.label))));
            if !item.description.is_empty() {
                content.push_str("   ");
                content.push_str(&item.description);
            }
            if !item.meta.is_empty() {
                let pad = width
                    .saturating_sub(1 + 2 + visible_width(&content) + visible_width(&item.meta));
                if pad > 3 {
                    content.push_str(&" ".repeat(pad));
                    content.push_str(&item.meta);
                }
            }
            // Escape-aware clipping: SGR runs are zero columns and a cut row
            // closes its styles instead of severing a sequence mid-run.
            body.push(crate::tui::markdown::clip_styled(
                &format!("  {open}{content}{close}"),
                width,
            ));
        }
        // The band holds only its rows — the reference never blank-pads a
        // short match list.
        crate::tui::panel::frame(theme, width, header, body)
    }
}

/// Pick the widest hint variant that fits — the reference degrades its nav
/// hints stepwise instead of clipping them.
pub fn degrade_hint(hint: &'static str, width: usize) -> &'static str {
    let variants: &[&'static str] = if hint == HINT_USE {
        &[
            HINT_USE,
            "↑↓ Navigate  Enter Use  Esc Close",
            "↑↓ Move  Enter  Esc",
            "Enter Use  Esc Close",
            "Enter Esc",
        ]
    } else {
        return hint;
    };
    variants
        .iter()
        .copied()
        .find(|v| v.chars().count() <= width)
        .unwrap_or(variants[variants.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::fuzzy_score;

    #[test]
    fn fuzzy_prefers_tight_early_matches() {
        // Subsequence required.
        assert!(fuzzy_score("xyz", "composer.rs").is_none());
        // Tighter match beats scattered.
        let tight = fuzzy_score("comp", "src/tui/composer.rs").unwrap();
        let scattered = fuzzy_score("comp", "core/completions.rs").unwrap();
        assert!(tight < usize::MAX / 2 && scattered < usize::MAX / 2);
        // Case-insensitive.
        assert!(fuzzy_score("LOG", "/login").is_some());
        // Empty query keeps everything, original order.
        assert_eq!(fuzzy_score("", "anything"), Some(usize::MAX / 2));
    }
}
