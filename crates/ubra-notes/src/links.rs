//! Links to the tools a team works in, recognised so a note can show them as
//! chips: a Notion page, a Google Doc or Sheet, a Linear issue, a HubSpot
//! deal, a Figma file, a Slack thread, a GitHub pull request, an analytics
//! dashboard. Ubra's notes are read by marketers, ops and PMs as much as by
//! developers, so these are first-class, not a GitHub special case.
//!
//! Recognition is by host and path only: nothing is fetched. A link stays an
//! ordinary Markdown link; the chip is presentation.

/// The tool a link opens, which picks the chip's glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Service {
    Notion,
    GoogleDocs,
    GoogleSheets,
    GoogleSlides,
    GoogleDrive,
    Linear,
    HubSpot,
    Figma,
    Slack,
    GitHub,
    /// Analytics and BI: Looker Studio, Google Analytics, Amplitude,
    /// Mixpanel, PostHog, Tableau, Metabase, Grafana, Datadog, Power BI.
    Dashboard,
}

/// A recognised link: its tool, the tool's name as people say it, and a
/// readable title for a link pasted without one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recognized {
    pub service: Service,
    pub name: &'static str,
    pub title: String,
}

struct Url<'a> {
    host: String,
    segments: Vec<&'a str>,
}

fn split(url: &str) -> Option<Url<'_>> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..end].to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_owned();
    let host = host.split(':').next().unwrap_or_default().to_owned();
    if host.is_empty() {
        return None;
    }
    let path = &rest[end..];
    let path = &path[..path.find(['?', '#']).unwrap_or(path.len())];
    let segments = path.split('/').filter(|s| !s.is_empty()).collect();
    Some(Url { host, segments })
}

fn host_is(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Recognises `url`, or `None` for anything that is not a known tool.
pub fn recognize(url: &str) -> Option<Recognized> {
    let u = split(url)?;
    let seg = |i: usize| u.segments.get(i).copied().unwrap_or_default();
    let found = |service, name: &'static str, title: String| {
        Some(Recognized {
            service,
            name,
            title,
        })
    };
    let host = u.host.as_str();

    if host_is(host, "notion.so") || host_is(host, "notion.site") {
        let title = u
            .segments
            .last()
            .map(|s| slug_title(strip_trailing_id(s)))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "Notion page".into());
        return found(Service::Notion, "Notion", title);
    }
    if host == "docs.google.com" {
        return match seg(0) {
            "document" => found(Service::GoogleDocs, "Google Docs", "Google Doc".into()),
            "spreadsheets" => found(
                Service::GoogleSheets,
                "Google Sheets",
                "Google Sheet".into(),
            ),
            "presentation" => found(
                Service::GoogleSlides,
                "Google Slides",
                "Google Slides".into(),
            ),
            "forms" => found(Service::GoogleDocs, "Google Forms", "Google Form".into()),
            _ => found(
                Service::GoogleDrive,
                "Google Drive",
                "Google Drive file".into(),
            ),
        };
    }
    if host == "drive.google.com" {
        let title = if seg(1) == "folders" || seg(0) == "drive" {
            "Google Drive folder"
        } else {
            "Google Drive file"
        };
        return found(Service::GoogleDrive, "Google Drive", title.into());
    }
    if host == "linear.app" {
        let title = match seg(1) {
            "issue" if !seg(2).is_empty() => {
                let key = seg(2).to_ascii_uppercase();
                let words = slug_title(seg(3));
                if words.is_empty() {
                    key
                } else {
                    format!("{key} {words}")
                }
            }
            "project" if !seg(2).is_empty() => slug_title(strip_trailing_id(seg(2))),
            _ => String::new(),
        };
        let title = if title.is_empty() {
            "Linear".into()
        } else {
            title
        };
        return found(Service::Linear, "Linear", title);
    }
    if host_is(host, "hubspot.com") {
        // Every CRM record lives under `/contacts/{portal}/…`, deals and
        // companies included, so the object type is read after that prefix.
        let object = u.segments.iter().skip(1).find_map(|s| match *s {
            "0-1" | "contact" | "contacts" => Some("contact"),
            "0-2" | "company" | "companies" => Some("company"),
            "0-3" | "deal" | "deals" => Some("deal"),
            "0-5" | "ticket" | "tickets" => Some("ticket"),
            "reports" | "reports-dashboard" | "dashboard" => Some("dashboard"),
            _ => None,
        });
        let title = object.map_or_else(|| "HubSpot".into(), |o| format!("HubSpot {o}"));
        return found(Service::HubSpot, "HubSpot", title);
    }
    if host_is(host, "figma.com") {
        let title = match seg(0) {
            "file" | "design" | "proto" | "board" | "slides" | "deck" => slug_title(seg(2)),
            _ => String::new(),
        };
        let title = if title.is_empty() {
            "Figma file".into()
        } else {
            title
        };
        return found(Service::Figma, "Figma", title);
    }
    if host_is(host, "slack.com") {
        let title = if seg(0) == "archives" {
            if seg(2).starts_with('p') {
                "Slack message"
            } else {
                "Slack channel"
            }
        } else {
            "Slack"
        };
        return found(Service::Slack, "Slack", title.into());
    }
    if host == "github.com" && !seg(0).is_empty() && !seg(1).is_empty() {
        let repo = format!("{}/{}", seg(0), seg(1));
        let title = match (seg(2), seg(3)) {
            ("pull" | "issues", n) if !n.is_empty() => format!("{repo}#{n}"),
            _ => repo,
        };
        return found(Service::GitHub, "GitHub", title);
    }
    let dashboard = if host == "lookerstudio.google.com" || host == "datastudio.google.com" {
        Some(("Looker Studio", "report"))
    } else if host == "analytics.google.com" {
        Some(("Google Analytics", "report"))
    } else if host_is(host, "amplitude.com") {
        Some(("Amplitude", "chart"))
    } else if host_is(host, "mixpanel.com") {
        Some(("Mixpanel", "report"))
    } else if host_is(host, "posthog.com") {
        Some(("PostHog", "dashboard"))
    } else if host_is(host, "tableau.com") {
        Some(("Tableau", "dashboard"))
    } else if host_is(host, "looker.com") || host_is(host, "cloud.looker.com") {
        Some(("Looker", "dashboard"))
    } else if host_is(host, "datadoghq.com") || host_is(host, "datadoghq.eu") {
        Some(("Datadog", "dashboard"))
    } else if host == "app.powerbi.com" {
        Some(("Power BI", "report"))
    } else if host.split('.').any(|part| part == "metabase") {
        Some(("Metabase", "dashboard"))
    } else if host.split('.').any(|part| part == "grafana") || host_is(host, "grafana.net") {
        Some(("Grafana", "dashboard"))
    } else {
        None
    };
    dashboard.and_then(|(name, what)| found(Service::Dashboard, name, format!("{name} {what}")))
}

/// `Q4-campaign-brief` → `Q4 campaign brief`: words from a URL slug, with
/// the first letter raised if the slug was all lower case.
fn slug_title(slug: &str) -> String {
    let words: Vec<String> = slug
        .split(['-', '_', '+'])
        .filter(|w| !w.is_empty())
        .map(percent_decode)
        .collect();
    let mut title = words.join(" ");
    if let Some(first) = title.chars().next()
        && first.is_lowercase()
    {
        title.replace_range(..first.len_utf8(), &first.to_uppercase().to_string());
    }
    title
}

/// Drops a trailing id the way Notion and Linear append one to a slug:
/// `Q4-plan-1f2e3d…` (32 hex digits) or `roadmap-8a2c1b7f9d10`.
fn strip_trailing_id(slug: &str) -> &str {
    let Some(dash) = slug.rfind('-') else {
        return if is_id(slug) { "" } else { slug };
    };
    if is_id(&slug[dash + 1..]) {
        &slug[..dash]
    } else {
        slug
    }
}

fn is_id(part: &str) -> bool {
    part.len() >= 8
        && part.chars().all(|c| c.is_ascii_hexdigit())
        && part.chars().any(|c| c.is_ascii_digit())
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn title(url: &str) -> (Service, String) {
        let r = recognize(url).unwrap_or_else(|| panic!("not recognised: {url}"));
        (r.service, r.title)
    }

    #[test]
    fn recognises_the_tools_teams_link() {
        let cases = [
            (
                "https://www.notion.so/acme/Q4-campaign-brief-1f2e3d4c5b6a79881f2e3d4c5b6a7988",
                Service::Notion,
                "Q4 campaign brief",
            ),
            (
                "https://acme.notion.site/1f2e3d4c5b6a79881f2e3d4c5b6a7988",
                Service::Notion,
                "Notion page",
            ),
            (
                "https://docs.google.com/document/d/1AbC/edit",
                Service::GoogleDocs,
                "Google Doc",
            ),
            (
                "https://docs.google.com/spreadsheets/d/1AbC/edit#gid=0",
                Service::GoogleSheets,
                "Google Sheet",
            ),
            (
                "https://docs.google.com/presentation/d/1AbC",
                Service::GoogleSlides,
                "Google Slides",
            ),
            (
                "https://drive.google.com/drive/folders/1AbC",
                Service::GoogleDrive,
                "Google Drive folder",
            ),
            (
                "https://linear.app/acme/issue/ENG-123/fix-resize-flicker",
                Service::Linear,
                "ENG-123 Fix resize flicker",
            ),
            (
                "https://linear.app/acme/project/q4-launch-8a2c1b7f9d10",
                Service::Linear,
                "Q4 launch",
            ),
            (
                "https://app.hubspot.com/contacts/12345/record/0-3/678",
                Service::HubSpot,
                "HubSpot deal",
            ),
            (
                "https://app-eu1.hubspot.com/contacts/1/company/2",
                Service::HubSpot,
                "HubSpot company",
            ),
            (
                "https://www.figma.com/design/AbC123/Onboarding-v2?node-id=1-2",
                Service::Figma,
                "Onboarding v2",
            ),
            (
                "https://acme.slack.com/archives/C024BE91L/p1700000000000100",
                Service::Slack,
                "Slack message",
            ),
            (
                "https://acme.slack.com/archives/C024BE91L",
                Service::Slack,
                "Slack channel",
            ),
            (
                "https://github.com/Ubra-Dev/ubra-app/pull/600",
                Service::GitHub,
                "Ubra-Dev/ubra-app#600",
            ),
            (
                "https://github.com/Ubra-Dev/ubra-app",
                Service::GitHub,
                "Ubra-Dev/ubra-app",
            ),
            (
                "https://lookerstudio.google.com/reporting/abc",
                Service::Dashboard,
                "Looker Studio report",
            ),
            (
                "https://analytics.google.com/analytics/web/#/p1/reports",
                Service::Dashboard,
                "Google Analytics report",
            ),
            (
                "https://app.amplitude.com/analytics/acme/chart/abc",
                Service::Dashboard,
                "Amplitude chart",
            ),
            (
                "https://us.posthog.com/project/1/dashboard/2",
                Service::Dashboard,
                "PostHog dashboard",
            ),
            (
                "https://metabase.acme.io/dashboard/4",
                Service::Dashboard,
                "Metabase dashboard",
            ),
        ];
        for (url, service, want) in cases {
            assert_eq!(title(url), (service, want.to_owned()), "{url}");
        }
    }

    #[test]
    fn everything_else_is_a_plain_link() {
        for url in [
            "https://getubra.com/notes",
            "ubra://session/s_1",
            "mailto:a@b.c",
            "https://notnotion.so.example.com/x",
            "https://github.com/",
        ] {
            assert_eq!(recognize(url), None, "{url}");
        }
    }

    #[test]
    fn slugs_read_as_words() {
        assert_eq!(slug_title("q4-plan"), "Q4 plan");
        assert_eq!(slug_title("Caf%C3%A9-menu"), "Café menu");
        assert_eq!(strip_trailing_id("plan-deadbeef12"), "plan");
        assert_eq!(strip_trailing_id("plan-v2"), "plan-v2");
        assert_eq!(strip_trailing_id("release"), "release");
    }
}
