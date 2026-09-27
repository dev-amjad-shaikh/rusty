//! Importing skills from elsewhere.
//!
//! `POST /skills/import` reads one gzipped tar archive over the
//! deployment's egress policy, finds every `SKILL.md` in it, and registers
//! each package through exactly the path `POST /skills` uses — parse, the
//! security scan, an immutable content-addressed version. Nothing here
//! weakens a rule to make an import succeed: a package the importer
//! accepts is one the registry accepts, and one it refuses is reported by
//! path with the reason.
//!
//! `GET /skills/library` lists the places the deployment suggests importing
//! from ([`SkillLibrarySource`], configured by the embedder). They are
//! pointers: the server fetches a source when a person asks, never at boot.
//!
//! A GitHub repository URL is the common case and resolves to its codeload
//! tarball (`https://codeload.github.com/{owner}/{repo}/tar.gz/{ref}`); any
//! `https` URL that names a `.tar.gz` / `.tgz` is read as is. Members
//! outside the closed package shape — `SKILL.md`, `references/`, `assets/`
//! — are not imported and are named per skill in the report, so a builder
//! can see what a skill's text may refer to that the server does not hold
//! (a `scripts/` directory is the usual case).

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State as AxumState;
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;
use rusty_agent_runtime::connector::{CheckRequest, ConnectorTransport, HttpMethod};
use rusty_agent_runtime::skill::{SkillError, SkillPackage, SkillSource};

/// The most compressed bytes an archive may be.
const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
/// The most bytes an archive may unpack to, summed over every member read.
const MAX_UNPACKED_BYTES: usize = 256 * 1024 * 1024;
/// The largest single member read. Larger ones are reported, not read —
/// no skill member may exceed core's ceiling anyway.
const MAX_ENTRY_BYTES: usize = 4 * 1024 * 1024;
/// The most archive entries walked.
const MAX_ENTRIES: usize = 50_000;
/// The most skills one import registers.
const MAX_SKILLS: usize = 500;
/// How long the fetch may take end to end.
const FETCH_TIMEOUT: Duration = Duration::from_secs(90);

/// One place a builder can import skills from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillLibrarySource {
    /// Stable id for the row.
    pub id: String,
    /// What to call it.
    pub name: String,
    /// The repository or archive URL the import reads.
    pub url: String,
    /// What a builder finds there.
    pub description: String,
    /// Who publishes it.
    pub publisher: String,
    /// The licence, when one covers the whole source.
    #[serde(default)]
    pub license: Option<String>,
    /// A path inside the archive to confine the import to (`skills`).
    #[serde(default)]
    pub subpath: Option<String>,
}

/// `GET /skills/library` — the configured sources.
pub(crate) async fn list_library(AxumState(state): AxumState<Arc<AppState>>) -> Json<Value> {
    Json(json!({ "sources": state.skill_library }))
}

/// `POST /skills/import` payload.
#[derive(Debug, Deserialize)]
pub(crate) struct ImportSkillsPayload {
    /// A GitHub repository URL (optionally `/tree/{ref}/{path}`) or a
    /// direct `.tar.gz` / `.tgz` URL. https only.
    url: String,
    /// The git ref to read, for a repository URL. Defaults to `HEAD`.
    #[serde(default, rename = "ref")]
    git_ref: Option<String>,
    /// A path inside the archive to confine the import to.
    #[serde(default)]
    subpath: Option<String>,
    /// Who the versions are registered by. Defaults to the signed-in
    /// principal.
    #[serde(default)]
    author: Option<String>,
}

/// The archive a URL means.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Archive {
    /// What is fetched.
    pub url: String,
    /// The path inside it the import is confined to.
    pub subpath: Option<String>,
    /// How the provenance names it: repository, ref and path for a
    /// forge URL; the URL itself otherwise.
    pub display: String,
}

/// Resolve a URL a builder typed to the archive it names.
pub(crate) fn archive_for(
    url: &str,
    git_ref: Option<&str>,
    subpath: Option<&str>,
) -> Result<Archive, String> {
    let trimmed = url.trim().trim_end_matches('/');
    let Some(rest) = trimmed.strip_prefix("https://") else {
        return Err(
            "the URL must start with https:// — an archive is read over TLS or not at all"
                .to_owned(),
        );
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let subpath = subpath
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_matches('/').to_owned());

    if host.eq_ignore_ascii_case("github.com") || host.eq_ignore_ascii_case("www.github.com") {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        if parts.len() < 2 {
            return Err(
                "a GitHub URL names an owner and a repository: https://github.com/{owner}/{repo}"
                    .to_owned(),
            );
        }
        let owner = parts[0];
        let repo = parts[1].trim_end_matches(".git");
        // `/tree/{ref}/{path…}` — the ref and path a browser URL carries.
        let (tree_ref, tree_path) = match parts.get(2) {
            Some(&"tree") | Some(&"blob") if parts.len() > 3 => (
                Some(parts[3]),
                (parts.len() > 4).then(|| parts[4..].join("/")),
            ),
            _ => (None, None),
        };
        let git_ref = git_ref
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .or(tree_ref)
            .unwrap_or("HEAD");
        let ref_ok = !git_ref.contains("..")
            && git_ref
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'));
        if !ref_ok {
            return Err(format!("`{git_ref}` is not a git ref"));
        }
        let subpath = subpath.or(tree_path);
        let at_path = subpath
            .as_deref()
            .map(|p| format!("/{p}"))
            .unwrap_or_default();
        return Ok(Archive {
            url: format!("https://codeload.github.com/{owner}/{repo}/tar.gz/{git_ref}"),
            display: format!("https://github.com/{owner}/{repo}@{git_ref}{at_path}"),
            subpath,
        });
    }

    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return Ok(Archive {
            url: trimmed.to_owned(),
            display: trimmed.to_owned(),
            subpath,
        });
    }
    Err("only a GitHub repository URL or a direct .tar.gz / .tgz URL can be imported".to_owned())
}

/// One skill found in an archive: the directory around a `SKILL.md`.
#[derive(Debug)]
pub(crate) struct FoundSkill {
    /// The directory inside the archive.
    pub path: String,
    /// The members inside the closed package shape, keyed as
    /// [`SkillPackage::from_files`] expects.
    pub files: BTreeMap<String, Vec<u8>>,
    /// The members outside it, by relative path — not imported, named.
    pub not_imported: Vec<String>,
}

/// What an archive held.
#[derive(Debug, Default)]
pub(crate) struct Unpacked {
    pub skills: Vec<FoundSkill>,
    /// Members too large to read, by path.
    pub oversized: Vec<String>,
}

/// A path as the archive names it, without a leading `./` or `/`.
fn normalize(path: &str) -> &str {
    let mut p = path;
    loop {
        if let Some(rest) = p.strip_prefix("./") {
            p = rest;
        } else if let Some(rest) = p.strip_prefix('/') {
            p = rest;
        } else {
            return p;
        }
    }
}

/// Whether a skill directory sits under the requested subpath. A forge
/// archive wraps the tree in one top-level directory (`{repo}-{sha}/`),
/// so the subpath is matched with and without that first component.
pub(crate) fn within(root: &str, subpath: Option<&str>) -> bool {
    let Some(sub) = subpath else {
        return true;
    };
    let prefix = format!("{sub}/");
    if root.starts_with(&prefix) {
        return true;
    }
    match root.split_once('/') {
        Some((_, rest)) => rest.starts_with(&prefix),
        None => false,
    }
}

/// Every regular file in a gzipped tar archive, by normalized path.
#[derive(Debug, Default)]
pub(crate) struct Files {
    pub files: BTreeMap<String, Vec<u8>>,
    /// Entries walked.
    pub entries: usize,
    /// Members too large to read, by path.
    pub oversized: Vec<String>,
}

/// Read a gzipped tar archive within the ceilings: regular files only (a
/// link escapes any package shape by construction), no `..` segment, no
/// member past [`MAX_ENTRY_BYTES`], the whole past [`MAX_UNPACKED_BYTES`]
/// refused.
pub(crate) fn unpack_files(archive: &[u8]) -> Result<Files, String> {
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    let mut unpacked = Files::default();
    let mut total = 0usize;

    let entries = tar
        .entries()
        .map_err(|e| format!("not a gzipped tar archive: {e}"))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|e| format!("not a gzipped tar archive, or a truncated one: {e}"))?;
        unpacked.entries += 1;
        if unpacked.entries > MAX_ENTRIES {
            return Err(format!("the archive has more than {MAX_ENTRIES} entries"));
        }
        // Only a regular file can be a member; a link escapes the package
        // shape by construction and a directory is implied by its files.
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = match entry.path() {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        let path = normalize(&path).to_owned();
        if path.is_empty() || path.split('/').any(|seg| seg == "..") {
            continue;
        }
        let size = entry.header().size().unwrap_or(0) as usize;
        if size > MAX_ENTRY_BYTES {
            unpacked.oversized.push(path);
            continue;
        }
        total = total.saturating_add(size);
        if total > MAX_UNPACKED_BYTES {
            return Err(format!(
                "the archive unpacks past the {MAX_UNPACKED_BYTES}-byte ceiling"
            ));
        }
        let mut bytes = Vec::with_capacity(size);
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| format!("archive entry `{path}` unreadable: {e}"))?;
        unpacked.files.insert(path, bytes);
    }
    Ok(unpacked)
}

/// Read a gzipped tar archive and find every skill in it.
pub(crate) fn unpack_skills(archive: &[u8], subpath: Option<&str>) -> Result<Unpacked, String> {
    let Files {
        files, oversized, ..
    } = unpack_files(archive)?;
    let mut unpacked = Unpacked {
        skills: Vec::new(),
        oversized,
    };

    // A skill is wherever a SKILL.md is; the directory around it is the
    // package.
    let roots: Vec<String> = files
        .keys()
        .filter(|p| p.as_str() == "SKILL.md" || p.ends_with("/SKILL.md"))
        .map(|p| p[..p.len() - "SKILL.md".len()].to_owned())
        .collect();
    for root in roots {
        if !within(&root, subpath) {
            continue;
        }
        let mut members = BTreeMap::new();
        let mut not_imported = Vec::new();
        for (path, bytes) in files.range(root.clone()..) {
            if !path.starts_with(&root) {
                break;
            }
            let rel = &path[root.len()..];
            if rel == "SKILL.md" || rel.starts_with("references/") || rel.starts_with("assets/") {
                members.insert(rel.to_owned(), bytes.clone());
            } else {
                not_imported.push(rel.to_owned());
            }
        }
        unpacked.skills.push(FoundSkill {
            path: root.trim_end_matches('/').to_owned(),
            files: members,
            not_imported,
        });
        if unpacked.skills.len() >= MAX_SKILLS {
            break;
        }
    }
    Ok(unpacked)
}

/// Fetch an archive over the deployment's egress policy. The fetch rides
/// the same transport a connector check does: egress policy evaluated
/// first, DNS preflighted, the socket pinned to the address preflight
/// approved, redirects re-evaluated. `purpose` names the caller in the
/// transport's log line and the User-Agent.
pub(crate) async fn fetch_archive(
    state: &AppState,
    archive: &Archive,
    purpose: &str,
) -> Result<Vec<u8>, ApiError> {
    let host = reqwest::Url::parse(&archive.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .ok_or_else(|| ApiError::bad_request("the archive URL has no host".to_owned()))?;
    let policy = Some(Arc::new(crate::connectors::effective_egress_policy(
        state,
        Some(&host),
    )));
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| ApiError::internal(format!("http client: {e}")))?;
    let transport = crate::connectors::ReqwestConnectorTransport::new(client, policy, purpose);
    let response = transport
        .send(CheckRequest {
            method: HttpMethod::Get,
            url: archive.url.clone(),
            headers: vec![("user-agent".to_owned(), format!("rusty-server/{purpose}"))],
            timeout: FETCH_TIMEOUT,
            max_response_bytes: MAX_ARCHIVE_BYTES,
            body: None,
        })
        .await
        .map_err(|error| {
            let message = error.to_string();
            if message.contains("egress") {
                ApiError::forbidden(message)
            } else {
                ApiError::new(StatusCode::BAD_GATEWAY, "fetch_failed", message)
            }
        })?;
    if !(200..300).contains(&response.status) {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "fetch_failed",
            format!("{} answered {}", archive.display, response.status),
        ));
    }
    Ok(response.body)
}

/// `POST /skills/import` — fetch, unpack, register; report every skill by
/// path as imported (name, revision, what was left behind) or skipped
/// (the reason, verbatim from the validator or the scan).
pub(crate) async fn import_skills(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<ImportSkillsPayload>,
) -> Result<Json<Value>, ApiError> {
    let archive = archive_for(
        &payload.url,
        payload.git_ref.as_deref(),
        payload.subpath.as_deref(),
    )
    .map_err(ApiError::bad_request)?;
    let author = match payload.author {
        None => tenant.principal().name.clone(),
        Some(author) if author.trim().is_empty() => {
            return Err(ApiError::bad_request(
                "the author, when given, is not empty — leave it out to import as yourself"
                    .to_owned(),
            ));
        }
        Some(author) => author.trim().to_owned(),
    };

    let body = fetch_archive(&state, &archive, "skills-import").await?;

    let subpath = archive.subpath.clone();
    let unpacked = tokio::task::spawn_blocking(move || unpack_skills(&body, subpath.as_deref()))
        .await
        .map_err(|e| ApiError::internal(format!("unpack: {e}")))?
        .map_err(ApiError::bad_request)?;

    let found = unpacked.skills.len();
    let mut imported = Vec::new();
    let mut skipped = Vec::new();
    for skill in unpacked.skills {
        let package = match SkillPackage::from_files(skill.files) {
            Ok(package) => package,
            Err(error) => {
                skipped.push(json!({ "path": skill.path, "reason": error.to_string() }));
                continue;
            }
        };
        let source = SkillSource::Url {
            url: archive.display.clone(),
        };
        match state
            .skills
            .register(tenant.tenant(), package, source, author.clone())
            .await
        {
            Ok(registration) => imported.push(json!({
                "name": registration.version.name(),
                "revision": registration.version.revision(),
                "content_hash": registration.version.content_hash(),
                "already_registered": registration.already_registered,
                "path": skill.path,
                "not_imported": skill.not_imported,
            })),
            Err(SkillError::ScanDenied { denials }) => skipped.push(json!({
                "path": skill.path,
                "reason": format!("the security scan denied it: {} finding(s)", denials.len()),
                "findings": denials,
            })),
            Err(error) => skipped.push(json!({ "path": skill.path, "reason": error.to_string() })),
        }
    }

    Ok(Json(json!({
        "source": archive.display,
        "found": found,
        "imported": imported,
        "skipped": skipped,
        "oversized": unpacked.oversized,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn targz(entries: &[(&str, &[u8])], links: &[(&str, &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *bytes).unwrap();
        }
        for (path, target) in links {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_cksum();
            builder.append_link(&mut header, path, target).unwrap();
        }
        let tar_bytes = builder.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&tar_bytes).unwrap();
        encoder.finish().unwrap()
    }

    const SKILL_A: &[u8] =
        b"---\nname: alpha\ndescription: The first skill.\n---\n\n# Alpha\n\nDo the thing.\n";
    const SKILL_B: &[u8] = b"---\nname: beta\ndescription: The second skill.\n---\n\n# Beta\n";

    #[test]
    fn a_github_repository_url_is_its_codeload_tarball() {
        let archive = archive_for("https://github.com/acme/skills", None, None).unwrap();
        assert_eq!(
            archive.url,
            "https://codeload.github.com/acme/skills/tar.gz/HEAD"
        );
        assert_eq!(archive.display, "https://github.com/acme/skills@HEAD");
        assert_eq!(archive.subpath, None);

        let archive = archive_for(
            "https://github.com/acme/skills.git/",
            Some("v2"),
            Some("/packs/"),
        )
        .unwrap();
        assert_eq!(
            archive.url,
            "https://codeload.github.com/acme/skills/tar.gz/v2"
        );
        assert_eq!(archive.display, "https://github.com/acme/skills@v2/packs");
        assert_eq!(archive.subpath.as_deref(), Some("packs"));
    }

    #[test]
    fn a_browser_tree_url_carries_its_ref_and_path() {
        let archive = archive_for(
            "https://github.com/acme/skills/tree/main/skills/docx",
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            archive.url,
            "https://codeload.github.com/acme/skills/tar.gz/main"
        );
        assert_eq!(archive.subpath.as_deref(), Some("skills/docx"));
        // An explicit ref wins over the one in the URL.
        let archive = archive_for(
            "https://github.com/acme/skills/tree/main",
            Some("release"),
            None,
        )
        .unwrap();
        assert_eq!(
            archive.url,
            "https://codeload.github.com/acme/skills/tar.gz/release"
        );
    }

    #[test]
    fn only_https_and_only_archives_or_forges() {
        assert!(archive_for("http://github.com/acme/skills", None, None)
            .unwrap_err()
            .contains("https://"));
        assert!(archive_for("https://github.com/acme", None, None)
            .unwrap_err()
            .contains("owner and a repository"));
        assert!(
            archive_for("https://github.com/acme/skills", Some("../x"), None)
                .unwrap_err()
                .contains("not a git ref")
        );
        assert!(archive_for("https://example.com/skills", None, None)
            .unwrap_err()
            .contains("GitHub repository URL or a direct"));
        let archive = archive_for("https://example.com/dl/skills.TGZ", None, None).unwrap();
        assert_eq!(archive.url, "https://example.com/dl/skills.TGZ");
    }

    #[test]
    fn every_skill_md_is_a_package_and_the_rest_is_named() {
        let archive = targz(
            &[
                ("repo-abc123/README.md", b"top level"),
                ("repo-abc123/skills/alpha/SKILL.md", SKILL_A),
                ("repo-abc123/skills/alpha/references/guide.md", b"guide"),
                ("repo-abc123/skills/alpha/scripts/run.py", b"print(1)"),
                ("repo-abc123/skills/alpha/LICENSE", b"MIT"),
                ("./repo-abc123/skills/beta/SKILL.md", SKILL_B),
                ("repo-abc123/other/notes.md", b"not a skill"),
            ],
            &[("repo-abc123/skills/alpha/link", "../../etc/passwd")],
        );
        let unpacked = unpack_skills(&archive, None).unwrap();
        assert_eq!(unpacked.skills.len(), 2, "{unpacked:?}");
        let alpha = &unpacked.skills[0];
        assert_eq!(alpha.path, "repo-abc123/skills/alpha");
        assert_eq!(
            alpha.files.keys().cloned().collect::<Vec<_>>(),
            vec!["SKILL.md".to_owned(), "references/guide.md".to_owned()]
        );
        assert_eq!(
            alpha.not_imported,
            vec!["LICENSE".to_owned(), "scripts/run.py".to_owned()]
        );
        assert_eq!(unpacked.skills[1].path, "repo-abc123/skills/beta");
        // The members become a package the registry accepts.
        let package = SkillPackage::from_files(alpha.files.clone()).unwrap();
        assert_eq!(package.name(), "alpha");
    }

    #[test]
    fn a_subpath_confines_the_import_with_or_without_the_forge_prefix() {
        let archive = targz(
            &[
                ("repo-abc123/skills/alpha/SKILL.md", SKILL_A),
                ("repo-abc123/drafts/beta/SKILL.md", SKILL_B),
            ],
            &[],
        );
        let skills = unpack_skills(&archive, Some("skills")).unwrap().skills;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].path, "repo-abc123/skills/alpha");
        let skills = unpack_skills(&archive, Some("repo-abc123/drafts"))
            .unwrap()
            .skills;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].path, "repo-abc123/drafts/beta");
        assert!(unpack_skills(&archive, Some("nowhere"))
            .unwrap()
            .skills
            .is_empty());
    }

    #[test]
    fn bytes_that_are_not_an_archive_say_so() {
        let error = unpack_skills(b"definitely not gzip", None).unwrap_err();
        assert!(error.contains("not a gzipped tar archive"), "{error}");
    }
}
