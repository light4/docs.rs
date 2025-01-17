//! Source code browser

use crate::{
    db::Pool,
    impl_webpage,
    utils::get_correct_docsrs_style_file,
    web::{
        error::Nope, file::File as DbFile, match_version, page::WebPage, redirect_base,
        MatchSemver, MetaData, Url,
    },
    Storage,
};
use iron::{IronResult, Request, Response};
use postgres::Client;
use router::Router;
use serde::Serialize;
use serde_json::Value;
use std::cmp::Ordering;

/// A source file's name and mime type
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Serialize)]
struct File {
    /// The name of the file
    name: String,
    /// The mime type of the file
    mime: String,
}

impl File {
    fn from_path_and_mime(path: &str, mime: &str) -> File {
        let (name, mime) = if let Some((dir, _)) = path.split_once('/') {
            (dir, "dir")
        } else {
            (path, mime)
        };

        Self {
            name: name.to_owned(),
            mime: mime.to_owned(),
        }
    }
}

/// A list of source files
#[derive(Debug, Clone, PartialEq, Serialize)]
struct FileList {
    metadata: MetaData,
    files: Vec<File>,
}

impl FileList {
    /// Gets FileList from a request path
    ///
    /// All paths stored in database have this format:
    ///
    /// ```text
    /// [
    ///   ["text/plain", ".gitignore"],
    ///   ["text/x-c", "src/reseeding.rs"],
    ///   ["text/x-c", "src/lib.rs"],
    ///   ["text/x-c", "README.md"],
    ///   ...
    /// ]
    /// ```
    ///
    /// This function is only returning FileList for requested directory. If is empty,
    /// it will return list of files (and dirs) for root directory. req_path must be a
    /// directory or empty for root directory.
    fn from_path(
        conn: &mut Client,
        name: &str,
        version: &str,
        version_or_latest: &str,
        req_path: &str,
    ) -> Option<FileList> {
        let rows = conn
            .query(
                "SELECT crates.name,
                        releases.version,
                        releases.description,
                        releases.target_name,
                        releases.rustdoc_status,
                        releases.files,
                        releases.default_target,
                        releases.doc_targets,
                        releases.yanked,
                        releases.doc_rustc_version
                FROM releases
                LEFT OUTER JOIN crates ON crates.id = releases.crate_id
                WHERE crates.name = $1 AND releases.version = $2",
                &[&name, &version],
            )
            .unwrap();

        if rows.is_empty() {
            return None;
        }

        let files: Value = rows[0].try_get(5).ok()?;

        let mut file_list = Vec::new();
        if let Some(files) = files.as_array() {
            file_list.reserve(files.len());

            for file in files {
                if let Some(file) = file.as_array() {
                    let mime = file[0].as_str().unwrap();
                    let path = file[1].as_str().unwrap();

                    // skip .cargo-ok generated by cargo
                    if path == ".cargo-ok" {
                        continue;
                    }

                    // look only files for req_path
                    if let Some(path) = path.strip_prefix(req_path) {
                        let file = File::from_path_and_mime(path, mime);

                        // avoid adding duplicates, a directory may occur more than once
                        if !file_list.contains(&file) {
                            file_list.push(file);
                        }
                    }
                }
            }

            if file_list.is_empty() {
                return None;
            }

            file_list.sort_by(|a, b| {
                // directories must be listed first
                if a.mime == "dir" && b.mime != "dir" {
                    Ordering::Less
                } else if a.mime != "dir" && b.mime == "dir" {
                    Ordering::Greater
                } else {
                    a.name.to_lowercase().cmp(&b.name.to_lowercase())
                }
            });

            Some(FileList {
                metadata: MetaData {
                    name: rows[0].get(0),
                    version: rows[0].get(1),
                    version_or_latest: version_or_latest.to_string(),
                    description: rows[0].get(2),
                    target_name: rows[0].get(3),
                    rustdoc_status: rows[0].get(4),
                    default_target: rows[0].get(6),
                    doc_targets: MetaData::parse_doc_targets(rows[0].get(7)),
                    yanked: rows[0].get(8),
                    rustdoc_css_file: get_correct_docsrs_style_file(rows[0].get(9)).unwrap(),
                },
                files: file_list,
            })
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct SourcePage {
    file_list: FileList,
    show_parent_link: bool,
    file: Option<File>,
    file_content: Option<String>,
    canonical_url: String,
}

impl_webpage! {
    SourcePage = "crate/source.html",
}

pub fn source_browser_handler(req: &mut Request) -> IronResult<Response> {
    let router = extension!(req, Router);
    let mut crate_name = cexpect!(req, router.find("name"));
    let req_version = cexpect!(req, router.find("version"));
    let pool = extension!(req, Pool);
    let mut conn = pool.get()?;

    let mut req_path = req.url.path();
    // remove first elements from path which is /crate/:name/:version/source
    req_path.drain(0..4);

    let v = match_version(&mut conn, crate_name, Some(req_version))?;
    if let Some(new_name) = &v.corrected_name {
        // `match_version` checked against -/_ typos, so if we have a name here we should
        // use that instead
        crate_name = new_name;
    }
    let (version, version_or_latest) = match v.version {
        MatchSemver::Latest((version, _)) => (version, "latest".to_string()),
        MatchSemver::Exact((version, _)) => (version.clone(), version),
        MatchSemver::Semver((version, _)) => {
            let url = ctry!(
                req,
                Url::parse(&format!(
                    "{}/crate/{}/{}/source/{}",
                    redirect_base(req),
                    crate_name,
                    version,
                    req_path.join("/"),
                )),
            );

            return Ok(super::redirect(url));
        }
    };

    // get path (req_path) for FileList::from_path and actual path for super::file::File::from_path
    let (req_path, file_path) = {
        let mut req_path = req.url.path();
        // remove first elements from path which is /crate/:name/:version/source
        for _ in 0..4 {
            req_path.remove(0);
        }
        let file_path = req_path.join("/");

        // FileList::from_path is only working for directories
        // remove file name if it's not a directory
        if let Some(last) = req_path.last_mut() {
            if !last.is_empty() {
                *last = "";
            }
        }

        // remove crate name and version from req_path
        let path = req_path
            .join("/")
            .replace(&format!("{}/{}/", crate_name, version), "");

        (path, file_path)
    };

    let canonical_url = format!(
        "https://docs.rs/crate/{}/latest/source/{}",
        crate_name, file_path
    );

    let storage = extension!(req, Storage);
    let archive_storage: bool = {
        let rows = ctry!(
            req,
            conn.query(
                "
                SELECT archive_storage
                FROM releases
                INNER JOIN crates ON releases.crate_id = crates.id
                WHERE
                    name = $1 AND
                    version = $2
                ",
                &[&crate_name, &version]
            )
        );
        // this unwrap is safe because `match_version` guarantees that the `crate_name`/`version`
        // combination exists.
        let row = rows.get(0).unwrap();

        row.get::<_, bool>(0)
    };

    // try to get actual file first
    // skip if request is a directory
    let blob = if !file_path.ends_with('/') {
        storage
            .fetch_source_file(crate_name, &version, &file_path, archive_storage)
            .ok()
    } else {
        None
    };

    let (file, file_content) = if let Some(blob) = blob {
        let is_text = blob.mime.starts_with("text") || blob.mime == "application/json";
        // serve the file with DatabaseFileHandler if file isn't text and not empty
        if !is_text && !blob.is_empty() {
            return Ok(DbFile(blob).serve());
        } else if is_text && !blob.is_empty() {
            let path = blob
                .path
                .rsplit_once('/')
                .map(|(_, path)| path)
                .unwrap_or(&blob.path);
            (
                Some(File::from_path_and_mime(path, &blob.mime)),
                String::from_utf8(blob.content).ok(),
            )
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    let file_list = FileList::from_path(
        &mut conn,
        crate_name,
        &version,
        &version_or_latest,
        &req_path,
    )
    .ok_or(Nope::ResourceNotFound)?;

    SourcePage {
        file_list,
        show_parent_link: !req_path.is_empty(),
        file,
        file_content,
        canonical_url,
    }
    .into_response(req)
}

#[cfg(test)]
mod tests {
    use crate::test::*;
    use test_case::test_case;

    #[test_case(true)]
    #[test_case(false)]
    fn fetch_source_file_content(archive_storage: bool) {
        wrapper(|env| {
            env.fake_release()
                .archive_storage(archive_storage)
                .name("fake")
                .version("0.1.0")
                .source_file("some_filename.rs", b"some_random_content")
                .create()?;
            let web = env.frontend();
            assert_success("/crate/fake/0.1.0/source/", web)?;
            let response = web
                .get("/crate/fake/0.1.0/source/some_filename.rs")
                .send()?;
            assert!(response.status().is_success());
            assert!(response.text()?.contains("some_random_content"));
            Ok(())
        });
    }

    #[test_case(true)]
    #[test_case(false)]
    fn cargo_ok_not_skipped(archive_storage: bool) {
        wrapper(|env| {
            env.fake_release()
                .archive_storage(archive_storage)
                .name("fake")
                .version("0.1.0")
                .source_file(".cargo-ok", b"ok")
                .source_file("README.md", b"hello")
                .create()?;
            let web = env.frontend();
            assert_success("/crate/fake/0.1.0/source/", web)?;
            Ok(())
        });
    }

    #[test]
    fn latest_contains_links_to_latest() {
        wrapper(|env| {
            env.fake_release()
                .archive_storage(true)
                .name("fake")
                .version("0.1.0")
                .source_file(".cargo-ok", b"ok")
                .source_file("README.md", b"hello")
                .create()?;
            let resp = env.frontend().get("/crate/fake/latest/source/").send()?;
            assert!(resp.url().as_str().ends_with("/crate/fake/latest/source/"));
            let body = String::from_utf8(resp.bytes().unwrap().to_vec()).unwrap();
            assert!(body.contains("<a href=\"/crate/fake/latest/builds\""));
            assert!(body.contains("<a href=\"/crate/fake/latest/source/\""));
            assert!(body.contains("<a href=\"/crate/fake/latest\""));
            assert!(body.contains("<a href=\"/crate/fake/latest/features\""));

            Ok(())
        });
    }

    #[test_case(true)]
    #[test_case(false)]
    fn directory_not_found(archive_storage: bool) {
        wrapper(|env| {
            env.fake_release()
                .archive_storage(archive_storage)
                .name("mbedtls")
                .version("0.2.0")
                .create()?;
            let web = env.frontend();
            assert_not_found("/crate/mbedtls/0.2.0/source/test/", web)?;
            Ok(())
        })
    }

    #[test_case(true)]
    #[test_case(false)]
    fn semver_handled(archive_storage: bool) {
        wrapper(|env| {
            env.fake_release()
                .archive_storage(archive_storage)
                .name("mbedtls")
                .version("0.2.0")
                .source_file("README.md", b"hello")
                .create()?;
            let web = env.frontend();
            assert_success("/crate/mbedtls/0.2.0/source/", web)?;
            assert_redirect(
                "/crate/mbedtls/*/source/",
                "/crate/mbedtls/0.2.0/source/",
                web,
            )?;
            Ok(())
        })
    }

    #[test_case(true)]
    #[test_case(false)]
    fn literal_krate_description(archive_storage: bool) {
        wrapper(|env| {
            env.fake_release()
                .archive_storage(archive_storage)
                .name("rustc-ap-syntax")
                .version("178.0.0")
                .description("some stuff with krate")
                .source_file("fold.rs", b"fn foo() {}")
                .create()?;
            let web = env.frontend();
            assert_success("/crate/rustc-ap-syntax/178.0.0/source/fold.rs", web)?;
            Ok(())
        })
    }

    #[test]
    fn cargo_special_filetypes_are_highlighted() {
        wrapper(|env| {
            env.fake_release()
                .name("fake")
                .version("0.1.0")
                .source_file("Cargo.toml.orig", b"[package]")
                .source_file("Cargo.lock", b"[dependencies]")
                .create()?;

            let web = env.frontend();

            let response = web
                .get("/crate/fake/0.1.0/source/Cargo.toml.orig")
                .send()?
                .text()?;
            assert!(response.contains(r#"<span class="syntax-source syntax-toml">"#));

            let response = web
                .get("/crate/fake/0.1.0/source/Cargo.lock")
                .send()?
                .text()?;
            assert!(response.contains(r#"<span class="syntax-source syntax-toml">"#));

            Ok(())
        });
    }

    #[test]
    fn dotfiles_with_extension_are_highlighted() {
        wrapper(|env| {
            env.fake_release()
                .name("fake")
                .version("0.1.0")
                .source_file(".rustfmt.toml", b"[rustfmt]")
                .create()?;

            let web = env.frontend();

            let response = web
                .get("/crate/fake/0.1.0/source/.rustfmt.toml")
                .send()?
                .text()?;
            assert!(response.contains(r#"<span class="syntax-source syntax-toml">"#));

            Ok(())
        });
    }

    #[test]
    fn json_is_served_as_rendered_html() {
        wrapper(|env| {
            env.fake_release()
                .name("fake")
                .version("0.1.0")
                .source_file("config.json", b"{}")
                .create()?;

            let web = env.frontend();

            let response = web.get("/crate/fake/0.1.0/source/config.json").send()?;
            assert!(response
                .headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/html"));
            assert!(response.text()?.starts_with(r#"<!DOCTYPE html>"#));

            Ok(())
        });
    }
}
