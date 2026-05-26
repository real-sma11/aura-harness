//! AGENTS.md probe + injection.
//!
//! The historical `append_agents_md` lived inline in
//! `prompts/system/mod.rs`. PR B relocates the probe + render here and
//! adds the hardening called out in the simplification plan:
//!
//! 1. A structured [`AgentsMdProbe`] outcome enum so callers (chiefly
//!    [`super::super::SystemPromptBuilder`]) can observe whether the
//!    file was found, missing, oversized, or whether the workspace
//!    path wasn't a directory at all.
//! 2. A `tracing::info!` log on the `Found` branch so operators get a
//!    positive confirmation that AGENTS.md reached the model — until
//!    PR B the only signal was a `warn!` on the size cap.
//! 3. Cross-platform unit tests covering each probe outcome.
//!
//! The injected section's *byte layout* is unchanged in PR B (the
//! header literal and surrounding whitespace are preserved verbatim)
//! so the four PR A golden snapshots keep passing without an
//! `UPDATE_SNAPSHOTS` regeneration.

use std::path::Path;

/// Hard cap on AGENTS.md bytes injected into the system prompt. Larger
/// files are skipped (with a `warn!` log) rather than truncated so the
/// agent never reads a half-instruction.
pub(crate) const AGENTS_MD_MAX_BYTES: usize = 64 * 1024;

/// Opening tag used when an AGENTS.md is found at the workspace root.
/// PR C flips the section from the legacy `## Project AGENTS.md`
/// markdown header to the canonical `<agents_md path="...">` envelope.
/// Tests assert on this string instead of duplicating the literal.
pub(crate) const AGENTS_MD_SECTION_TAG_PREFIX: &str = "<agents_md path=\"";

/// Filename variants the probe walks in order. The first read that
/// succeeds and fits the byte cap wins. We try a small explicit set
/// instead of doing a full directory scan: the AGENTS.md convention
/// is well-defined and three `fs::read_to_string` probes are cheaper
/// than enumerating the workspace root (and on case-insensitive
/// filesystems all three resolve to the same inode anyway).
const AGENTS_MD_VARIANTS: &[&str] = &["AGENTS.md", "agents.md", "Agents.md"];

/// Structured outcome of a single AGENTS.md probe attempt.
///
/// Surfaced through the builder so operators have a concrete event to
/// observe ("did the model see AGENTS.md or not?") rather than the
/// pre-PR-B silent best-effort behaviour. The wire layer in PR C / D
/// will lift this onto the run event stream; for now it's available
/// to callers via [`super::super::SystemPromptBuilder::agents_md_probe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentsMdProbe {
    /// File found on disk, fits the byte cap, and was injected into
    /// the prompt. `path` is the resolved absolute / verbatim path,
    /// `bytes` is the file size, `variant` is which casing matched
    /// first (`AGENTS.md`, `agents.md`, or `Agents.md`).
    Found {
        path: String,
        bytes: usize,
        variant: &'static str,
    },
    /// `folder_path` is a real directory but no AGENTS.md variant is
    /// present. The common case for fresh workspaces.
    Missing { folder: String },
    /// File was found but exceeded [`AGENTS_MD_MAX_BYTES`] and was
    /// therefore skipped (not truncated). The `warn!` log is still
    /// emitted alongside this outcome so operators are notified.
    OverCap {
        path: String,
        bytes: usize,
        cap: usize,
    },
    /// `folder_path` did not point at a directory. The historical
    /// helper silently no-op'd in this case; the probe surfaces it so
    /// the dev-loop's `effective_project_path` resolution can be
    /// audited if AGENTS.md unexpectedly fails to inject.
    NotADir { folder: String },
}

/// Read result bundling a [`AgentsMdProbe`] with the file content, when
/// applicable. Internal-only so we can probe and read in a single pass
/// without exposing the content on the public enum (the plan pins the
/// public shape).
struct AgentsMdRead {
    probe: AgentsMdProbe,
    content: Option<String>,
}

fn read(folder_path: &str) -> AgentsMdRead {
    let folder = Path::new(folder_path);
    if !folder.is_dir() {
        return AgentsMdRead {
            probe: AgentsMdProbe::NotADir {
                folder: folder_path.to_string(),
            },
            content: None,
        };
    }
    for variant in AGENTS_MD_VARIANTS {
        let path = folder.join(variant);
        match std::fs::read_to_string(&path) {
            Ok(content) if content.len() <= AGENTS_MD_MAX_BYTES => {
                return AgentsMdRead {
                    probe: AgentsMdProbe::Found {
                        path: path.to_string_lossy().into_owned(),
                        bytes: content.len(),
                        variant,
                    },
                    content: Some(content),
                };
            }
            Ok(content) => {
                return AgentsMdRead {
                    probe: AgentsMdProbe::OverCap {
                        path: path.to_string_lossy().into_owned(),
                        bytes: content.len(),
                        cap: AGENTS_MD_MAX_BYTES,
                    },
                    content: None,
                };
            }
            Err(_) => continue,
        }
    }
    AgentsMdRead {
        probe: AgentsMdProbe::Missing {
            folder: folder_path.to_string(),
        },
        content: None,
    }
}

/// Public probe entry-point: returns the structured outcome without
/// holding the file content. Useful for tests or any caller that wants
/// to surface the probe verdict (e.g. on the run event stream) without
/// reaching into the rendered prompt.
#[must_use]
pub fn probe_agents_md(folder_path: &str) -> AgentsMdProbe {
    read(folder_path).probe
}

/// Append the AGENTS.md section to `prompt` (when one is found),
/// returning the structured outcome and emitting the appropriate
/// `tracing` event.
///
/// - `Found`  -> `tracing::info!` + section appended.
/// - `OverCap`-> `tracing::warn!` + nothing appended.
/// - Missing / NotADir -> `tracing::debug!` + nothing appended.
pub(crate) fn append(prompt: &mut String, folder_path: &str) -> AgentsMdProbe {
    let AgentsMdRead { probe, content } = read(folder_path);
    log_probe(&probe);
    if let (AgentsMdProbe::Found { ref variant, .. }, Some(body)) = (&probe, content) {
        prompt.push_str(&render_section(variant, &body));
    }
    probe
}

fn log_probe(probe: &AgentsMdProbe) {
    match probe {
        AgentsMdProbe::Found {
            path,
            bytes,
            variant,
        } => {
            tracing::info!(
                path = %path,
                bytes = bytes,
                variant = variant,
                "AGENTS.md injected into system prompt"
            );
        }
        AgentsMdProbe::OverCap { path, bytes, cap } => {
            tracing::warn!(
                path = %path,
                bytes = bytes,
                cap = cap,
                "AGENTS.md exceeded byte cap; skipping injection"
            );
        }
        AgentsMdProbe::Missing { folder } => {
            tracing::debug!(
                folder = %folder,
                "AGENTS.md probe found no AGENTS.md / agents.md / Agents.md at workspace root"
            );
        }
        AgentsMdProbe::NotADir { folder } => {
            tracing::debug!(
                folder = %folder,
                "AGENTS.md probe skipped: workspace folder is not a directory"
            );
        }
    }
}

fn render_section(variant: &str, content: &str) -> String {
    // PR C: the AGENTS.md body is the source of truth from the
    // operator, so we drop the "Treat them as authoritative ..."
    // preamble that the markdown-header rendering used and inline the
    // file body inside an `<agents_md path="<variant>">` envelope. The
    // `path=` attribute records which casing variant matched first so
    // case-sensitive vs case-insensitive filesystems are
    // distinguishable from the rendered prompt alone.
    let mut out = String::with_capacity(content.len() + 64);
    out.push_str(AGENTS_MD_SECTION_TAG_PREFIX);
    out.push_str(variant);
    out.push_str("\">\n");
    out.push_str(content);
    if !content.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("</agents_md>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_found_when_agents_md_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "guidance body").unwrap();
        let folder = dir.path().to_string_lossy().into_owned();

        let probe = probe_agents_md(&folder);

        match probe {
            AgentsMdProbe::Found { bytes, variant, .. } => {
                assert_eq!(bytes, "guidance body".len());
                // Cross-platform: case-insensitive FS may resolve any
                // of the three variants. Accept whichever matched
                // first.
                assert!(
                    matches!(variant, "AGENTS.md" | "agents.md" | "Agents.md"),
                    "unexpected variant {variant}",
                );
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn probe_returns_found_with_lowercase_variant() {
        let dir = tempfile::tempdir().unwrap();
        // Only the lowercase variant exists.
        std::fs::write(dir.path().join("agents.md"), "lower").unwrap();
        let folder = dir.path().to_string_lossy().into_owned();

        match probe_agents_md(&folder) {
            AgentsMdProbe::Found { variant, .. } => {
                // On case-insensitive filesystems (Windows / macOS
                // default) the first probe `AGENTS.md` may open the
                // same inode as `agents.md`. On case-sensitive
                // filesystems (Linux) the lowercase probe wins. Both
                // outcomes satisfy the contract.
                assert!(
                    matches!(variant, "AGENTS.md" | "agents.md"),
                    "expected lowercase or case-insensitive match, got {variant}",
                );
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn probe_returns_missing_when_folder_empty() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().to_string_lossy().into_owned();

        match probe_agents_md(&folder) {
            AgentsMdProbe::Missing { folder: probed } => {
                assert_eq!(probed, folder);
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn probe_returns_not_a_dir_when_path_is_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("not_a_dir.txt");
        std::fs::write(&file_path, "x").unwrap();
        let folder = file_path.to_string_lossy().into_owned();

        match probe_agents_md(&folder) {
            AgentsMdProbe::NotADir { folder: probed } => {
                assert_eq!(probed, folder);
            }
            other => panic!("expected NotADir, got {other:?}"),
        }
    }

    #[test]
    fn probe_returns_over_cap_when_file_too_big() {
        let dir = tempfile::tempdir().unwrap();
        let oversize = "x".repeat(AGENTS_MD_MAX_BYTES + 1);
        std::fs::write(dir.path().join("AGENTS.md"), &oversize).unwrap();
        let folder = dir.path().to_string_lossy().into_owned();

        match probe_agents_md(&folder) {
            AgentsMdProbe::OverCap { bytes, cap, .. } => {
                assert_eq!(bytes, oversize.len());
                assert_eq!(cap, AGENTS_MD_MAX_BYTES);
            }
            other => panic!("expected OverCap, got {other:?}"),
        }
    }

    #[test]
    fn append_writes_section_for_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "rule body").unwrap();
        let folder = dir.path().to_string_lossy().into_owned();

        let mut prompt = String::new();
        let probe = append(&mut prompt, &folder);

        assert!(matches!(probe, AgentsMdProbe::Found { .. }));
        assert!(prompt.contains(AGENTS_MD_SECTION_TAG_PREFIX));
        assert!(prompt.contains("</agents_md>"));
        assert!(prompt.contains("rule body"));
    }

    #[test]
    fn append_is_noop_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().to_string_lossy().into_owned();

        let mut prompt = String::from("seed");
        let probe = append(&mut prompt, &folder);

        assert!(matches!(probe, AgentsMdProbe::Missing { .. }));
        assert_eq!(prompt, "seed");
    }

    #[test]
    fn append_is_noop_for_oversize_file() {
        let dir = tempfile::tempdir().unwrap();
        let oversize = "x".repeat(AGENTS_MD_MAX_BYTES + 1);
        std::fs::write(dir.path().join("AGENTS.md"), &oversize).unwrap();
        let folder = dir.path().to_string_lossy().into_owned();

        let mut prompt = String::new();
        let probe = append(&mut prompt, &folder);

        assert!(matches!(probe, AgentsMdProbe::OverCap { .. }));
        assert!(prompt.is_empty());
    }
}
