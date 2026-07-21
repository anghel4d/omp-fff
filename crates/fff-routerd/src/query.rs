use std::path::{Component, Path};

pub fn normalize_path_constraint(path_constraint: &str, base: &Path) -> Result<Option<String>, String> {
    let mut trimmed = path_constraint.trim().to_string();
    if trimmed.is_empty() {
        return Ok(Some(trimmed));
    }
    if Path::new(&trimmed).is_absolute() {
        let relative = lexical_relative(base, Path::new(&trimmed)).ok_or_else(|| {
            format!("Path constraint must be inside the indexed root: {path_constraint}")
        })?;
        if relative.is_empty() {
            return Ok(None);
        }
        trimmed = relative;
    }
    if trimmed == "." || trimmed == "./" {
        return Ok(None);
    }
    if let Some(rest) = trimmed.strip_prefix("./") {
        trimmed = rest.to_string();
    }
    if let Some(dir) = trimmed
        .strip_suffix("/**/*")
        .or_else(|| trimmed.strip_suffix("/**"))
    {
        if !dir.is_empty() && !has_glob(dir) {
            return Ok(Some(format!("{dir}/")));
        }
    }
    if trimmed.starts_with('/') || trimmed.ends_with('/') || has_glob(&trimmed) {
        return Ok(Some(trimmed));
    }
    let last = trimmed.rsplit('/').next().unwrap_or_default();
    if filename_extension_constraint(last) {
        return Ok(Some(trimmed));
    }
    Ok(Some(format!("{trimmed}/")))
}

pub fn normalize_excludes(excludes: &[String], base: &Path) -> Result<Vec<String>, String> {
    let mut output = Vec::new();
    for raw in excludes {
        for piece in raw.split(|c: char| c == ',' || c.is_whitespace()) {
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            let stripped = piece.strip_prefix('!').unwrap_or(piece);
            if let Some(normalized) = normalize_path_constraint(stripped, base)? {
                if !normalized.is_empty() {
                    output.push(format!("!{normalized}"));
                }
            }
        }
    }
    Ok(output)
}

pub fn build_query(
    path_constraint: Option<&str>,
    pattern: &str,
    excludes: &[String],
    base: &Path,
) -> Result<String, String> {
    let mut parts = Vec::new();
    if let Some(path_constraint) = path_constraint {
        if let Some(normalized) = normalize_path_constraint(path_constraint, base)? {
            if !normalized.is_empty() {
                parts.push(normalized);
            }
        }
    }
    parts.extend(normalize_excludes(excludes, base)?);
    if !pattern.is_empty() {
        parts.push(pattern.to_string());
    }
    Ok(parts.join(" "))
}

pub fn build_constraints(
    path_constraint: Option<&str>,
    excludes: &[String],
    verbatim: &[String],
    base: &Path,
) -> Result<Option<String>, String> {
    let mut parts = Vec::new();
    if let Some(path_constraint) = path_constraint {
        if let Some(normalized) = normalize_path_constraint(path_constraint, base)? {
            if !normalized.is_empty() {
                parts.push(normalized);
            }
        }
    }
    parts.extend(normalize_excludes(excludes, base)?);
    parts.extend(verbatim.iter().filter(|v| !v.is_empty()).cloned());
    Ok((!parts.is_empty()).then(|| parts.join(" ")))
}

fn has_glob(value: &str) -> bool {
    value.bytes().any(|b| matches!(b, b'*' | b'?' | b'[' | b'{'))
}

fn filename_extension_constraint(last: &str) -> bool {
    let Some(dot) = last.rfind('.') else { return false };
    let ext = &last[dot + 1..];
    (1..=10).contains(&ext.len())
        && ext.as_bytes()[0].is_ascii_alphabetic()
        && ext.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn lexical_relative(base: &Path, candidate: &Path) -> Option<String> {
    let base = normalize_components(base)?;
    let candidate = normalize_components(candidate)?;
    if candidate.len() < base.len() || candidate[..base.len()] != base {
        return None;
    }
    Some(candidate[base.len()..].join("/"))
}

fn normalize_components(path: &Path) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => out.push(value.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop()?;
            }
            Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_matches_router_table() {
        let base = Path::new("/repo");
        let cases = [
            ("src", Some("src/")),
            ("src/", Some("src/")),
            ("*.rs", Some("*.rs")),
            ("src/**", Some("src/")),
            ("src/**/*", Some("src/")),
            ("src/main.rs", Some("src/main.rs")),
            ("/repo/src", Some("src/")),
            ("/repo", None),
            (".", None),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_path_constraint(input, base).unwrap().as_deref(), expected);
        }
        assert_eq!(
            normalize_path_constraint("/other", base).unwrap_err(),
            "Path constraint must be inside the indexed root: /other"
        );
    }

    #[test]
    fn query_order_and_excludes_match_router() {
        let base = Path::new("/repo");
        let excludes = vec!["!test, generated/ tmp".to_string()];
        assert_eq!(
            build_query(Some("src"), "needle", &excludes, base).unwrap(),
            "src/ !test/ !generated/ !tmp/ needle"
        );
        assert_eq!(
            build_constraints(Some("src"), &excludes, &["*.rs".into()], base).unwrap(),
            Some("src/ !test/ !generated/ !tmp/ *.rs".into())
        );
    }
}
