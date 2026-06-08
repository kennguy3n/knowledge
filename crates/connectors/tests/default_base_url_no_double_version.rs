//! Corpus-wide regression guard for the "double version segment" connector bug.
//!
//! Several regional connectors (PR #184) shipped a `DEFAULT_API_BASE_URL` that
//! already carried an API version (e.g. `https://api.brevo.com/v3`) while their
//! request-path builders *also* hardcoded `/v1/<resource>`. Under the shipped
//! default config that composes an unreachable, double-versioned URL such as
//! `https://api.brevo.com/v3/v1/contacts`, which 404s in production so the
//! connector can never sync. It went unnoticed because every connector unit
//! test overrides `api_base_url` with a version-less test host
//! (`https://api.test/<name>`), so the real default URL is never exercised.
//!
//! This test closes that blind spot at the corpus level: it scans every
//! connector source file, composes the request URL from the *shipped*
//! `DEFAULT_API_BASE_URL` and each `{base_url}/...` path template, and asserts
//! no composed URL contains more than one API version segment. Any future
//! connector that reintroduces the pattern fails here, regardless of whether
//! its own unit tests override the base URL.

use std::fs;
use std::path::Path;

/// A path segment is "version-like" if it is an optional `v`/`V` followed by a
/// dotted numeric token: `v1`, `v2`, `2.0`, `v3.3`, `v2.01`, `1.0`, ...
fn is_version_segment(seg: &str) -> bool {
    let s = seg
        .strip_prefix('v')
        .or_else(|| seg.strip_prefix('V'))
        .unwrap_or(seg);
    if s.is_empty() {
        return false;
    }
    let mut has_digit = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            has_digit = true;
        } else if ch != '.' {
            return false;
        }
    }
    has_digit
}

fn count_version_segments(path: &str) -> usize {
    path.split('/')
        .filter(|s| !s.is_empty() && is_version_segment(s))
        .count()
}

/// Value of the `DEFAULT_API_BASE_URL` string constant, if the file declares one.
fn default_base_url(src: &str) -> Option<String> {
    let idx = src.find("DEFAULT_API_BASE_URL")?;
    let rest = &src[idx..];
    let eq = rest.find('=')?;
    let after = &rest[eq + 1..];
    let q1 = after.find('"')?;
    let after2 = &after[q1 + 1..];
    let q2 = after2.find('"')?;
    Some(after2[..q2].to_string())
}

/// The path portion of a URL (everything after `scheme://host`).
fn url_path(url: &str) -> &str {
    if let Some(pos) = url.find("://") {
        let after = &url[pos + 3..];
        return match after.find('/') {
            Some(slash) => &after[slash..],
            None => "",
        };
    }
    url
}

/// Every literal path string that immediately follows `{base_url}/` in the
/// source (the path template feeding `format!("{base_url}/...")`). Reading stops
/// at the first interpolation/query/quote/whitespace boundary.
fn base_url_path_literals(src: &str) -> Vec<String> {
    let needle = "{base_url}/";
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(pos) = src[start..].find(needle) {
        let abs = start + pos + needle.len();
        let mut lit = String::new();
        for ch in src[abs..].chars() {
            if "{}?\"' \n\t#&".contains(ch) {
                break;
            }
            lit.push(ch);
        }
        out.push(lit);
        start = abs;
    }
    out
}

#[test]
fn no_connector_composes_a_double_version_url() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();

    for entry in fs::read_dir(&src_dir).expect("read connectors src dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = fs::read_to_string(&path).expect("read connector source");
        let Some(base) = default_base_url(&src) else {
            continue;
        };
        let base_versions = count_version_segments(url_path(&base));

        for lit in base_url_path_literals(&src) {
            let composed = base_versions + count_version_segments(&lit);
            if composed > 1 {
                violations.push(format!(
                    "{}: DEFAULT_API_BASE_URL `{}` + path template `{{base_url}}/{}` \
                     composes {} version segments (must be <= 1). Keep the API \
                     version in exactly one place: either the base URL or the \
                     request path, never both.",
                    path.file_name().unwrap().to_string_lossy(),
                    base,
                    lit,
                    composed,
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Connector(s) compose a double-version request URL:\n{}",
        violations.join("\n"),
    );
}
