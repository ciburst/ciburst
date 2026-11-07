//! The site at ciburst.zwa.la (spec 11.5, D75): pages from `README.md`, `AGENTS.md` and `docs/`,
//! one stylesheet, two fonts, and the home page's drawings generated at build time.

mod figures;
mod flow;
mod markdown;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub use markdown::{escape, render, slug};

/// The project on gitlab.com: repository files the pages link to, releases, source.
pub const PROJECT_URL: &str = "https://gitlab.com/ciburst/ciburst";

const CSS: &str = include_str!("../assets/site.css");
const PIPELINE: &str = include_str!("../assets/pipeline.json");
const FLOW: &str = include_str!("../assets/flow.json");
/// The logo in the header and the cog as the icon, from `brand/` (spec 11.6).
const LOGO: &str = include_str!("../../../brand/logo.svg");
const FAVICON: &str = include_str!("../../../brand/favicon.svg");
const FONTS: &[(&str, &[u8])] = &[
    (
        "source-sans-3-latin.woff2",
        include_bytes!("../assets/fonts/source-sans-3-latin.woff2"),
    ),
    (
        "source-code-pro-latin.woff2",
        include_bytes!("../assets/fonts/source-code-pro-latin.woff2"),
    ),
    (
        "OFL-source-sans-3.txt",
        include_bytes!("../assets/fonts/OFL-source-sans-3.txt"),
    ),
    (
        "OFL-source-code-pro.txt",
        include_bytes!("../assets/fonts/OFL-source-code-pro.txt"),
    ),
];

/// A page below the site root: its path ("" for the home page), its entry in the contents and its
/// Markdown.
pub struct Page {
    pub path: &'static str,
    pub label: &'static str,
    pub markdown: String,
}

/// The pages of each source file, for links to them (spec 11.5).
const SOURCES: &[(&str, &str)] = &[
    ("README.md", ""),
    ("AGENTS.md", "agents"),
    ("docs/SPEC.md", "design"),
    ("docs/RUNBOOK.md", "operations"),
    ("docs/PROVIDERS.md", "design/providers"),
];

/// README sections and the pages they become: path, entry in the contents, section title.
const README_PAGES: &[(&str, &str, &str)] = &[
    ("how-it-works", "How it works", "How it works"),
    ("costs", "Cost and limits", "Cost and limits"),
    ("development", "Development", "Development"),
    ("adopt", "Adopt", "Adopt"),
    ("join", "Join an existing setup", "Join an existing setup"),
    ("lanes", "Lanes", "Lanes"),
    ("command-line", "Command line", "Run from a workstation"),
];

fn readme_page(sections: &[(String, String)], path: &str) -> Result<Page, String> {
    let (path, label, title) = README_PAGES
        .iter()
        .find(|(p, _, _)| *p == path)
        .ok_or_else(|| format!("no README page {path}"))?;
    Ok(Page {
        path,
        label,
        markdown: promote(section(sections, "README.md", title)?, 1),
    })
}

/// The README's section that tells the home page's story, a chapter per `###` heading, and the
/// picture beside each chapter (spec 11.5).
const STORY: &str = "What ciburst does";
const CHAPTERS: &[(&str, Picture)] = &[
    ("Each job in its own image", Picture::Pipeline),
    ("Docker when a job needs it", Picture::Hour),
    ("Nothing runs between pipelines", Picture::Costs),
    ("In your own cloud account", Picture::Adopt),
];

#[derive(Clone, Copy)]
enum Picture {
    Pipeline,
    Hour,
    Costs,
    Adopt,
}

fn read(root: &Path, file: &str) -> Result<String, String> {
    fs::read_to_string(root.join(file)).map_err(|e| format!("{file}: {e}"))
}

/// The text before the first `## ` heading, and each `## ` section with its heading line;
/// headings inside code fences do not count.
fn split(markdown: &str) -> (String, Vec<(String, String)>) {
    let mut intro = String::new();
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut fence = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            fence = !fence;
        }
        if !fence && let Some(title) = line.strip_prefix("## ") {
            sections.push((title.trim().to_string(), String::new()));
        }
        let target = match sections.last_mut() {
            Some((_, body)) => body,
            None => &mut intro,
        };
        target.push_str(line);
        target.push('\n');
    }
    (intro, sections)
}

fn section<'a>(
    sections: &'a [(String, String)],
    file: &str,
    title: &str,
) -> Result<&'a str, String> {
    sections
        .iter()
        .find(|(t, _)| t == title)
        .map(|(_, body)| body.as_str())
        .ok_or_else(|| format!("{file}: no section \"## {title}\""))
}

/// The section of `docs/SPEC.md` that starts with the heading `### 5.4 `, up to the next
/// heading of level 2 or 3, without its heading line.
fn spec_section(spec: &str, number: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut inside = false;
    let mut fence = false;
    for line in spec.lines() {
        if line.trim_start().starts_with("```") {
            fence = !fence;
        }
        if !fence && (line.starts_with("### ") || line.starts_with("## ")) {
            if inside {
                break;
            }
            inside = line.starts_with(&format!("### {number} "));
            continue;
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    if out.trim().is_empty() {
        return Err(format!("docs/SPEC.md: no section {number}"));
    }
    Ok(out)
}

/// Raises every ATX heading outside code fences by `by` levels, `## A` to `# A` for 1.
fn promote(markdown: &str, by: usize) -> String {
    let mut out = String::new();
    let mut fence = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            fence = !fence;
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if !fence && hashes > by && line[hashes..].starts_with(' ') {
            out.push_str(&line[by..]);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Drops the first line, a document's own `# ` title.
fn without_title(markdown: &str) -> &str {
    markdown.split_once('\n').map_or("", |(_, rest)| rest)
}

/// The pages of the site, in the order of the contents (spec 11.5). The home page is built
/// separately from the README's opening.
pub fn pages(root: &Path) -> Result<Vec<Page>, String> {
    let readme = read(root, "README.md")?;
    let (_, sections) = split(&readme);
    let mut pages = vec![
        readme_page(&sections, "how-it-works")?,
        readme_page(&sections, "adopt")?,
        readme_page(&sections, "join")?,
        readme_page(&sections, "lanes")?,
    ];
    let spec = read(root, "docs/SPEC.md")?;
    pages.push(Page {
        path: "ciburst-json",
        label: "ciburst.json",
        markdown: format!(
            "# ciburst.json\n\n{}",
            promote(&spec_section(&spec, "5.4")?, 2)
        ),
    });
    pages.push(readme_page(&sections, "costs")?);
    pages.push(readme_page(&sections, "command-line")?);
    let assistant = section(&sections, "README.md", "With an AI assistant")?;
    let agents = read(root, "AGENTS.md")?;
    pages.push(Page {
        path: "agents",
        label: "For AI agents",
        markdown: format!(
            "# For AI agents\n\n{}\n{}",
            without_title(assistant),
            without_title(&agents)
        ),
    });
    pages.push(Page {
        path: "operations",
        label: "Operations",
        markdown: read(root, "docs/RUNBOOK.md")?,
    });
    pages.push(Page {
        path: "design",
        label: "Design",
        markdown: spec,
    });
    pages.push(Page {
        path: "design/providers",
        label: "Providers",
        markdown: read(root, "docs/PROVIDERS.md")?,
    });
    let mut development = readme_page(&sections, "development")?;
    development.markdown = format!(
        "{}\n{}",
        development.markdown,
        section(&sections, "README.md", "License")?
    );
    pages.push(development);
    Ok(pages)
}

/// The relative way from page `from` to page `to` (both site paths, "" for the root), ending
/// in `/` unless it is the root itself.
fn rel(from: &str, to: &str) -> String {
    let up = if from.is_empty() {
        0
    } else {
        from.split('/').count()
    };
    let mut out = "../".repeat(up);
    if !to.is_empty() {
        out.push_str(to);
        out.push('/');
    }
    if out.is_empty() {
        out.push_str("./");
    }
    out
}

/// The href of a repository path, with an optional anchor, from page `from`: a source file goes
/// to its page; a README section to the page that holds it.
fn source_link(from: &str, target: &str) -> Option<String> {
    let (path, anchor) = target.split_once('#').unwrap_or((target, ""));
    let page = SOURCES.iter().find(|(file, _)| *file == path)?.1;
    let page = if path == "README.md" {
        README_PAGES
            .iter()
            .find(|(_, _, title)| !anchor.is_empty() && slug(title) == anchor)
            .map_or(page, |(p, _, _)| p)
    } else {
        page
    };
    let mut href = rel(from, page);
    if !anchor.is_empty() {
        href.push('#');
        href.push_str(anchor);
    }
    Some(href)
}

fn render_for(from: &str, markdown: &str) -> markdown::Rendered {
    render(markdown, &|target| source_link(from, target))
}

const PATTERNS: &str = "<svg class=\"defs\" width=\"0\" height=\"0\" aria-hidden=\"true\" focusable=\"false\"><defs>\
<pattern id=\"hatch\" width=\"5\" height=\"5\" patternUnits=\"userSpaceOnUse\" patternTransform=\"rotate(45)\"><rect class=\"hatch-bg\" width=\"5\" height=\"5\"/><line class=\"hatch-line\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"5\"/></pattern>\
<pattern id=\"hatch-lane\" width=\"5\" height=\"5\" patternUnits=\"userSpaceOnUse\" patternTransform=\"rotate(45)\"><rect class=\"hatch-lane-bg\" width=\"5\" height=\"5\"/><line class=\"hatch-lane-line\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"5\"/></pattern>\
</defs></svg>";

const NUMBERS: &[&str] = &[
    "no", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
];

/// The home page: the README's opening as heading and lede, the animation, then the story of
/// What ciburst does, a picture beside each chapter (spec 11.5). The rest of the opening points
/// to places the site links anyway and is left out.
fn home(root: &Path) -> Result<String, String> {
    let readme = read(root, "README.md")?;
    let (intro, sections) = split(&readme);
    let mut paragraphs = without_title(&intro)
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty());
    let heading = paragraphs
        .next()
        .ok_or("README.md: no opening sentence")?
        .trim_end_matches('.');
    let lede = paragraphs.next().ok_or("README.md: no second paragraph")?;
    let adopt = section(&sections, "README.md", "Adopt")?;
    let steps = adopt
        .lines()
        .filter(|l| {
            l.split_once(". ")
                .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        })
        .count();
    let steps = NUMBERS.get(steps).copied().unwrap_or("a few");
    let include = adopt
        .split("```yaml\n")
        .nth(1)
        .and_then(|s| s.split("```").next())
        .ok_or("README.md: no yaml block in Adopt")?;
    let include = include
        .lines()
        .map(|l| l.strip_prefix("   ").unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n");
    let include = include.trim_end();
    let inline = |md: &str| {
        let html = render_for("", md).html;
        html.trim()
            .trim_start_matches("<p>")
            .trim_end_matches("</p>")
            .to_string()
    };
    let pipeline = figures::parse_pipeline(PIPELINE)?;
    let costs = figures::parse_costs(section(&sections, "README.md", "Cost and limits")?)?;
    let mut body = format!(
        "{PATTERNS}<div class=\"open\"><h1>{}</h1><p class=\"lede\">{}</p>\
         <p class=\"actions\"><a class=\"btn\" href=\"adopt/\">Adopt in {steps} steps</a>\
         <a href=\"how-it-works/\">How it works</a></p></div>{}",
        inline(heading),
        inline(lede),
        flow_figure()?
    );
    let story = section(&sections, "README.md", STORY)?;
    let mut chapters = story.split("\n### ").skip(1).peekable();
    if chapters.peek().is_none() {
        return Err(format!("README.md: {STORY} has no ### chapters"));
    }
    for chapter in chapters {
        let (title, text) = chapter.split_once('\n').unwrap_or((chapter, ""));
        let title = title.trim();
        let picture = CHAPTERS
            .iter()
            .find(|(t, _)| *t == title)
            .map(|(_, p)| *p)
            .ok_or_else(|| format!("README.md: no picture for the chapter \"{title}\""))?;
        let picture = match picture {
            Picture::Pipeline => figures::pipeline_figure(&pipeline),
            Picture::Hour => figures::hour_figure(),
            Picture::Costs => figures::cost_figure(&costs, "costs/"),
            Picture::Adopt => format!(
                "<pre><code>{}</code></pre><p class=\"actions\"><a class=\"btn\" href=\"adopt/\">\
                 Adopt in {steps} steps</a><a href=\"join/\">Join an existing setup</a></p>",
                escape(include)
            ),
        };
        body.push_str(&format!(
            "<section class=\"story\"><div class=\"story-text\"><h2 id=\"{}\">{}</h2>{}</div>\
             <div class=\"story-picture\">{picture}</div></section>",
            slug(title),
            escape(title),
            render_for("", text).html
        ));
    }
    Ok(body)
}

/// The way from page `path` to the site root, empty on the root itself.
fn root_of(path: &str) -> String {
    match rel(path, "").as_str() {
        "./" => String::new(),
        other => other.to_string(),
    }
}

/// The page around `body`: header with the logo and the site links, the contents on doc
/// pages, the footer. `root` leads from the page to the site root: empty on the home page,
/// `/` on the 404 page, which GitLab serves at any depth.
fn layout(root: &str, title: &str, body: &str, contents: Option<&str>) -> String {
    let docs_current = if contents.is_some() {
        " aria-current=\"page\""
    } else {
        ""
    };
    let main = match contents {
        Some(contents) => format!(
            "<div class=\"docs\"><nav class=\"side\" aria-label=\"Contents\">{contents}</nav>\
             <article class=\"doc\">{body}</article></div>"
        ),
        None => format!("<div class=\"home\">{body}</div>"),
    };
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n\
         <meta name=\"description\" content=\"Serverless GitLab runners in your own cloud account.\">\n\
         <link rel=\"preload\" href=\"{root}fonts/source-sans-3-latin.woff2\" as=\"font\" type=\"font/woff2\" crossorigin>\n\
         <link rel=\"icon\" href=\"{root}favicon.svg\" type=\"image/svg+xml\">\n\
         <link rel=\"stylesheet\" href=\"{root}site.css\">\n</head>\n<body>\n\
         <header class=\"bar\"><a class=\"word\" href=\"{}\">\
         <img src=\"{root}logo.svg\" alt=\"ciburst\" width=\"123\" height=\"28\"></a>\
         <nav class=\"nav\" aria-label=\"Site\"><a href=\"{root}adopt/\"{docs_current}>Docs</a>\
         <a href=\"{root}costs/\">Costs</a><a href=\"{PROJECT_URL}/-/releases\">Releases</a>\
         <a href=\"{PROJECT_URL}\">GitLab</a></nav></header>\n<main>{main}</main>\n\
         <footer class=\"foot\">ciburst {}, Apache-2.0. Source, issues and releases on \
         <a href=\"{PROJECT_URL}\">GitLab</a>.</footer>\n</body>\n</html>\n",
        escape(title),
        if root.is_empty() { "./" } else { root },
        env!("CARGO_PKG_VERSION"),
    )
}

/// The contents of a doc page: every page, the current one with its sections.
fn contents(pages: &[Page], current: &str, headings: &[markdown::Heading]) -> String {
    let mut out = String::from("<ul>");
    out.push_str(&format!(
        "<li><a href=\"{}\">Overview</a></li>",
        rel(current, "")
    ));
    for page in pages {
        if page.path == current {
            out.push_str(&format!(
                "<li><a href=\"./\" aria-current=\"page\">{}</a>",
                escape(page.label)
            ));
            let subs: Vec<&markdown::Heading> = headings.iter().filter(|h| h.level == 2).collect();
            if subs.len() > 1 {
                out.push_str("<ul class=\"sub\">");
                for h in subs {
                    out.push_str(&format!(
                        "<li><a href=\"#{}\">{}</a></li>",
                        h.id,
                        escape(&h.text)
                    ));
                }
                out.push_str("</ul>");
            }
            out.push_str("</li>");
        } else {
            out.push_str(&format!(
                "<li><a href=\"{}\">{}</a></li>",
                rel(current, page.path),
                escape(page.label)
            ));
        }
    }
    out.push_str("</ul>");
    out
}

fn write(out: &Path, path: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let file = out.join(path);
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(&file, bytes).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(file)
}

/// Builds the site from the repository at `root` into `out`; returns the files written.
pub fn build(root: &Path, out: &Path) -> Result<Vec<PathBuf>, String> {
    let pages = pages(root)?;
    let mut files = vec![write(
        out,
        "index.html",
        layout("", "ciburst: serverless GitLab runners", &home(root)?, None).as_bytes(),
    )?];
    let pipeline = figures::parse_pipeline(PIPELINE)?;
    for page in &pages {
        let rendered = render_for(page.path, &page.markdown);
        let title = format!("{}, ciburst", page.label);
        // The home page's pictures also stand on the pages that explain them.
        let body = match page.path {
            "how-it-works" => format!(
                "{PATTERNS}{}{}",
                rendered.html,
                figures::pipeline_figure(&pipeline)
            ),
            "costs" => format!(
                "{PATTERNS}{}",
                rendered
                    .html
                    .replacen("</h1>", &format!("</h1>{}", figures::hour_figure()), 1)
            ),
            _ => rendered.html.clone(),
        };
        let html = layout(
            &root_of(page.path),
            &title,
            &body,
            Some(&contents(&pages, page.path, &rendered.headings)),
        );
        files.push(write(
            out,
            &format!("{}/index.html", page.path),
            html.as_bytes(),
        )?);
    }
    files.push(write(
        out,
        "404.html",
        layout(
            "/",
            "Not found, ciburst",
            "<div class=\"open\"><h1>Not found</h1><p class=\"lede\">No page at this address. \
             <a href=\"/\">The home page</a> leads to the rest.</p></div>",
            None,
        )
        .as_bytes(),
    )?);
    files.push(write(out, "site.css", CSS.as_bytes())?);
    files.push(write(out, "logo.svg", LOGO.as_bytes())?);
    files.push(write(out, "favicon.svg", FAVICON.as_bytes())?);
    for (name, bytes) in FONTS {
        files.push(write(out, &format!("fonts/{name}"), bytes)?);
    }
    // GitLab Pages serves `<file>.gz` instead of a text file to browsers that accept it.
    let mut compressed = Vec::new();
    for file in &files {
        if file
            .extension()
            .is_some_and(|e| ["html", "css", "svg", "txt"].iter().any(|t| e == *t))
        {
            let bytes = fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            gz.write_all(&bytes)
                .and_then(|()| gz.finish())
                .and_then(|data| {
                    let mut name = file.clone().into_os_string();
                    name.push(".gz");
                    let path = PathBuf::from(name);
                    fs::write(&path, data).map(|()| path)
                })
                .map(|path| compressed.push(path))
                .map_err(|e| format!("{}.gz: {e}", file.display()))?;
        }
    }
    files.extend(compressed);
    Ok(files)
}

/// The README's animation, `docs/flow.svg`, below the repository at `root`.
pub const README_FLOW: &str = "docs/flow.svg";

/// Writes the README's animation into the repository at `root`.
pub fn write_readme_flow(root: &Path) -> Result<PathBuf, String> {
    write(root, README_FLOW, readme_flow()?.as_bytes())
}

/// The animation on the home page: the wide drawing, the narrow one on phones, and a button in
/// its corner that pauses it and plays it again (spec 11.5, D80).
fn flow_figure() -> Result<String, String> {
    let f = flow::parse(FLOW)?;
    Ok(format!(
        "<input type=\"checkbox\" id=\"flow-toggle\" class=\"flow-toggle\">\
         <figure class=\"fig flow\"><div class=\"flow-stage\">\
         <div class=\"flow-wide\">{}</div><div class=\"flow-narrow\">{}</div>\
         <label for=\"flow-toggle\" class=\"flow-button\">\
         <span class=\"flow-pause\"><svg viewBox=\"0 0 10 12\" aria-hidden=\"true\">\
         <rect width=\"3\" height=\"12\"/><rect x=\"7\" width=\"3\" height=\"12\"/></svg>Pause</span>\
         <span class=\"flow-play\"><svg viewBox=\"0 0 10 12\" aria-hidden=\"true\">\
         <path d=\"M0 0L10 6L0 12Z\"/></svg>Play</span></label></div>\
         <figcaption>{}</figcaption></figure>",
        flow::flow_svg(&f, flow::Layout::Wide, None),
        flow::flow_svg(&f, flow::Layout::Narrow, None),
        escape(&f.caption)
    ))
}

/// The animation for the README: the wide drawing on the dark tile, with its colors and
/// Source Sans 3 inside the file.
pub fn readme_flow() -> Result<String, String> {
    let f = flow::parse(FLOW)?;
    let style = format!(
        "svg{{--paper:#10151C;--sunk:#18202A;--ink:#E7EBF0;--ink-2:#A9B2BE;--rule:#2C3642;         --lane:#E3A54E;--vm:#86B4E3;--stop:#F2867B;--sans:\"Source Sans 3\",sans-serif}}         @font-face{{font-family:\"Source Sans 3\";font-weight:200 900;         src:url(data:font/woff2;base64,{}) format(\"woff2\")}}",
        base64(FONTS[0].1)
    );
    Ok(flow::flow_svg(&f, flow::Layout::Wide, Some(&style)))
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let v = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(v >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_ways_between_pages() {
        assert_eq!(rel("", "adopt"), "adopt/");
        assert_eq!(rel("adopt", ""), "../");
        assert_eq!(rel("design/providers", "operations"), "../../operations/");
        assert_eq!(rel("", ""), "./");
    }

    #[test]
    fn source_files_and_readme_sections_find_their_pages() {
        assert_eq!(
            source_link("adopt", "docs/SPEC.md").as_deref(),
            Some("../design/")
        );
        assert_eq!(
            source_link("design", "README.md#lanes").as_deref(),
            Some("../lanes/#lanes")
        );
        assert_eq!(source_link("lanes", "README.md").as_deref(), Some("../"));
        assert_eq!(source_link("", "LICENSE"), None);
    }

    #[test]
    fn sections_ignore_headings_in_code() {
        let (intro, sections) = split("# T\n\nx\n\n## A\n\n```\n## not\n```\n\n## B\ny\n");
        assert_eq!(intro, "# T\n\nx\n\n");
        assert_eq!(sections.len(), 2);
        assert!(sections[0].1.contains("## not"));
        assert_eq!(
            promote("## A\n### B\n```\n## c\n```\n", 1),
            "# A\n## B\n```\n## c\n```\n"
        );
    }
}
