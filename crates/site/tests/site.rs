//! spec 11.5: the site built from this repository's own files.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Site {
    _dir: tempfile::TempDir,
    root: PathBuf,
    /// Every HTML file below the root, by its path with `/`.
    html: BTreeMap<String, String>,
}

fn built() -> Site {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("public");
    ciburst_site::build(&repo(), &root).unwrap();
    let mut html = BTreeMap::new();
    let mut stack = vec![root.clone()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "html") {
                let rel = path
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                html.insert(rel, fs::read_to_string(&path).unwrap());
            }
        }
    }
    Site {
        _dir: dir,
        root,
        html,
    }
}

/// The targets of a page's links and images.
fn links(html: &str) -> Vec<String> {
    ["href=\"", "src=\""]
        .iter()
        .flat_map(|attr| html.split(attr).skip(1))
        .map(|rest| rest.split('"').next().unwrap().replace("&amp;", "&"))
        .collect()
}

/// The HTML file a relative href leads to from `from` (both below the root).
fn target(from: &str, href: &str) -> String {
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop();
    for piece in href.split('/') {
        match piece {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if href.ends_with('/') || href.is_empty() || href == "." || href.ends_with("..") {
        if joined.is_empty() {
            "index.html".to_string()
        } else {
            format!("{joined}/index.html")
        }
    } else {
        joined
    }
}

#[test]
fn every_page_is_built() {
    let site = built();
    let pages: Vec<&str> = site.html.keys().map(String::as_str).collect();
    assert_eq!(
        pages,
        [
            "404.html",
            "adopt/index.html",
            "agents/index.html",
            "ciburst-json/index.html",
            "command-line/index.html",
            "costs/index.html",
            "design/index.html",
            "design/providers/index.html",
            "development/index.html",
            "how-it-works/index.html",
            "index.html",
            "join/index.html",
            "lanes/index.html",
            "operations/index.html",
        ]
    );
    for asset in [
        "site.css",
        "logo.svg",
        "favicon.svg",
        "fonts/source-sans-3-latin.woff2",
        "fonts/source-code-pro-latin.woff2",
        "fonts/OFL-source-sans-3.txt",
        "fonts/OFL-source-code-pro.txt",
    ] {
        assert!(site.root.join(asset).is_file(), "{asset}");
    }
}

/// Every internal link, anchor and image resolves; no page holds a script or a dash the docs
/// forbid.
#[test]
fn links_and_anchors_resolve() {
    let site = built();
    for (page, html) in &site.html {
        assert!(!html.contains("<script"), "{page} holds a script");
        assert!(
            !html.contains('\u{2013}') && !html.contains('\u{2014}'),
            "{page} holds an en or em dash"
        );
        if page == "404.html" {
            continue;
        }
        for href in links(html) {
            if href.starts_with("https://")
                || href.starts_with("http://")
                || href.starts_with("mailto:")
            {
                continue;
            }
            let (path, anchor) = href.split_once('#').unwrap_or((&href, ""));
            let file = if path.is_empty() {
                page.clone()
            } else {
                target(page, path)
            };
            let text = match site.html.get(&file) {
                Some(text) => text.clone(),
                None => {
                    assert!(
                        site.root.join(&file).is_file(),
                        "{page}: {href} leads to {file}, which does not exist"
                    );
                    continue;
                }
            };
            if !anchor.is_empty() {
                assert!(
                    text.contains(&format!("id=\"{anchor}\"")),
                    "{page}: {href} names an anchor that {file} lacks"
                );
            }
        }
    }
}

/// spec 11.5: the home page with its stylesheet, fonts, logo and icon stays under 100 KB as
/// served, the text files compressed; every text file has its compressed copy.
#[test]
fn the_home_page_stays_light() {
    let site = built();
    let bytes: u64 = [
        "index.html.gz",
        "site.css.gz",
        "logo.svg.gz",
        "favicon.svg.gz",
        "fonts/source-sans-3-latin.woff2",
        "fonts/source-code-pro-latin.woff2",
    ]
    .iter()
    .map(|f| fs::metadata(site.root.join(f)).unwrap().len())
    .sum();
    assert!(bytes < 100 * 1024, "the first view weighs {bytes} bytes");
    for page in site.html.keys() {
        let gz = site.root.join(format!("{page}.gz"));
        let mut text = String::new();
        std::io::Read::read_to_string(
            &mut flate2::read::GzDecoder::new(fs::File::open(&gz).unwrap()),
            &mut text,
        )
        .unwrap();
        assert_eq!(&text, &site.html[page], "{page}.gz holds the page");
    }
}

/// spec 11.5: the animation on the home page, wide and narrow, with its button; the README's
/// file is the generator's.
#[test]
fn the_animation_is_on_the_home_page_and_in_the_readme() {
    let site = built();
    let home = &site.html["index.html"];
    assert!(
        home.contains("class=\"fw\"") && home.contains("class=\"fn\""),
        "both layouts"
    );
    assert!(
        home.contains("<label for=\"flow-toggle\""),
        "the play and pause button"
    );
    assert!(home.contains("prefers-reduced-motion"));
    assert_eq!(
        home.matches("<img").count(),
        1,
        "the header's logo is the page's one image; the README's animation stays off it"
    );
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    assert!(readme.contains("](docs/flow.svg)"));
    let file = fs::read_to_string(repo().join(ciburst_site::README_FLOW))
        .unwrap()
        .replace("\r\n", "\n");
    assert!(
        file == ciburst_site::readme_flow().unwrap(),
        "docs/flow.svg differs from the generator: cargo run -p ciburst-site -- --flow"
    );
}

#[test]
fn each_source_lands_on_its_page() {
    let site = built();
    let home = &site.html["index.html"];
    assert!(
        home.contains("<h1>Serverless GitLab runners in your own cloud account</h1>"),
        "{home}"
    );
    for chapter in [
        "each-job-in-its-own-image",
        "docker-when-a-job-needs-it",
        "nothing-runs-between-pipelines",
        "in-your-own-cloud-account",
    ] {
        assert!(
            home.contains(&format!("<h2 id=\"{chapter}\">")),
            "the story's chapter {chapter}"
        );
    }
    assert!(home.contains("Adopt in five steps"));
    assert!(
        home.contains(">ciburst lanes</text>") && home.contains(">23.23 EUR</text>"),
        "the cost bars hold the README's month"
    );
    assert!(
        !home.contains("Status: ") && !home.contains("docs/SPEC.md"),
        "the opening's pointers stay off the home page"
    );
    assert!(
        !home.contains("https://ciburst.zwa.la"),
        "the README's line naming the site stays off the site"
    );
    assert!(
        !home.contains("logo-tile.svg") && home.contains("<img src=\"logo.svg\" alt=\"ciburst\""),
        "the header carries the logo, the README's title line stays off the page (D76)"
    );
    assert!(home.contains("class=\"p-lane\"") && home.contains("VM ends itself, minute 55"));
    let costs = &site.html["costs/index.html"];
    assert!(costs.contains("<h1 id=\"cost-and-limits\">Cost and limits</h1>"));
    assert!(
        costs.contains("VM ends itself, minute 55"),
        "the VM hour on its page"
    );
    assert!(
        costs.contains("<a href=\"../design/\"><code>docs/SPEC.md</code></a>"),
        "the README's file names link to their pages"
    );
    assert!(
        site.html["how-it-works/index.html"].contains("<h1 id=\"how-it-works\">How it works</h1>")
            && site.html["how-it-works/index.html"].contains("class=\"p-lane\""),
        "How it works with the measured pipeline"
    );
    assert!(
        site.html["development/index.html"].contains("id=\"license\""),
        "Development with the license"
    );
    let adopt = &site.html["adopt/index.html"];
    assert!(adopt.contains("<h1 id=\"adopt\">Adopt</h1>"));
    assert!(
        adopt.contains("<span class=\"check-label\">Check</span>"),
        "the README's checks become check rows"
    );
    assert!(site.html["agents/index.html"].contains("Adopt ciburst in a GitLab project"));
    assert!(site.html["ciburst-json/index.html"].contains("ignoreJobs"));
    assert!(site.html["operations/index.html"].contains("<h1 id=\"runbook\">Runbook</h1>"));
    assert!(site.html["design/providers/index.html"].contains("trait Provider"));
}

/// The VM hour drawing keeps the README's numbers.
#[test]
fn the_readme_still_states_the_drawn_numbers() {
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    assert!(readme.contains("0.03 EUR per Scaleway DEV1-M hour"));
    assert!(readme.contains("minute 55"));
}
