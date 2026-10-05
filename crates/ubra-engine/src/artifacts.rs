//! Extracts and classifies URLs (PRs, Linear issues, previews, plain links)
//! from a session's terminal.
//!
//! Input is a [`LinkSource`]: the ANSI-free text of the screen plus recent
//! scrollback with soft-wrapped rows already rejoined, and every OSC 8
//! hyperlink target (agents render `[title](url)` as a hyperlink whose URL is
//! never printed). Candidates are cut by hand rather than by one greedy regex
//! so that a URL is only ever what a URL can contain: the host must be a real
//! host name, surrounding prose and markdown punctuation is trimmed, and a
//! URL an application broke across rows at the terminal edge is put back
//! together. PR and Linear URLs reduce to one identity each, so `…/pull/7`,
//! `…/pull/7/files` and `…/pull/7#discussion` are one link.

use std::collections::HashMap;

use ubra_proto::{ArtifactKind, DateMillis, SessionArtifact};
use ubra_terminal_state::LinkSource;

pub const MAX_ARTIFACTS: usize = 50;

/// Hosts that appear in code and docs as identifiers, not as destinations.
const NOISE_HOSTS: &[&str] = &[
    "w3.org",
    "json-schema.org",
    "example.com",
    "example.org",
    "example.net",
    "schemas.openxmlformats.org",
    "schemas.microsoft.com",
    "purl.org",
    "xmlns.com",
];

/// Hosted preview deployments and tunnels, beyond localhost.
const PREVIEW_SUFFIXES: &[&str] = &[
    ".vercel.app",
    ".netlify.app",
    ".pages.dev",
    ".ngrok.io",
    ".ngrok.app",
    ".ngrok-free.app",
    ".ngrok-free.dev",
    ".trycloudflare.com",
    ".fly.dev",
    ".onrender.com",
    ".up.railway.app",
];

/// Where a URL may begin: a scheme, or a bare GitHub/Linear host, which
/// agents often print without one.
const STARTS: &[&str] = &["https://", "http://", "github.com/", "linear.app/"];

/// Scans `source`, merges with `existing` (preserving each link's original
/// `firstSeenAt`), dedupes by identity, and caps at [`MAX_ARTIFACTS`] — the
/// oldest entries drop first when over the cap.
pub fn scan(
    source: &LinkSource,
    existing: &[SessionArtifact],
    now: DateMillis,
) -> Vec<SessionArtifact> {
    // Printed URLs and hyperlink targets interleave in screen order, which
    // is the order the popover lists them in.
    let mut found: Vec<(usize, String)> = candidates(&source.text, source.cols).collect();
    found.extend(source.hyperlinks.iter().cloned());
    found.sort_by_key(|(at, _)| *at);
    let found = found
        .into_iter()
        .filter_map(|(_, raw)| classify(&raw))
        .map(|(kind, url)| SessionArtifact {
            kind,
            url,
            first_seen_at: now,
        });
    merge(existing.iter().cloned().chain(found))
}

/// Whether `source` could hold a link at all: most screens pay no more than
/// this substring check.
pub fn may_contain_links(source: &LinkSource) -> bool {
    !source.hyperlinks.is_empty()
        || source.text.contains("http")
        || source.text.contains("github.com")
        || source.text.contains("linear.app")
}

/// Unions artifact lists in order: a link keeps the first place and the
/// earliest `firstSeenAt` it was given, entries that are not valid links are
/// dropped, and the newest [`MAX_ARTIFACTS`] survive.
pub fn merge(artifacts: impl IntoIterator<Item = SessionArtifact>) -> Vec<SessionArtifact> {
    let mut result: Vec<SessionArtifact> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for artifact in artifacts {
        let Some((kind, url)) = classify(&artifact.url) else {
            continue;
        };
        let key = identity(kind, &url);
        match index.get(&key) {
            Some(&at) => {
                let prior = &mut result[at];
                if artifact.first_seen_at.0 < prior.first_seen_at.0 {
                    prior.first_seen_at = artifact.first_seen_at;
                }
                // A Linear link with its title slug reads better than the
                // bare key; both open the same issue.
                if kind == ArtifactKind::LinearIssue && url.len() > prior.url.len() {
                    prior.url = url;
                }
            }
            None => {
                index.insert(key, result.len());
                result.push(SessionArtifact {
                    kind,
                    url,
                    first_seen_at: artifact.first_seen_at,
                });
            }
        }
    }
    if result.len() > MAX_ARTIFACTS {
        // Keep the newest by first-seen time, stable for ties via order.
        let mut indexed: Vec<(usize, SessionArtifact)> = result.into_iter().enumerate().collect();
        indexed.sort_by(|a, b| {
            a.1.first_seen_at
                .0
                .partial_cmp(&b.1.first_seen_at.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        let mut kept = indexed.split_off(indexed.len() - MAX_ARTIFACTS);
        kept.sort_by_key(|(index, _)| *index);
        result = kept.into_iter().map(|(_, artifact)| artifact).collect();
    }
    result
}

/// The key two URLs share when they are the same destination.
fn identity(kind: ArtifactKind, url: &str) -> String {
    match kind {
        ArtifactKind::LinearIssue => {
            let key = url.split('/').nth(5).unwrap_or(url);
            format!("linear:{}", key.to_ascii_uppercase())
        }
        _ => url.trim_end_matches('/').to_owned(),
    }
}

/// Everything a URL may contain once it is cut out of prose: RFC 3986
/// characters minus the quotes, brackets, and backticks that delimit URLs in
/// code and markdown far more often than they appear inside one.
fn url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#@!$&()*+,;=%".contains(c)
}

/// Candidate URL strings in `text`, trimmed of trailing punctuation but not
/// yet validated. `cols` (0 when unknown) lets a URL that fills a row to the
/// terminal's edge continue on the next row, which is how applications that
/// wrap text themselves break long URLs.
fn candidates(text: &str, cols: usize) -> impl Iterator<Item = (usize, String)> + '_ {
    let lower = text.to_ascii_lowercase();
    let mut at = 0;
    std::iter::from_fn(move || {
        loop {
            let start = next_start(&lower, at)?;
            let (raw, end) = extend(text, start, cols);
            at = end.max(start + 1);
            if let Some(url) = trim(&raw) {
                return Some((start, url.to_owned()));
            }
        }
    })
}

/// The byte offset of the next URL start at or after `from`. A bare host
/// counts only at a word boundary (optionally behind `www.`), so it never
/// restarts inside a URL or a longer host name.
fn next_start(lower: &str, from: usize) -> Option<usize> {
    let bytes = lower.as_bytes();
    // Only a character that could extend the host or path disqualifies:
    // `(github.com/…` starts a link, `notgithub.com/…` does not.
    let boundary = |at: usize| {
        at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || b"-._/@".contains(&bytes[at - 1]))
    };
    let mut search = from;
    loop {
        let (start, needle) = STARTS
            .iter()
            .filter_map(|needle| {
                lower[search..]
                    .find(needle)
                    .map(|at| (search + at, *needle))
            })
            .min_by_key(|(at, _)| *at)?;
        if needle.starts_with("http") || boundary(start) {
            return Some(start);
        }
        if lower[..start].ends_with("www.") && boundary(start - 4) {
            return Some(start - 4);
        }
        search = start + needle.len();
    }
}

/// The URL-character run from `start`, following hand-wrapped continuations.
/// Returns the run and the byte offset scanning resumes from.
fn extend(text: &str, start: usize, cols: usize) -> (String, usize) {
    let mut url = String::new();
    let mut at = start;
    loop {
        let run_end = text[at..]
            .find(|c: char| !url_char(c))
            .map_or(text.len(), |offset| at + offset);
        url.push_str(&text[at..run_end]);
        match continuation(text, run_end, cols) {
            Some(next) => at = next,
            None => return (url, run_end),
        }
    }
}

/// When the row ending at `end` was filled to the terminal's edge by a URL
/// and the next row carries on with the rest of that token (after
/// indentation or a box border), where the continuation starts.
fn continuation(text: &str, end: usize, cols: usize) -> Option<usize> {
    if cols < 20 || !text[end..].starts_with('\n') {
        return None;
    }
    let line_start = text[..end].rfind('\n').map_or(0, |at| at + 1);
    // A logical line may be several soft-wrapped rows; only its last row can
    // be the one that was filled by hand.
    let width = text[line_start..end].chars().count();
    let last_row = if width == 0 {
        0
    } else {
        (width - 1) % cols + 1
    };
    if last_row + 4 < cols {
        return None;
    }
    let next = end + 1;
    let body = next
        + text[next..]
            .char_indices()
            .find(|(_, c)| !matches!(c, ' ' | '│' | '┃' | '|' | '>'))
            .map(|(offset, _)| offset)?;
    let fragment_end = text[body..]
        .find(|c: char| !url_char(c))
        .map_or(text.len(), |offset| body + offset);
    let fragment = &text[body..fragment_end];
    let fragment_lower = fragment.to_ascii_lowercase();
    if fragment.is_empty() || STARTS.iter().any(|s| fragment_lower.starts_with(s)) {
        return None;
    }
    // The rest of a URL either ends its row or looks like URL, never like the
    // first word of a sentence.
    let rest_of_row = text[fragment_end..].split('\n').next().unwrap_or_default();
    let alone = rest_of_row.trim_matches([' ', '│', '┃']).is_empty();
    let url_shaped = fragment.contains(['/', '.', '?', '=', '&', '%', '#', '_', '-']);
    (alone || url_shaped).then_some(body)
}

/// Strips trailing punctuation that closes the sentence or markdown around a
/// URL, and a closing parenthesis the URL itself never opened.
fn trim(raw: &str) -> Option<&str> {
    let mut url = raw;
    while let Some(last) = url.chars().last() {
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '*' | '(' => true,
            ')' => url.matches('(').count() < url.matches(')').count(),
            _ => false,
        };
        if !strip {
            break;
        }
        url = &url[..url.len() - 1];
    }
    (!url.is_empty()).then_some(url)
}

/// Validates a candidate and returns its kind and canonical URL: a scheme is
/// guaranteed, scheme and host are lowercase, and PR/Linear links are reduced
/// to the part that names the item.
pub fn classify(raw: &str) -> Option<(ArtifactKind, String)> {
    let raw = trim(raw.trim())?;
    let lower = raw.to_ascii_lowercase();
    let (scheme, rest) = if lower.starts_with("https://") {
        ("https", &raw["https://".len()..])
    } else if lower.starts_with("http://") {
        ("http", &raw["http://".len()..])
    } else {
        ("https", raw)
    };
    if !rest.chars().all(url_char) {
        return None;
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    // Credentials never belong in a link list.
    if authority.contains('@') {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if port.is_some_and(|port| {
        port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit())
    }) {
        return None;
    }
    let host = host.to_ascii_lowercase();
    if !valid_host(&host) || noise_host(&host) {
        return None;
    }
    let path: Vec<&str> = tail
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();

    if matches!(host.as_str(), "github.com" | "www.github.com")
        && let [owner, repo, "pull" | "pulls", number, ..] = path.as_slice()
        && !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
        && valid_segment(owner)
        && valid_segment(repo)
    {
        return Some((
            ArtifactKind::PullRequest,
            format!("https://github.com/{owner}/{repo}/pull/{number}"),
        ));
    }
    if host == "linear.app"
        && let [workspace, "issue", key, slug @ ..] = path.as_slice()
        && valid_segment(workspace)
        && linear_key(key)
    {
        let mut url = format!(
            "https://linear.app/{workspace}/issue/{}",
            key.to_ascii_uppercase()
        );
        if let Some(slug) = slug.first().filter(|slug| valid_segment(slug)) {
            url.push('/');
            url.push_str(slug);
        }
        return Some((ArtifactKind::LinearIssue, url));
    }
    let kind = if local_host(&host) || PREVIEW_SUFFIXES.iter().any(|s| host.ends_with(s)) {
        ArtifactKind::Preview
    } else {
        ArtifactKind::Link
    };
    let port = port.map(|port| format!(":{port}")).unwrap_or_default();
    Some((kind, format!("{scheme}://{host}{port}{tail}")))
}

fn local_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0") || host.ends_with(".localhost")
}

/// `localhost`, a dotted IPv4 address, or a DNS name with an alphabetic TLD.
/// This is what rejects code that merely starts like a URL: `https://${host}`,
/// `https://[^\s]+`, `https://...`, `http://<your-app>`.
fn valid_host(host: &str) -> bool {
    if local_host(host) {
        return true;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() == 4 && labels.iter().all(|label| label.parse::<u8>().is_ok()) {
        return true;
    }
    labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        && labels
            .last()
            .is_some_and(|tld| tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_alphabetic()))
}

fn noise_host(host: &str) -> bool {
    NOISE_HOSTS.iter().any(|noise| {
        host == *noise
            || host
                .strip_suffix(noise)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// `ENG-123`: a team key starting with a letter, a dash, and a number.
fn linear_key(key: &str) -> bool {
    key.split_once('-').is_some_and(|(team, number)| {
        team.starts_with(|c: char| c.is_ascii_alphabetic())
            && team.bytes().all(|b| b.is_ascii_alphanumeric())
            && !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(text: &str, cols: usize, hyperlinks: &[&str]) -> Vec<SessionArtifact> {
        scan(
            &LinkSource {
                text: text.to_owned(),
                hyperlinks: hyperlinks
                    .iter()
                    .map(|uri| (text.len(), (*uri).to_owned()))
                    .collect(),
                cols,
            },
            &[],
            DateMillis(1000.0),
        )
    }

    fn scan_fresh(text: &str) -> Vec<SessionArtifact> {
        source(text, 0, &[])
    }

    fn urls(text: &str) -> Vec<String> {
        urls_in(text, 0)
    }

    fn urls_in(text: &str, cols: usize) -> Vec<String> {
        source(text, cols, &[]).into_iter().map(|a| a.url).collect()
    }

    fn at(text: &str, existing: &[SessionArtifact], now: f64) -> Vec<SessionArtifact> {
        let source = LinkSource {
            text: text.to_owned(),
            ..LinkSource::default()
        };
        scan(&source, existing, DateMillis(now))
    }

    #[test]
    fn urls_classify_specific_over_generic() {
        let text = "PR at https://github.com/o/r/pull/42 and docs https://docs.dev/page \
                    with preview http://localhost:3000/app and linear.app/team/issue/ABC-7";
        let found = scan_fresh(text);
        let kind_of = |needle: &str| {
            found
                .iter()
                .find(|artifact| artifact.url.contains(needle))
                .map(|artifact| artifact.kind)
        };
        assert_eq!(kind_of("pull/42"), Some(ArtifactKind::PullRequest));
        assert_eq!(kind_of("docs.dev"), Some(ArtifactKind::Link));
        assert_eq!(kind_of("localhost:3000"), Some(ArtifactKind::Preview));
        assert_eq!(kind_of("ABC-7"), Some(ArtifactKind::LinearIssue));
        assert_eq!(found.len(), 4, "{found:?}");
    }

    #[test]
    fn trailing_punctuation_is_stripped_and_schemes_added() {
        assert_eq!(
            urls("see (github.com/o/r/pull/9)."),
            ["https://github.com/o/r/pull/9"]
        );
        assert_eq!(
            urls("Opened **https://github.com/o/r/pull/10**!"),
            ["https://github.com/o/r/pull/10"]
        );
        assert_eq!(
            urls("docs: <https://docs.rs/gpui/latest/gpui/>, then `https://crates.io/x`"),
            ["https://docs.rs/gpui/latest/gpui/", "https://crates.io/x"]
        );
        assert_eq!(
            urls("https://en.wikipedia.org/wiki/Rust_(programming_language))"),
            ["https://en.wikipedia.org/wiki/Rust_(programming_language)"]
        );
    }

    #[test]
    fn markdown_and_quoted_urls_end_at_their_delimiters() {
        assert_eq!(
            urls("[the PR](https://github.com/o/r/pull/3) and 'https://a.dev/x','https://b.dev/y'"),
            [
                "https://github.com/o/r/pull/3",
                "https://a.dev/x",
                "https://b.dev/y"
            ]
        );
        assert_eq!(
            urls("href=\"https://a.dev/page?x=1&y=2#top\">"),
            ["https://a.dev/page?x=1&y=2#top"]
        );
    }

    #[test]
    fn code_that_only_looks_like_a_url_is_rejected() {
        let text = r#"
            let re = Regex::new(r"https?://[^\s"'\)\]]+"); let x = "https://[^\s";
            fetch(`https://${host}/api`) or http://<your-app> or https://... or
            https://example.com/demo https://www.w3.org/2000/svg https://user:pw@db.dev
            https://-bad-.com https://localhost:abc https://foo
        "#;
        assert!(urls(text).is_empty(), "{:?}", urls(text));
    }

    #[test]
    fn one_pull_request_is_one_link_whatever_page_it_names() {
        let found = scan_fresh(
            "https://github.com/o/r/pull/7 https://github.com/o/r/pull/7/files \
             https://GitHub.com/o/r/pull/7#issuecomment-1 github.com/o/r/pull/7/checks \
             www.github.com/o/r/pull/7",
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].url, "https://github.com/o/r/pull/7");
        assert_eq!(found[0].kind, ArtifactKind::PullRequest);
    }

    #[test]
    fn bare_hosts_only_start_at_a_word_boundary() {
        assert!(urls("notgithub.com/o/r/pull/1").is_empty());
        assert_eq!(
            urls("(www.github.com/o/r/pull/2)"),
            ["https://github.com/o/r/pull/2"]
        );
    }

    #[test]
    fn linear_links_dedupe_by_key_and_keep_the_title_slug() {
        let found = scan_fresh(
            "linear.app/acme/issue/prd-7868 and \
             https://linear.app/acme/issue/PRD-7868/welcome-to-anara?utm=1",
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].url,
            "https://linear.app/acme/issue/PRD-7868/welcome-to-anara"
        );
    }

    #[test]
    fn previews_cover_hosted_deploys_and_tunnels() {
        for url in [
            "http://127.0.0.1:5173",
            "http://app.localhost:3000/x",
            "https://my-app-git-main.vercel.app",
            "https://abc.ngrok-free.app/hook",
            "https://deploy-preview-3--site.netlify.app",
            "https://x.trycloudflare.com",
        ] {
            assert_eq!(
                scan_fresh(url).first().map(|a| a.kind),
                Some(ArtifactKind::Preview),
                "{url}"
            );
        }
    }

    #[test]
    fn hyperlink_targets_are_links_even_when_never_printed() {
        let found = source(
            "Opened PR #8123 for review\n",
            80,
            &[
                "https://github.com/anaralabs/anara/pull/8123",
                "file:///Users/me/src/main.rs",
                "https://github.com/anaralabs/anara/pull/8123/files",
            ],
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].kind, ArtifactKind::PullRequest);
    }

    #[test]
    fn hyperlinks_and_printed_urls_keep_screen_order() {
        let text = "first https://a.dev/1 then PR #9 then https://b.dev/2\n";
        let found = scan(
            &LinkSource {
                text: text.to_owned(),
                hyperlinks: vec![(
                    text.find("PR #9").unwrap(),
                    "https://github.com/o/r/pull/9".into(),
                )],
                cols: 80,
            },
            &[],
            DateMillis(1.0),
        );
        let urls: Vec<_> = found.iter().map(|a| a.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://a.dev/1",
                "https://github.com/o/r/pull/9",
                "https://b.dev/2"
            ]
        );
    }

    #[test]
    fn a_url_broken_at_the_terminal_edge_is_rejoined() {
        let cols = 40;
        let url = "https://static.anara.com/images/emails/new-campaign/anara-hero.png";
        let (first, rest) = url.split_at(cols - 2);
        let text = format!("  {first}\n  {rest} done\n");
        assert_eq!(urls_in(&text, cols), [url]);
        // Inside a bordered box the continuation starts after the border.
        let text = format!("│ {first}\n│ {rest}\n");
        assert_eq!(urls_in(&text, cols), [url]);
        // A short URL followed by an unrelated line is left alone.
        let text = "see https://a.dev/x\nnext line\n";
        assert_eq!(urls_in(text, cols), ["https://a.dev/x"]);
        // A full row followed by prose is not a continuation.
        let text = format!("  {first}\n  and then some words\n");
        assert_eq!(urls_in(&text, cols), [first.to_owned()]);
        // Nor is a second URL.
        let text = format!("  {first}\n  https://b.dev/y\n");
        assert_eq!(urls_in(&text, cols), [first, "https://b.dev/y"]);
    }

    #[test]
    fn existing_artifacts_keep_their_first_seen_time() {
        let first = at("https://github.com/o/r/pull/1", &[], 500.0);
        let merged = at("https://github.com/o/r/pull/1/files again", &first, 900.0);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].first_seen_at, DateMillis(500.0));
    }

    #[test]
    fn merging_drops_links_an_older_scanner_misread() {
        let stale = |url: &str| SessionArtifact {
            kind: ArtifactKind::Link,
            url: url.into(),
            first_seen_at: DateMillis(1.0),
        };
        let merged = merge([
            stale("https://[^\\s"),
            stale("https://github.com/o/r/pull/5/files"),
            stale("https://docs.rs/x"),
        ]);
        let urls: Vec<_> = merged.iter().map(|a| a.url.as_str()).collect();
        assert_eq!(urls, ["https://github.com/o/r/pull/5", "https://docs.rs/x"]);
        assert_eq!(merged[0].kind, ArtifactKind::PullRequest);
    }

    #[test]
    fn the_cap_drops_oldest_first() {
        let existing: Vec<_> = (0..MAX_ARTIFACTS)
            .map(|n| SessionArtifact {
                kind: ArtifactKind::Link,
                url: format!("https://docs.dev/{n}"),
                first_seen_at: DateMillis(n as f64),
            })
            .collect();
        let result = at("https://docs.dev/newest", &existing, 9999.0);
        assert_eq!(result.len(), MAX_ARTIFACTS);
        assert!(result.iter().any(|a| a.url.ends_with("newest")));
        assert!(
            !result.iter().any(|a| a.url.ends_with("/0")),
            "the oldest entry is the one dropped"
        );
    }
}
