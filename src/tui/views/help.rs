use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::tui::Screen;
use crate::tui::theme::ThemeColors;

/// Render a centered help overlay with keybindings filtered by source screen.
pub fn render_help(
    frame: &mut Frame,
    area: Rect,
    source_screen: &Screen,
    search_active: bool,
    is_production_delete: Option<bool>,
    theme: &ThemeColors,
    scroll: u16,
) {
    let (sections, context_label) =
        build_help_sections(source_screen, search_active, is_production_delete, theme);
    let content_height = sections.len() as u16 + 2; // +2 for borders

    // Size to content, capped at 90% of terminal height
    let max_height = (area.height as u32 * 90 / 100) as u16;
    let popup_height = content_height.min(max_height);
    let popup = centered_rect_with_height(60, popup_height, area);

    // Clear the area behind the popup
    frame.render_widget(Clear, popup);

    let is_scrollable = content_height > popup_height;
    let title_bottom = if is_scrollable {
        " \u{2191}\u{2193} scroll "
    } else {
        ""
    };

    let title = format!(" {context_label} \u{2014} Keybindings ");

    let block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .title_bottom(Line::from(title_bottom).alignment(Alignment::Center))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.surface));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let paragraph = Paragraph::new(sections).scroll((scroll, 0));
    frame.render_widget(paragraph, inner);
}

/// Build help sections filtered by the source screen.
///
/// Returns `(lines, context_label)` where `context_label` is used in the overlay title.
fn build_help_sections(
    source_screen: &Screen,
    search_active: bool,
    is_production_delete: Option<bool>,
    theme: &ThemeColors,
) -> (Vec<Line<'static>>, &'static str) {
    let mut lines = Vec::new();

    let label = match source_screen {
        Screen::List if search_active => {
            build_search_sections(&mut lines, theme);
            "Search"
        }
        Screen::List => {
            build_list_sections(&mut lines, theme);
            "List"
        }
        Screen::AddForm | Screen::EditForm(_) => {
            build_form_sections(&mut lines, theme);
            "Form"
        }
        Screen::DeleteConfirm(_) => {
            build_delete_sections(&mut lines, is_production_delete, theme);
            "Delete"
        }
        Screen::Help => {
            // Fallback — should not happen in practice since we use help_source
            build_list_sections(&mut lines, theme);
            "List"
        }
    };

    (lines, label)
}

/// Sections shown when on the list screen (search inactive).
fn build_list_sections(lines: &mut Vec<Line<'static>>, theme: &ThemeColors) {
    section_header(lines, "Navigation", theme);
    key_hint(lines, "\u{2191} / k / Ctrl+P", "Move selection up", theme);
    key_hint(lines, "\u{2193} / j / Ctrl+N", "Move selection down", theme);
    key_hint(lines, "Home / g", "Jump to first item", theme);
    key_hint(lines, "End / G", "Jump to last item", theme);
    key_hint(lines, "Page Up", "Jump up 10 items", theme);
    key_hint(lines, "Page Down", "Jump down 10 items", theme);
    lines.push(Line::from(""));

    section_header(lines, "Actions", theme);
    key_hint(lines, "Enter", "SSH connect", theme);
    key_hint(lines, "f", "SFTP file browser", theme);
    key_hint(lines, "a", "Add new bookmark", theme);
    key_hint(lines, "e", "Edit selected bookmark", theme);
    key_hint(lines, "d", "Delete selected bookmark", theme);
    lines.push(Line::from(""));

    section_header(lines, "Search & Filter", theme);
    key_hint(lines, "/", "Toggle search mode", theme);
    key_hint(lines, "1-5", "Filter by environment", theme);
    key_hint(lines, "0", "Clear environment filter", theme);
    lines.push(Line::from(""));

    section_header(lines, "Environment Filters", theme);
    key_hint(lines, "1", "Production", theme);
    key_hint(lines, "2", "Staging", theme);
    key_hint(lines, "3", "Development", theme);
    key_hint(lines, "4", "Local", theme);
    key_hint(lines, "5", "Testing", theme);
    lines.push(Line::from(""));

    section_header(lines, "General", theme);
    key_hint(lines, "?", "Toggle this help", theme);
    key_hint(lines, "q / Ctrl+C", "Quit", theme);
}

/// Sections shown when search is active on the list screen.
fn build_search_sections(lines: &mut Vec<Line<'static>>, theme: &ThemeColors) {
    section_header(lines, "Navigation", theme);
    key_hint(lines, "\u{2191}", "Move selection up", theme);
    key_hint(lines, "\u{2193}", "Move selection down", theme);
    lines.push(Line::from(""));

    section_header(lines, "Search Controls", theme);
    key_hint(lines, "Type", "Filter bookmarks", theme);
    key_hint(lines, "Enter", "Confirm search and exit search mode", theme);
    key_hint(lines, "Esc", "Clear search and exit search mode", theme);
    key_hint(lines, "Backspace", "Delete last character", theme);
    lines.push(Line::from(""));

    section_header(lines, "General", theme);
    key_hint(lines, "?", "Toggle this help", theme);
    key_hint(lines, "Ctrl+C", "Quit", theme);
}

/// Sections shown on the add/edit form screen.
fn build_form_sections(lines: &mut Vec<Line<'static>>, theme: &ThemeColors) {
    section_header(lines, "Form Navigation", theme);
    key_hint(lines, "Tab / \u{2193}", "Next field", theme);
    key_hint(lines, "Shift+Tab / \u{2191}", "Previous field", theme);
    lines.push(Line::from(""));

    section_header(lines, "Field Editing", theme);
    key_hint(lines, "Type", "Enter text in focused field", theme);
    key_hint(
        lines,
        "\u{2190} / \u{2192}",
        "Cycle environment / profile",
        theme,
    );
    key_hint(lines, "Backspace", "Delete last character", theme);
    lines.push(Line::from(""));

    section_header(lines, "Actions", theme);
    key_hint(lines, "Enter", "Save bookmark", theme);
    key_hint(lines, "Esc", "Cancel and return to list", theme);
    lines.push(Line::from(""));

    section_header(lines, "General", theme);
    key_hint(lines, "?", "Toggle this help", theme);
}

/// Sections shown on the delete confirmation screen.
fn build_delete_sections(
    lines: &mut Vec<Line<'static>>,
    is_production_delete: Option<bool>,
    theme: &ThemeColors,
) {
    section_header(lines, "Confirm / Cancel", theme);
    key_hint(lines, "Enter", "Confirm deletion", theme);
    key_hint(lines, "Esc", "Cancel and return to list", theme);

    if is_production_delete == Some(true) {
        lines.push(Line::from(""));
        section_header(lines, "Production Safety", theme);
        key_hint(
            lines,
            "Type \"yes\"",
            "Required to confirm production bookmark deletion",
            theme,
        );
        key_hint(lines, "Backspace", "Delete last character", theme);
    }
    lines.push(Line::from(""));

    section_header(lines, "General", theme);
    key_hint(lines, "?", "Toggle this help", theme);
}

fn section_header(lines: &mut Vec<Line<'static>>, title: &str, theme: &ThemeColors) {
    lines.push(Line::from(Span::styled(
        format!("  {title}"),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
}

fn key_hint(lines: &mut Vec<Line<'static>>, key: &str, desc: &str, theme: &ThemeColors) {
    lines.push(Line::from(vec![
        Span::styled(
            format!("    {key:<20}"),
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(desc.to_string(), Style::default().fg(theme.fg)),
    ]));
}

/// Create a centered rectangle with given percentage width and fixed height.
fn centered_rect_with_height(percent_x: u16, height: u16, area: Rect) -> Rect {
    let v_pad = area.height.saturating_sub(height) / 2;
    let vertical = Layout::vertical([
        Constraint::Length(v_pad),
        Constraint::Length(height),
        Constraint::Min(0),
    ])
    .split(area);

    let horizontal = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1]);

    horizontal[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::resolve_theme;

    /// Flatten all Line spans into a single string for easy content checks.
    fn lines_to_string(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn default_theme() -> ThemeColors {
        resolve_theme("")
    }

    #[test]
    fn test_list_help_no_ssh_session() {
        let theme = default_theme();
        let (sections, label) = build_help_sections(&Screen::List, false, None, &theme);
        let text = lines_to_string(&sections);
        assert_eq!(label, "List");
        assert!(
            !text.contains("In SSH Session"),
            "List help should not contain SSH session section"
        );
        assert!(
            !text.contains("~~"),
            "List help should not contain snippet picker escape"
        );
        assert!(
            !text.contains("~b"),
            "List help should not contain save-as-bookmark escape"
        );
        assert!(
            !text.contains("~f"),
            "List help should not contain file browser escape"
        );
    }

    #[test]
    fn test_list_help_contains_navigation_and_actions() {
        let theme = default_theme();
        let (sections, _) = build_help_sections(&Screen::List, false, None, &theme);
        let text = lines_to_string(&sections);
        assert!(text.contains("Navigation"), "should contain Navigation");
        assert!(text.contains("Actions"), "should contain Actions");
        assert!(
            text.contains("Search & Filter"),
            "should contain Search & Filter"
        );
        assert!(text.contains("SSH connect"), "should contain SSH connect");
        assert!(text.contains("General"), "should contain General");
    }

    #[test]
    fn test_search_help_no_list_actions() {
        let theme = default_theme();
        let (sections, label) = build_help_sections(&Screen::List, true, None, &theme);
        let text = lines_to_string(&sections);
        assert_eq!(label, "Search");
        assert!(
            text.contains("Search Controls"),
            "should contain Search Controls"
        );
        assert!(
            !text.contains("SSH connect"),
            "search help should not show SSH connect"
        );
        assert!(
            !text.contains("Add new bookmark"),
            "search help should not show Add action"
        );
        assert!(
            !text.contains("Delete selected"),
            "search help should not show Delete action"
        );
    }

    #[test]
    fn test_form_help_no_list_actions() {
        let theme = default_theme();
        let (sections, label) = build_help_sections(&Screen::AddForm, false, None, &theme);
        let text = lines_to_string(&sections);
        assert_eq!(label, "Form");
        assert!(
            text.contains("Form Navigation"),
            "should contain Form Navigation"
        );
        assert!(
            text.contains("Field Editing"),
            "should contain Field Editing"
        );
        assert!(
            !text.contains("SSH connect"),
            "form help should not show SSH connect"
        );
        assert!(
            !text.contains("Delete selected"),
            "form help should not show Delete action"
        );
    }

    #[test]
    fn test_form_help_uses_arrows_for_env_cycle() {
        let theme = default_theme();
        let (sections, _) = build_help_sections(&Screen::AddForm, false, None, &theme);
        let text = lines_to_string(&sections);
        // AC-9: env cycling uses Left/Right arrows, NOT Space
        assert!(
            text.contains('\u{2190}') && text.contains('\u{2192}'),
            "form help should show Left/Right arrows for env cycling"
        );
    }

    #[test]
    fn test_edit_form_help_same_as_add() {
        let theme = default_theme();
        let (add_sections, add_label) = build_help_sections(&Screen::AddForm, false, None, &theme);
        let (edit_sections, edit_label) =
            build_help_sections(&Screen::EditForm(0), false, None, &theme);
        assert_eq!(add_label, "Form");
        assert_eq!(edit_label, "Form");
        assert_eq!(
            lines_to_string(&add_sections),
            lines_to_string(&edit_sections),
            "Add and Edit form help should have the same content"
        );
    }

    #[test]
    fn test_delete_help_production_note() {
        let theme = default_theme();
        let (sections, label) =
            build_help_sections(&Screen::DeleteConfirm(0), false, Some(true), &theme);
        let text = lines_to_string(&sections);
        assert_eq!(label, "Delete");
        assert!(
            text.contains("Production Safety"),
            "production delete should show safety section"
        );
        assert!(
            text.contains("yes"),
            "production delete should mention typing 'yes'"
        );
    }

    #[test]
    fn test_delete_help_no_production_note() {
        let theme = default_theme();
        let (sections, label) =
            build_help_sections(&Screen::DeleteConfirm(0), false, Some(false), &theme);
        let text = lines_to_string(&sections);
        assert_eq!(label, "Delete");
        assert!(
            !text.contains("Production Safety"),
            "non-production delete should not show safety section"
        );
        assert!(
            text.contains("Confirm / Cancel"),
            "should still show confirm/cancel"
        );
    }

    #[test]
    fn test_delete_help_invalid_index_no_panic() {
        let theme = default_theme();
        // Using None for is_production_delete simulates the case where confirm_state
        // is not available (e.g., stale index). Should not panic, shows generic help.
        let (sections, label) =
            build_help_sections(&Screen::DeleteConfirm(9999), false, None, &theme);
        let text = lines_to_string(&sections);
        assert_eq!(label, "Delete");
        assert!(
            text.contains("Confirm / Cancel"),
            "should show generic confirm/cancel even with invalid index"
        );
        assert!(
            !text.contains("Production Safety"),
            "should not show production note when state is unknown"
        );
    }

    #[test]
    fn test_each_screen_produces_different_content() {
        let theme = default_theme();
        let (list, _) = build_help_sections(&Screen::List, false, None, &theme);
        let (search, _) = build_help_sections(&Screen::List, true, None, &theme);
        let (form, _) = build_help_sections(&Screen::AddForm, false, None, &theme);
        let (delete, _) =
            build_help_sections(&Screen::DeleteConfirm(0), false, Some(false), &theme);

        let list_text = lines_to_string(&list);
        let search_text = lines_to_string(&search);
        let form_text = lines_to_string(&form);
        let delete_text = lines_to_string(&delete);

        // Each should be distinct
        assert_ne!(list_text, search_text);
        assert_ne!(list_text, form_text);
        assert_ne!(list_text, delete_text);
        assert_ne!(form_text, delete_text);
    }

    #[test]
    fn test_no_ssh_session_on_any_screen() {
        let theme = default_theme();
        let screens: Vec<(Screen, bool, Option<bool>)> = vec![
            (Screen::List, false, None),
            (Screen::List, true, None),
            (Screen::AddForm, false, None),
            (Screen::EditForm(0), false, None),
            (Screen::DeleteConfirm(0), false, Some(false)),
            (Screen::DeleteConfirm(0), false, Some(true)),
            (Screen::Help, false, None),
        ];

        for (screen, search, prod) in &screens {
            let (sections, _) = build_help_sections(screen, *search, *prod, &theme);
            let text = lines_to_string(&sections);
            assert!(
                !text.contains("In SSH Session"),
                "Screen {:?} should not contain SSH session section",
                screen
            );
        }
    }
}
