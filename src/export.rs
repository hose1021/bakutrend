//! A static snapshot of the page, for a host that runs nothing.
//!
//! A server renders what a request asks for. A static host cannot answer a query, so every view
//! the page offers is written as a file of its own and every link between them names that file.
//! Two controls have no static form and are absent here while the server keeps them: the text
//! filter, because nothing behind a static file runs a search, and the card per story, because a
//! card is addressed by rank and the week window ranks over a thousand stories.
//!
//! Everything else is the same renderer, so a snapshot cannot disagree with the server about a
//! score, a notice or a sentence.

use std::fs;
use std::path::{Path, PathBuf};

use crate::app::{App, Sort};
use crate::config::Config;
use crate::i18n::Lang;
use crate::poller::PollReport;
use crate::store::{Store, Window};
use crate::web::{Links, back_to_root, page, page_path, stories_json, stories_path};

/// What one run wrote, for the line the command prints.
#[derive(Debug, Clone)]
pub struct Written {
    pub pages: usize,
    pub files: usize,
    pub bytes: u64,
    pub dir: PathBuf,
}

/// Write the whole site: one page per window, order and language, and the numbers behind each
/// view.
///
/// Every page is ranked against one clock — the moment this run started — so two pages of the
/// same snapshot cannot measure their ages from two different moments. The store is opened
/// read-only, exactly as the server opens it, so an export cannot race a poller that is writing
/// beside it.
pub fn write_site(
    db_path: &Path,
    config: &Config,
    report: Option<&PollReport>,
    dir: &Path,
) -> Result<Written, String> {
    let now = chrono::Utc::now().timestamp();
    let built = built_at(now);
    let mut written = Written {
        pages: 0,
        files: 0,
        bytes: 0,
        dir: dir.to_path_buf(),
    };

    for (lang, window, sort) in views() {
        let store = Store::open_read_only(db_path)
            .map_err(|error| format!("open {}: {error}", db_path.display()))?;
        let mut app = App::new(store, config.clone(), now);
        app.lang = lang;
        // The health line is the poller's own report. Without one the page says `sources
        // unknown`, which is what this run then knows.
        if let Some(report) = report {
            app.record_poll(report, now);
        }
        // The top story: a snapshot holds one card per page, and the first row of an order is the
        // story that order ranks first.
        app.set_view(window, sort, String::new(), 0);
        app.load_bodies();

        let file = page_path(lang, window, sort);
        let prefix = back_to_root(&file);
        let view = app.view();
        let html = page(
            &view,
            Links::Snapshot {
                prefix: &prefix,
                built: &built,
            },
        );
        write(&dir.join(&file), &html, &mut written)?;
        written.pages += 1;

        // The numbers carry no words, so one file serves all three languages. They are written
        // exactly as the endpoint returns them, byte for byte.
        if lang == Lang::En {
            let json = serde_json::to_string(&stories_json(&view))
                .map_err(|error| format!("encode {}: {error}", stories_path(window, sort)))?;
            write(&dir.join(stories_path(window, sort)), &json, &mut written)?;
        }
    }

    Ok(written)
}

/// Every view a snapshot holds.
fn views() -> Vec<(Lang, Window, Sort)> {
    let mut views = Vec::with_capacity(Lang::ALL.len() * Window::all().len() * Sort::ALL.len());
    for lang in Lang::ALL {
        for window in Window::all() {
            for sort in Sort::ALL {
                views.push((lang, window, sort));
            }
        }
    }
    views
}

/// The moment the ranking was read, written the one way every language reads a moment. It stands
/// on the page because the ages beside it are measured from here and not from the reader's clock.
fn built_at(now: i64) -> String {
    chrono::DateTime::from_timestamp(now, 0).map_or_else(
        || now.to_string(),
        |moment| moment.format("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

/// Write one file, making the directories it needs. Nothing is deleted: a snapshot only adds
/// files, so a directory that already holds pages of its own keeps them.
fn write(path: &Path, contents: &str, written: &mut Written) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    fs::write(path, contents).map_err(|error| format!("write {}: {error}", path.display()))?;
    written.files += 1;
    written.bytes += contents.len() as u64;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file a relative link points at, resolved from the page that carries it. The export
    /// writes paths that a browser resolves this way, so a test can too.
    fn resolve(from: &str, target: &str) -> String {
        let mut parts: Vec<&str> = from.split('/').collect();
        parts.pop();
        for part in target.split('/') {
            match part {
                ".." => {
                    parts.pop();
                }
                _ => parts.push(part),
            }
        }
        parts.join("/")
    }

    /// A snapshot is a closed set: every control it carries links to a file it holds, and no
    /// number file stands where a page does.
    #[test]
    fn every_page_links_to_a_file_the_snapshot_holds() {
        let files: Vec<String> = views()
            .iter()
            .map(|(lang, window, sort)| page_path(*lang, *window, *sort))
            .collect();
        let mut unique = files.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            files.len(),
            45,
            "three languages, three windows, five orders"
        );
        assert_eq!(unique.len(), files.len(), "one file per view: {files:?}");
        assert!(files.contains(&"index.html".to_string()), "{files:?}");

        for file in &files {
            for lang in Lang::ALL {
                for window in Window::all() {
                    for sort in Sort::ALL {
                        let target =
                            format!("{}{}", back_to_root(file), page_path(lang, window, sort));
                        let resolved = resolve(file, &target);
                        assert!(files.contains(&resolved), "{file} links to {resolved}");
                    }
                }
            }
        }

        for window in Window::all() {
            for sort in Sort::ALL {
                assert!(
                    !files.contains(&stories_path(window, sort)),
                    "{} would overwrite a page",
                    stories_path(window, sort)
                );
            }
        }
    }
}
