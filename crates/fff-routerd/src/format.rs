#[derive(Clone, Debug, Default)]
pub struct AnnotationData {
    pub git_status: Option<String>,
    pub total_frecency_score: i32,
    pub access_frecency_score: i32,
}

#[derive(Clone, Debug)]
pub struct GrepLine {
    pub relative_path: String,
    pub annotation: AnnotationData,
    pub line_number: u64,
    pub line_content: String,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct FindItem {
    pub relative_path: String,
    pub annotation: AnnotationData,
}

pub fn truncate(line: &str, max: usize) -> String {
    let value = line.trim();
    if value.chars().count() <= max {
        return value.to_string();
    }
    let prefix = value.chars().take(max).collect::<String>();
    format!("{prefix}...")
}

pub fn annotation(item: &AnnotationData) -> String {
    if let Some(git) = item.git_status.as_deref() {
        if !matches!(git, "clean" | "unknown" | "") {
            return format!(" [{git} in git]");
        }
    }
    let score = if item.total_frecency_score != 0 {
        item.total_frecency_score
    } else {
        item.access_frecency_score
    };
    if score >= 25 {
        " [very often touched]".into()
    } else if score >= 20 {
        " [often touched]".into()
    } else {
        String::new()
    }
}

pub fn format_grep(root: &str, matches: &[GrepLine]) -> String {
    if matches.is_empty() {
        return "No matches found".into();
    }
    let mut lines = Vec::new();
    let mut current = "";
    for item in matches {
        if item.relative_path != current {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            current = &item.relative_path;
            lines.push(format!("{}{}", absolute(root, current), annotation(&item.annotation)));
        }
        let before_len = item.context_before.len() as u64;
        for (index, line) in item.context_before.iter().enumerate() {
            lines.push(format!(
                " {}- {}",
                item.line_number - before_len + index as u64,
                truncate(line, 500)
            ));
        }
        lines.push(format!(" {}: {}", item.line_number, truncate(&item.line_content, 500)));
        for (index, line) in item.context_after.iter().enumerate() {
            lines.push(format!(
                " {}- {}",
                item.line_number + 1 + index as u64,
                truncate(line, 500)
            ));
        }
    }
    lines.join("\n")
}

pub fn format_find(
    root: &str,
    items: &[FindItem],
    scores: &[i32],
    limit: usize,
    pattern: &str,
) -> (String, bool, usize) {
    if items.is_empty() {
        return ("No files found matching pattern".into(), false, 0);
    }
    let top_score = scores.first().copied().unwrap_or(0);
    let weak = top_score < (pattern.chars().count() as i32 * 12 / 2);
    let shown_count = items.len().min(if weak { limit.min(5) } else { limit });
    let text = items[..shown_count]
        .iter()
        .map(|item| format!("{}{}", absolute(root, &item.relative_path), annotation(&item.annotation)))
        .collect::<Vec<_>>()
        .join("\n");
    (text, weak, shown_count)
}

fn absolute(root: &str, relative: &str) -> String {
    if relative.is_empty() {
        root.to_string()
    } else {
        format!("{}/{}", root.trim_end_matches('/'), relative.replace('\\', "/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotation_precedence_matches_router() {
        assert_eq!(annotation(&AnnotationData { git_status: Some("modified".into()), total_frecency_score: 30, access_frecency_score: 0 }), " [modified in git]");
        assert_eq!(annotation(&AnnotationData { git_status: Some("clean".into()), total_frecency_score: 25, access_frecency_score: 0 }), " [very often touched]");
        assert_eq!(annotation(&AnnotationData { git_status: None, total_frecency_score: 20, access_frecency_score: 0 }), " [often touched]");
    }

    #[test]
    fn truncation_is_character_safe() {
        assert_eq!(truncate("  abc  ", 500), "abc");
        assert_eq!(truncate("ééé", 2), "éé...");
    }

    #[test]
    fn weak_find_caps_at_five() {
        let items = (0..8)
            .map(|i| FindItem { relative_path: format!("{i}"), annotation: AnnotationData::default() })
            .collect::<Vec<_>>();
        let (_, weak, count) = format_find("/r", &items, &[0], 30, "long");
        assert!(weak);
        assert_eq!(count, 5);
    }
}
