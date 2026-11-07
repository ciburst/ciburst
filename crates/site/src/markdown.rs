//! Markdown to HTML for the site (spec 11.5): heading ids, check rows, links to the pages of the
//! source files, tables in their own scrolling box.

use std::collections::HashSet;

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

/// A heading of a rendered page: level, id and text.
#[derive(Debug, Clone, PartialEq)]
pub struct Heading {
    pub level: u8,
    pub id: String,
    pub text: String,
}

pub struct Rendered {
    pub html: String,
    pub headings: Vec<Heading>,
}

/// The id of a heading: lowercase letters and digits, every other run of characters one hyphen.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

const CHECK: &str = "Check:";

/// Renders `markdown`. `link` maps a repository path, with an optional `#anchor`, to the href of
/// its page, or `None` for a path that is not one of the source files.
pub fn render(markdown: &str, link: &dyn Fn(&str) -> Option<String>) -> Rendered {
    let events: Vec<Event> = Parser::new_ext(markdown, Options::ENABLE_TABLES).collect();
    let headings = headings(&events);
    let mut ids = headings.iter().map(|h| h.id.clone());
    let mut out = Vec::with_capacity(events.len());
    let mut in_link = 0usize;
    let mut check_open = false;
    let mut iter = events.into_iter().peekable();
    while let Some(event) = iter.next() {
        match event {
            Event::Start(Tag::Heading {
                level,
                classes,
                attrs,
                ..
            }) => out.push(Event::Start(Tag::Heading {
                level,
                id: ids.next().map(CowStr::from),
                classes,
                attrs,
            })),
            Event::Start(Tag::Table(alignments)) => {
                out.push(Event::Html("<div class=\"table\">".into()));
                out.push(Event::Start(Tag::Table(alignments)));
            }
            Event::End(TagEnd::Table) => {
                out.push(Event::End(TagEnd::Table));
                out.push(Event::Html("</div>".into()));
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                in_link += 1;
                let dest_url = rewrite(&dest_url, link).map_or(dest_url, CowStr::from);
                out.push(Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    title,
                    id,
                }));
            }
            Event::End(TagEnd::Link) => {
                in_link = in_link.saturating_sub(1);
                out.push(Event::End(TagEnd::Link));
            }
            Event::Code(code) if in_link == 0 => match link(&code) {
                Some(href) => out.push(Event::Html(
                    format!(
                        "<a href=\"{}\"><code>{}</code></a>",
                        escape(&href),
                        escape(&code)
                    )
                    .into(),
                )),
                None => out.push(Event::Code(code)),
            },
            Event::SoftBreak | Event::HardBreak if matches!(iter.peek(), Some(Event::Text(t)) if t.starts_with(CHECK)) =>
            {
                if check_open {
                    out.push(Event::Html("</span></span>".into()));
                }
                out.push(Event::Html(
                    "<span class=\"check\"><span class=\"check-label\">Check</span><span>".into(),
                ));
                if let Some(Event::Text(text)) = iter.next() {
                    out.push(Event::Text(
                        text[CHECK.len()..].trim_start().to_string().into(),
                    ));
                }
                check_open = true;
            }
            Event::End(end @ (TagEnd::Paragraph | TagEnd::Item)) => {
                if check_open {
                    out.push(Event::Html("</span></span>".into()));
                    check_open = false;
                }
                out.push(Event::End(end));
            }
            other => out.push(other),
        }
    }
    let mut html_out = String::new();
    html::push_html(&mut html_out, out.into_iter());
    Rendered {
        html: html_out,
        headings,
    }
}

/// A link to a repository path goes to its page, or to the file on gitlab.com; other links stay.
fn rewrite(dest: &str, link: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    if dest.is_empty() || dest.starts_with('#') || dest.contains("://") || dest.contains(':') {
        return None;
    }
    let path = dest.trim_start_matches("./");
    Some(link(path).unwrap_or_else(|| format!("{}/-/blob/main/{path}", crate::PROJECT_URL)))
}

/// Every heading with a unique id from its text.
fn headings(events: &[Event]) -> Vec<Heading> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut current: Option<(u8, String)> = None;
    for event in events {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                current = Some((*level as u8, String::new()))
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, text)) = current.as_mut() {
                    text.push_str(t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, text)) = current.take() {
                    let base = slug(&text);
                    let mut id = base.clone();
                    let mut n = 2;
                    while !seen.insert(id.clone()) {
                        id = format!("{base}-{n}");
                        n += 1;
                    }
                    out.push(Heading { level, id, text });
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_links(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn slugs_keep_letters_and_digits() {
        assert_eq!(slug("How it works"), "how-it-works");
        assert_eq!(slug("5.4 Configuration"), "5-4-configuration");
        assert_eq!(slug("Host: `trait Host`"), "host-trait-host");
        assert_eq!(slug("ciburst.json"), "ciburst-json");
    }

    #[test]
    fn headings_get_unique_ids() {
        let r = render("# A\n\n## Lanes\n\n## Lanes\n", &no_links);
        assert!(r.html.contains("<h1 id=\"a\">A</h1>"), "{}", r.html);
        assert!(r.html.contains("<h2 id=\"lanes\">"), "{}", r.html);
        assert!(r.html.contains("<h2 id=\"lanes-2\">"), "{}", r.html);
        assert_eq!(r.headings.len(), 3);
    }

    /// The README's steps keep inline code inside the check text, in one element.
    #[test]
    fn a_check_line_becomes_a_check_row() {
        let md = "1. Setup. Run `ciburst-setup`.\n   Check: the jobs start on `ci-<id>-vm`. Again.\n2. Next.\n";
        let html = render(md, &no_links).html;
        assert!(
            html.contains("<li>Setup. Run <code>ciburst-setup</code>.<span class=\"check\"><span class=\"check-label\">Check</span><span>the jobs start on <code>ci-&lt;id&gt;-vm</code>. Again.</span></span></li>"),
            "{html}"
        );
        assert!(html.contains("<li>Next.</li>"), "{html}");
        let loose = render("Undo: run it.\nCheck: it is gone.\n", &no_links).html;
        assert!(
            loose.contains(
                "<span class=\"check-label\">Check</span><span>it is gone.</span></span></p>"
            ),
            "{loose}"
        );
    }

    #[test]
    fn source_files_link_to_their_pages() {
        let link = |path: &str| (path == "docs/SPEC.md").then(|| "../design/".to_string());
        let html = render(
            "Design: `docs/SPEC.md`. Other: `LICENSE`. [Spec](docs/SPEC.md), [license](LICENSE), [web](https://example.org).",
            &link,
        )
        .html;
        assert!(
            html.contains("<a href=\"../design/\"><code>docs/SPEC.md</code></a>"),
            "{html}"
        );
        assert!(html.contains("<code>LICENSE</code>"), "{html}");
        assert!(html.contains("<a href=\"../design/\">Spec</a>"), "{html}");
        assert!(
            html.contains(
                "<a href=\"https://gitlab.com/ciburst/ciburst/-/blob/main/LICENSE\">license</a>"
            ),
            "{html}"
        );
        assert!(
            html.contains("<a href=\"https://example.org\">web</a>"),
            "{html}"
        );
    }

    #[test]
    fn tables_scroll_in_their_own_box() {
        let html = render("| a | b |\n|---|---|\n| 1 | 2 |\n", &no_links).html;
        assert!(html.starts_with("<div class=\"table\"><table>"), "{html}");
        assert!(
            html.trim_end().ends_with("</table>\n</div>")
                || html.trim_end().ends_with("</table></div>"),
            "{html}"
        );
    }
}
