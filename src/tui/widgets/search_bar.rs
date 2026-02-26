use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::config::model::Bookmark;

/// Fuzzy-match a query against a bookmark's searchable fields.
/// Returns Some(score) if matched, None if no match.
/// Search scope: name, host, user, tags (space-joined), env.
pub fn fuzzy_match_bookmark(
    matcher: &SkimMatcherV2,
    bookmark: &Bookmark,
    query: &str,
) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }

    let searchable = format!(
        "{} {} {} {} {}",
        bookmark.name,
        bookmark.host,
        bookmark.user.as_deref().unwrap_or(""),
        bookmark.tags.join(" "),
        bookmark.env,
    );

    matcher.fuzzy_match(&searchable, query)
}

/// Filter bookmarks by fuzzy query and optional environment filter.
/// Returns indices into the bookmarks slice, sorted by match score (best first).
pub fn filter_bookmarks(
    matcher: &SkimMatcherV2,
    bookmarks: &[Bookmark],
    query: &str,
    env_filter: Option<&str>,
) -> Vec<usize> {
    let mut scored: Vec<(usize, i64)> = bookmarks
        .iter()
        .enumerate()
        .filter(|(_, b)| {
            // Apply environment filter first
            if let Some(env) = env_filter
                && !b.env.eq_ignore_ascii_case(env)
            {
                return false;
            }
            true
        })
        .filter_map(|(i, b)| fuzzy_match_bookmark(matcher, b, query).map(|score| (i, score)))
        .collect();

    // Sort by score descending (best match first), then by index for stability
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    scored.into_iter().map(|(i, _)| i).collect()
}

/// Render the search bar at the given area.
pub fn render_search_bar(frame: &mut Frame, area: Rect, query: &str, active: bool) {
    let style = if active {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let cursor_char = if active { "_" } else { "" };
    let line = Line::from(vec![
        Span::styled(" / ", style),
        Span::styled(format!("{query}{cursor_char}"), style),
    ]);

    let widget = Paragraph::new(line);
    frame.render_widget(widget, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bookmark(name: &str, host: &str, env: &str, tags: &[&str]) -> Bookmark {
        Bookmark {
            name: name.into(),
            host: host.into(),
            user: Some("deploy".into()),
            port: 22,
            env: env.into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            identity_file: None,
            proxy_jump: None,
            notes: None,
            last_connected: None,
            connect_count: 0,
        }
    }

    #[test]
    fn test_empty_query_matches_all() {
        let matcher = SkimMatcherV2::default();
        let bookmarks = vec![
            make_bookmark("prod-web", "10.0.1.1", "production", &[]),
            make_bookmark("dev-api", "10.0.2.1", "development", &[]),
        ];

        let result = filter_bookmarks(&matcher, &bookmarks, "", None);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_fuzzy_match_by_name() {
        let matcher = SkimMatcherV2::default();
        let bookmarks = vec![
            make_bookmark("prod-web-01", "10.0.1.1", "production", &[]),
            make_bookmark("dev-api-01", "10.0.2.1", "development", &[]),
            make_bookmark("staging-web", "10.0.3.1", "staging", &[]),
        ];

        let result = filter_bookmarks(&matcher, &bookmarks, "web", None);
        assert!(result.len() >= 2); // prod-web-01 and staging-web should match
        // First result should be one of the "web" bookmarks
        let first_name = &bookmarks[result[0]].name;
        assert!(first_name.contains("web"));
    }

    #[test]
    fn test_fuzzy_match_by_host() {
        let matcher = SkimMatcherV2::default();
        let bookmarks = vec![
            make_bookmark("server-a", "192.168.1.10", "development", &[]),
            make_bookmark("server-b", "10.0.1.5", "production", &[]),
        ];

        let result = filter_bookmarks(&matcher, &bookmarks, "192.168", None);
        assert!(!result.is_empty());
        assert_eq!(bookmarks[result[0]].host, "192.168.1.10");
    }

    #[test]
    fn test_fuzzy_match_by_tags() {
        let matcher = SkimMatcherV2::default();
        let bookmarks = vec![
            make_bookmark("server-a", "10.0.1.1", "production", &["web", "frontend"]),
            make_bookmark("server-b", "10.0.1.2", "production", &["db", "postgres"]),
        ];

        let result = filter_bookmarks(&matcher, &bookmarks, "postgres", None);
        assert!(!result.is_empty());
        assert_eq!(bookmarks[result[0]].name, "server-b");
    }

    #[test]
    fn test_env_filter() {
        let matcher = SkimMatcherV2::default();
        let bookmarks = vec![
            make_bookmark("prod-web", "10.0.1.1", "production", &[]),
            make_bookmark("dev-api", "10.0.2.1", "development", &[]),
            make_bookmark("prod-db", "10.0.1.2", "production", &[]),
        ];

        let result = filter_bookmarks(&matcher, &bookmarks, "", Some("production"));
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|&i| bookmarks[i].env == "production"));
    }

    #[test]
    fn test_env_filter_combined_with_search() {
        let matcher = SkimMatcherV2::default();
        let bookmarks = vec![
            make_bookmark("prod-web", "10.0.1.1", "production", &[]),
            make_bookmark("dev-web", "10.0.2.1", "development", &[]),
            make_bookmark("prod-db", "10.0.1.2", "production", &[]),
        ];

        let result = filter_bookmarks(&matcher, &bookmarks, "web", Some("production"));
        assert_eq!(result.len(), 1);
        assert_eq!(bookmarks[result[0]].name, "prod-web");
    }

    #[test]
    fn test_no_match_returns_empty() {
        let matcher = SkimMatcherV2::default();
        let bookmarks = vec![make_bookmark("prod-web", "10.0.1.1", "production", &[])];

        let result = filter_bookmarks(&matcher, &bookmarks, "zzzzzzzzz", None);
        assert!(result.is_empty());
    }

    #[test]
    fn test_special_characters_in_query() {
        let matcher = SkimMatcherV2::default();
        let bookmarks = vec![make_bookmark("my-server.local", "127.0.0.1", "local", &[])];

        // Should not panic on special characters
        let result = filter_bookmarks(&matcher, &bookmarks, "server.local", None);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_case_insensitive_env_filter() {
        let matcher = SkimMatcherV2::default();
        let bookmarks = vec![
            make_bookmark("prod-web", "10.0.1.1", "production", &[]),
            make_bookmark("dev-api", "10.0.2.1", "development", &[]),
        ];

        let result = filter_bookmarks(&matcher, &bookmarks, "", Some("Production"));
        assert_eq!(result.len(), 1);
    }
}
