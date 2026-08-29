//! The first-visit trust panel — three-space prose and a `› ` caret on the
//! selected choice, asking whether e may load this directory's own
//! instructions. Shown once per directory; the answer persists in
//! ~/.e/trust.json. Unlike the auth panel's wide value column, the
//! descriptions here sit right beside the choices — a two-row question
//! reads as one block, not a table spanning the frame.

use crate::tui::markdown::clip_styled;
use crate::tui::render::bold;
use crate::tui::theme::Theme;

const CHOICES: [(&str, &str); 2] = [
    ("Trust this directory", "remembered in ~/.e/trust.json"),
    ("Not now", "work here without its instructions"),
];

pub fn render(stage: &TrustStage, theme: &Theme, width: usize, dir: &str) -> Vec<String> {
    let dim = |s: &str| theme.fg("dim", s);
    let label_col = CHOICES
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0);
    let choice = |index: usize| {
        let (label, description) = CHOICES[index];
        let selected = stage.selected == index;
        let caret = if selected { "› " } else { "  " };
        let pad = " ".repeat(label_col - label.chars().count());
        let text = format!("{caret}{label}{pad}   {description}");
        let row = if selected {
            bold(&theme.fg("userMessageText", &text))
        } else {
            dim(&text)
        };
        clip_styled(&row, width)
    };
    vec![
        String::new(),
        dim(&format!("   Trust {dir}?")),
        dim("   e reads the directory's AGENTS.md and .e/ skills+prompts into context, and runs tools here."),
        String::new(),
        choice(0),
        choice(1),
        String::new(),
        dim("   ↑↓ Choose · Enter Continue"),
    ]
}

pub struct TrustStage {
    pub selected: usize,
}
