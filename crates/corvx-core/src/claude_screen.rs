//! What CORVX reads from Claude Code's screen, in one place.
//!
//! CORVX prefers Claude Code's documented interfaces: hooks for what an agent is doing, the
//! status line for cost and context, MCP for orchestration. A few things have no such
//! interface and are recognised by their text on screen. Every such check lives here, with
//! the screens it was written against, so a Claude Code update that rewords one is found in
//! one place (`corvx doctor` reports the installed version against [`TESTED_VERSION`]).

/// The Claude Code version these checks were last confirmed against.
pub const TESTED_VERSION: &str = "2.1.323";

/// Claude Code asks whether to trust a folder the first time it starts there, before any
/// hook runs.
pub fn trust_question(screen: &str) -> bool {
    screen.contains("trust this folder") || screen.contains("trust the files in this folder")
}

/// The trust question with "Yes" selected, so Enter trusts the folder (Enter on the default
/// choice exits Claude Code).
pub fn trust_yes_selected(screen: &str) -> bool {
    screen.contains("❯ Yes, I trust this folder") || screen.contains("> Yes, I trust")
}

/// Claude Code moved its conversation into the background (Esc while idle) and shows its
/// list of sessions; Enter brings the conversation back. `screen` is plain text.
pub fn in_background(screen: &str) -> bool {
    screen.contains("conversation moved to the background")
        || screen
            .lines()
            .any(|l| l.contains("enter to return") && l.contains("space to reply"))
}

/// Lines at the bottom of a Claude Code screen that say nothing about the work: box borders,
/// the prompt, the mode footer and its hints, and CORVX's own status line.
pub fn footer_noise(line: &str) -> bool {
    let t = line.trim();
    t.chars().all(|c| "─━│╭╮╰╯┌┐└┘ ".contains(c))
        || t.starts_with('>')
        || t.starts_with('❯')
        || t.starts_with('⏵')
        || t.starts_with("│ >")
        || t.starts_with("corvx · ")
        || t.contains("for shortcuts")
        || t.contains("shift+tab")
        || t.contains("tmux detected")
        || t.contains("auto-accept edits")
        || t.contains("accept edits on")
        || t.contains("bypass permissions")
        || t.contains("/effort")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like Claude Code's trust question, with the default choice selected: the
    /// lines CORVX looks for (from the real-Claude e2e run), not a full capture.
    const TRUST: &str = "\
 Do you trust the files in this folder?

 /tmp/project

 Claude Code may read, write, or execute files contained in this directory.

 ❯ 1. No, exit
   2. Yes, I trust this folder

 Enter to confirm · Esc to cancel
";

    /// Claude Code 2.1.323's background view after Esc in an idle conversation, as captured
    /// from a real session (shortened).
    const AWAY: &str = "\
 0 awaiting input · 0 working · 1 completed

Your conversation moved to the background — enter opens it · esc returns to
it · ctrl+c twice quits

Completed
∙ Project state overview  adopted from previous supervisor                 5h
❯ describe a task for a new session
  ⏵⏵ auto mode · enter to return · space to reply · ? for shortcuts
";

    #[test]
    fn the_trust_question() {
        assert!(trust_question(TRUST));
        assert!(!trust_yes_selected(TRUST));
        assert!(trust_yes_selected(
            &TRUST
                .replace("❯ 1. No", "  1. No")
                .replace("   2. Yes", " ❯ Yes")
        ));
        assert!(!trust_question("⏺ I trust the tests now."));
    }

    #[test]
    fn the_background_view() {
        assert!(in_background(AWAY));
        assert!(!in_background(
            "⏺ I ran the server in the background.\n❯ \n"
        ));
    }

    #[test]
    fn footer_lines_are_noise() {
        for l in [
            "───────────",
            "❯ ",
            "  ⏵⏵ auto mode on (shift+tab to cycle)",
            "corvx · $0.42 · $3.10 today (estimated)",
        ] {
            assert!(footer_noise(l), "{l}");
        }
        assert!(!footer_noise("⏺ Edited src/cart.py"));
    }
}
