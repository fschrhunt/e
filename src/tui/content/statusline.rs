//! Activity row and status line — pure projections of app state.

use crate::core::output::compact_model_label;
use crate::core::providers::FailureCause;
use crate::tui::markdown::visible_width;
use crate::tui::theme::Theme;
use unicode_width::UnicodeWidthChar;

/// How long a "recovered" flash stays up before reverting to normal turn
/// activity — matches the reference client's brief, self-clearing confirm.
pub const RECOVERED_VISIBLE_MS: u64 = 1500;

/// Which transient progress surface owns the current turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnPhase {
    Thinking,
    /// Backing off after a retryable failure before another attempt.
    Retrying,
    Tool,
    AssistantText,
}

/// The detail `TurnPhase::Retrying` needs to render. `since` anchors the
/// live countdown — the label ticks the remaining seconds down from
/// `delay_secs` as the backoff burns, pi-style.
#[derive(Clone, Debug)]
pub struct RetryStatus {
    pub attempt: u32,
    pub limit: u32,
    pub delay_secs: u64,
    pub since: std::time::Instant,
    pub cause: FailureCause,
    pub reason: String,
}

/// A brief confirmation shown after a retry campaign's first successful
/// event, cleared once its display window elapses.
#[derive(Clone, Copy, Debug)]
pub struct RecoveredStatus {
    pub attempt: u32,
    pub limit: u32,
    pub since: std::time::Instant,
}

/// Truncate to `max_chars`, marking the cut with an ellipsis — keeps a raw
/// connect-error string or provider message from blowing out the row.
fn clip(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Elapsed time in the activity row grammar — the shared core formatter,
/// re-exported under its long-standing name.
pub use crate::core::output::format_elapsed;

/// Per-turn token flow and focused activity phase.
pub struct Turn {
    /// Latest request's full context, from real usage (a chars/4 seed until
    /// the first report lands).
    pub input: u64,
    /// True until a provider Usage frame replaces the chars/4 seed.
    pub input_estimated: bool,
    /// Output tokens of completed steps, from real usage.
    pub output: u64,
    /// chars/4 estimate of the current step's streamed text + reasoning.
    pub estimated_output: u64,
    streamed_chars: u64,
    /// Cumulative tool-call argument bytes streamed this step (real output
    /// the token estimate must count).
    assembly_bytes: u64,
    pub phase: TurnPhase,
    /// Set while `phase == Retrying`.
    pub retry: Option<RetryStatus>,
    /// Set briefly after a retry succeeds; the frame tick clears it.
    pub recovered: Option<RecoveredStatus>,
}

impl Default for Turn {
    fn default() -> Self {
        Self::new()
    }
}

impl Turn {
    pub fn new() -> Self {
        Turn {
            input: 0,
            input_estimated: false,
            output: 0,
            estimated_output: 0,
            streamed_chars: 0,
            assembly_bytes: 0,
            phase: TurnPhase::Thinking,
            retry: None,
            recovered: None,
        }
    }

    pub fn note_text(&mut self, text: &str) {
        self.streamed_chars = self
            .streamed_chars
            .saturating_add(text.chars().count() as u64);
        self.refresh_estimate();
    }

    /// Record the cumulative argument bytes the agent reported for this step.
    pub fn note_assembly(&mut self, cumulative_bytes: u64) {
        self.assembly_bytes = cumulative_bytes;
        self.refresh_estimate();
    }

    /// A real usage report landed: fold it in and reset the estimate — from
    /// here the estimate covers only what the *next* step streams, so the
    /// live display is real tokens plus the current step's delta, never an
    /// estimate of tokens already counted for real.
    pub fn note_usage(&mut self, input: u64, output: u64) {
        self.input = input;
        self.input_estimated = false;
        self.output = self.output.saturating_add(output);
        self.streamed_chars = 0;
        self.assembly_bytes = 0;
        self.estimated_output = 0;
    }

    pub fn seed_input(&mut self, input: u64) {
        self.input = input;
        self.input_estimated = true;
    }

    fn refresh_estimate(&mut self) {
        self.estimated_output = (self.streamed_chars + self.assembly_bytes).div_ceil(4);
    }

    pub fn tokens(&self) -> String {
        let output = self.output.saturating_add(self.estimated_output);
        if self.input == 0 && output == 0 {
            String::new()
        } else {
            // The reference prints raw counts — `(↑10 ↓20)` — never a
            // compacted `1k`. The `~` marks a chars/4 seed the reference
            // doesn't have; it clears once real usage lands.
            let estimate = if self.input_estimated { "~" } else { "" };
            format!("(↑{estimate}{} ↓{output})", self.input)
        }
    }

    /// The Thinking label split at the token suffix, so the paint layer can
    /// tone the verb and the `(↑… ↓…)` tail separately the reference way.
    pub fn label_parts(&self, elapsed_secs: u64) -> Option<(String, String)> {
        if self.recovered.is_some() || self.phase != TurnPhase::Thinking {
            return None;
        }
        let tokens = self.tokens();
        let suffix = if tokens.is_empty() {
            String::new()
        } else {
            format!(" {tokens}")
        };
        Some((
            format!("Thinking ({})", format_elapsed(elapsed_secs)),
            suffix,
        ))
    }

    /// The tool phase renders inside its transcript group, not in a duplicate
    /// footer row. Assistant text needs only markerless token progress. A
    /// recovered flash overrides everything else until it expires.
    pub fn label(&self, elapsed_secs: u64) -> Option<String> {
        if let Some(r) = &self.recovered {
            return Some(format!("Recovered · attempt {}/{}", r.attempt, r.limit));
        }
        match self.phase {
            TurnPhase::Thinking => {
                let (main, suffix) = self.label_parts(elapsed_secs)?;
                Some(format!("{main}{suffix}"))
            }
            TurnPhase::Retrying => {
                let r = self.retry.as_ref()?;
                let waited = r.since.elapsed().as_secs();
                let remaining = r.delay_secs.saturating_sub(waited);
                let reason = clip(&r.reason, 48);
                let hint = if reason.is_empty() {
                    String::new()
                } else {
                    format!(" · {reason}")
                };
                Some(if remaining > 0 {
                    format!(
                        "Retrying ({}/{}) in {}s… esc to cancel{hint}",
                        r.attempt, r.limit, remaining
                    )
                } else {
                    format!(
                        "Retrying ({}/{}) now… esc to cancel{hint}",
                        r.attempt, r.limit
                    )
                })
            }
            TurnPhase::Tool => None,
            TurnPhase::AssistantText => {
                let tokens = self.tokens();
                (!tokens.is_empty()).then_some(tokens)
            }
        }
    }
}

pub struct StatusData {
    /// `None` when no provider is signed in — nothing to show as "the"
    /// model, since none was actually chosen by the user.
    pub model: Option<String>,
    pub effort: Option<String>,
    pub session_name: Option<String>,
    /// Estimated tokens of the current context and the model's window, for
    /// the reference's `Context: 12k/200k 6%` segment.
    pub context_used: u64,
    pub context_total: Option<u64>,
    pub queued: usize,
    /// A turn is streaming — the reference says `enter queue`.
    pub streaming: bool,
    /// The workspace tail (`~`-relative) and git branch for the trailing
    /// identity segment.
    pub workspace: Option<String>,
    pub branch: Option<String>,
}

/// The reference caps the session title at 32 cells.
const MAX_SESSION_TITLE_CELLS: usize = 32;

fn prefix_by_cells(text: &str, cells: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > cells {
            break;
        }
        out.push(c);
        used += w;
    }
    out
}

fn suffix_by_cells(text: &str, cells: usize) -> String {
    let total: usize = text.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= cells {
        return text.to_string();
    }
    let mut skip = total.saturating_sub(cells);
    let mut out = String::new();
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if skip > 0 {
            skip = skip.saturating_sub(w);
            continue;
        }
        out.push(c);
    }
    out
}

/// The trailing `path (branch)` identity, budgeted the reference way: the
/// branch appears only when at least seven cells are free, takes at most
/// half the budget (floor four), and each side clips toward what matters —
/// the path keeps its tail, the branch its head.
fn workspace_identity(workspace: &str, branch: Option<&str>, budget: usize) -> Option<String> {
    if workspace.is_empty() || budget == 0 {
        return None;
    }
    if let Some(branch) = branch.filter(|b| !b.is_empty()) {
        if budget >= 7 {
            let branch_width: usize = branch.chars().map(|c| c.width().unwrap_or(0)).sum();
            let branch_budget = branch_width.min((budget - 4).min(4usize.max(budget / 2)));
            let path_budget = budget - 3 - branch_budget;
            return Some(format!(
                "{} ({})",
                suffix_by_cells(workspace, path_budget),
                prefix_by_cells(branch, branch_budget)
            ));
        }
    }
    Some(suffix_by_cells(workspace, budget))
}

/// The bottom row: dot-joined segments in the statusline gray — the
/// reference tones the whole row alike. With no panel open a blank spacer
/// rides above (the reference paints its bottom-divider row blank there);
/// an open panel's own divider sits directly above the row instead. A
/// transient overlay (armed-exit) rides right-aligned; a menu hint replaces
/// the row in dim.
pub fn statusline(
    theme: &Theme,
    data: &StatusData,
    overlay: Option<&str>,
    hint: Option<&str>,
    panel_open: bool,
    width: usize,
) -> Vec<String> {
    let lead: &[String] = if panel_open { &[] } else { &[String::new()] };
    if let Some(hint) = hint {
        let mut rows = lead.to_vec();
        rows.push(theme.fg("dim", hint));
        return rows;
    }
    let mut segments = Vec::new();
    if data.model.is_none() {
        segments.push("run /login".to_string());
    }
    if data.queued > 0 {
        segments.push(format!("queued {}", data.queued));
    }
    if data.streaming {
        segments.push("enter queue".to_string());
    }
    if let Some(model) = &data.model {
        segments.push(compact_model_label(model));
        if let Some(e) = &data.effort {
            if e != "off" {
                segments.push(e.clone());
            }
        }
    }
    if let Some(n) = &data.session_name {
        segments.push(prefix_by_cells(n, MAX_SESSION_TITLE_CELLS));
    }
    if data.context_used > 0 {
        segments.push(match data.context_total {
            Some(total) if total > 0 => format!(
                "Context: {}k/{}k {}%",
                data.context_used / 1000,
                total / 1000,
                (data.context_used * 100) / total
            ),
            _ => format!("Context: {}k", data.context_used / 1000),
        });
    }
    let mut line_plain = segments.join(" · ");
    if let Some(workspace) = &data.workspace {
        let used: usize = line_plain.chars().map(|c| c.width().unwrap_or(0)).sum();
        let sep = if line_plain.is_empty() { 0 } else { 3 };
        if used + sep < width {
            if let Some(identity) =
                workspace_identity(workspace, data.branch.as_deref(), width - used - sep)
            {
                if !line_plain.is_empty() {
                    line_plain.push_str(" · ");
                }
                line_plain.push_str(&identity);
            }
        }
    }

    let mut line = if line_plain.is_empty() {
        String::new()
    } else {
        theme.fg("muted", &line_plain)
    };
    if let Some(overlay) = overlay {
        let used = visible_width(&line);
        let pad = width.saturating_sub(used + overlay.chars().count());
        if pad > 1 {
            line.push_str(&" ".repeat(pad));
            line.push_str(&theme.fg("muted", overlay));
        } else {
            line = theme.fg("muted", overlay);
        }
    }
    let mut rows = lead.to_vec();
    rows.push(line);
    rows
}

#[cfg(test)]
mod tests {
    use super::{format_elapsed, RecoveredStatus, RetryStatus, Turn, TurnPhase};
    use crate::core::providers::FailureCause;

    #[test]
    fn activity_has_one_owner_per_phase() {
        let mut turn = Turn::new();
        assert_eq!(turn.label(0).as_deref(), Some("Thinking (0s)"));

        turn.phase = TurnPhase::Tool;
        assert_eq!(turn.label(1), None, "the focused group owns tool activity");

        turn.input = 1_000;
        turn.output = 20;
        turn.phase = TurnPhase::AssistantText;
        assert_eq!(turn.label(2).as_deref(), Some("(↑1000 ↓20)"));

        turn.phase = TurnPhase::Thinking;
        assert_eq!(turn.label(3).as_deref(), Some("Thinking (3s) (↑1000 ↓20)"));
    }

    #[test]
    fn elapsed_switches_to_minutes_above_a_minute() {
        // The reference grammar: no separators, no zero padding.
        assert_eq!(format_elapsed(59), "59s");
        assert_eq!(format_elapsed(60), "1m0s");
        assert_eq!(format_elapsed(636), "10m36s");
        assert_eq!(format_elapsed(1_080), "18m0s");
        assert_eq!(format_elapsed(3_663), "1h1m3s");
    }

    #[test]
    fn thinking_label_shows_minutes_past_a_minute() {
        let mut turn = Turn::new();
        turn.input = 39_000;
        turn.output = 20_000;
        // The reference prints raw token counts, never a compacted `39k`.
        assert_eq!(
            turn.label(636).as_deref(),
            Some("Thinking (10m36s) (↑39000 ↓20000)")
        );
    }

    #[test]
    fn assembly_bytes_count_toward_the_estimate_while_thinking() {
        let mut turn = Turn::new();
        turn.note_usage(50_000, 200);
        turn.note_assembly(8_000); // ~2k tokens of argument JSON so far
        assert_eq!(
            turn.label(7).as_deref(),
            Some("Thinking (7s) (↑50000 ↓2200)"),
            "argument streaming stays in the Thinking phase — the tool row, not the footer, owns the activity"
        );
        // The next cumulative report ticks the same counter.
        turn.note_assembly(12_000);
        assert_eq!(turn.tokens(), "(↑50000 ↓3200)");
    }

    #[test]
    fn usage_resets_the_streaming_estimate() {
        let mut turn = Turn::new();
        turn.note_text(&"x".repeat(4_000)); // ~1k estimated
        assert_eq!(turn.tokens(), "(↑0 ↓1000)");
        // Real usage lands: the estimate must not double what is now
        // counted for real.
        turn.note_usage(9_000, 800);
        assert_eq!(turn.tokens(), "(↑9000 ↓800)");
        // The next step's streaming adds on top of the real total.
        turn.note_text("abcd");
        assert_eq!(turn.tokens(), "(↑9000 ↓801)");
    }

    #[test]
    fn seeded_input_is_visibly_an_estimate() {
        let mut turn = Turn::new();
        turn.seed_input(5_208_000);
        assert_eq!(turn.tokens(), "(↑~5208000 ↓0)");
        turn.note_usage(10_000, 2);
        assert_eq!(turn.tokens(), "(↑10000 ↓2)");
    }

    #[test]
    fn retrying_counts_down_with_an_esc_hint() {
        let mut turn = Turn::new();
        turn.phase = TurnPhase::Retrying;
        turn.retry = Some(RetryStatus {
            attempt: 3,
            limit: 10,
            delay_secs: 4,
            since: std::time::Instant::now(),
            cause: FailureCause::ProviderUnavailable,
            reason: "503 Service Unavailable".into(),
        });
        assert_eq!(
            turn.label(0).as_deref(),
            Some("Retrying (3/10) in 4s… esc to cancel · 503 Service Unavailable")
        );
    }

    #[test]
    fn a_burned_backoff_says_retrying_now() {
        let mut turn = Turn::new();
        turn.phase = TurnPhase::Retrying;
        turn.retry = Some(RetryStatus {
            attempt: 1,
            limit: 10,
            delay_secs: 4,
            since: std::time::Instant::now() - std::time::Duration::from_secs(9),
            cause: FailureCause::RateLimited,
            reason: "429 Too Many Requests".into(),
        });
        assert_eq!(
            turn.label(0).as_deref(),
            Some("Retrying (1/10) now… esc to cancel · 429 Too Many Requests")
        );
    }

    #[test]
    fn retrying_clips_a_long_reason() {
        let mut turn = Turn::new();
        turn.phase = TurnPhase::Retrying;
        turn.retry = Some(RetryStatus {
            attempt: 1,
            limit: 10,
            delay_secs: 1,
            since: std::time::Instant::now(),
            cause: FailureCause::Network,
            reason: "x".repeat(200),
        });
        let label = turn.label(0).unwrap();
        assert!(label.len() < 150, "reason should be clipped: {label}");
        assert!(label.contains('…'));
    }

    #[test]
    fn recovered_overrides_the_underlying_phase() {
        let mut turn = Turn::new();
        turn.phase = TurnPhase::Thinking;
        turn.recovered = Some(RecoveredStatus {
            attempt: 4,
            limit: 10,
            since: std::time::Instant::now(),
        });
        assert_eq!(turn.label(9).as_deref(), Some("Recovered · attempt 4/10"));
    }
}
