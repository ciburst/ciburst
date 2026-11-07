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

fn hrefs(html: &str) -> Vec<String> {
    html.split("href=\"")
        .skip(1)
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
            "design/index.html",
            "design/providers/index.html",
            "index.html",
            "join/index.html",
            "lanes/index.html",
            "operations/index.html",
        ]
    );
    for asset in [
        "site.css",
        "fonts/source-sans-3-latin.woff2",
        "fonts/source-code-pro-latin.woff2",
        "fonts/OFL-source-sans-3.txt",
        "fonts/OFL-source-code-pro.txt",
    ] {
        assert!(site.root.join(asset).is_file(), "{asset}");
    }
}

/// Every internal link and anchor resolves; no page holds a script or a dash the docs forbid.
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
        for href in hrefs(html) {
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

/// spec 11.5: the home page with its stylesheet and fonts stays under 100 KB.
#[test]
fn the_home_page_stays_light() {
    let site = built();
    let bytes: u64 = [
        "index.html",
        "site.css",
        "fonts/source-sans-3-latin.woff2",
        "fonts/source-code-pro-latin.woff2",
    ]
    .iter()
    .map(|f| fs::metadata(site.root.join(f)).unwrap().len())
    .sum();
    assert!(bytes < 100 * 1024, "the first view weighs {bytes} bytes");
}

#[test]
fn each_source_lands_on_its_page() {
    let site = built();
    let home = &site.html["index.html"];
    assert!(
        home.contains("<h1>Serverless GitLab runners in your own cloud account</h1>"),
        "{home}"
    );
    assert!(home.contains("id=\"how-it-works\"") && home.contains("id=\"cost-and-limits\""));
    assert!(home.contains("Adopt in five steps"));
    assert!(home.contains("class=\"p-lane\"") && home.contains("VM ends itself, minute 55"));
    assert!(
        home.contains("<a href=\"design/\"><code>docs/SPEC.md</code></a>"),
        "the README's file names link to their pages"
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
