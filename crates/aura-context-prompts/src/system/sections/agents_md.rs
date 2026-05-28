//! AGENTS.md probe + injection.
//!
//! Probes the workspace root for `AGENTS.md` (case-insensitive),
//! reads the file when present, and renders the body wrapped in the
//! canonical `<agents_md path="...">...</agents_md>` envelope. The
//! probe outcome is exposed as [`AgentsMdProbe`] so callers (chiefly
//! [`crate::system::SystemPromptBuilder`]) can surface "did the
//! model see AGENTS.md or not?" on operator dashboards / event
//! streams.

use std::path::Path;

/// Hard cap on AGENTS.md bytes injected into the system prompt.
/// Larger files are skipped (with a `warn!` log) rather than
/// truncated so the agent never reads a half-instruction.
pub const AGENTS_MD_MAX_BYTES: usize = 64 * 1024;

/// Opening tag used when an AGENTS.md is found at the workspace
/// root. Tests assert on this string instead of duplicating the
/// literal.
pub const AGENTS_MD_SECTION_TAG_PREFIX: &str = "<agents_md path=\"";

/// Filename variants the probe walks in order. The first read that
/// succeeds and fits the byte cap wins. We try a small explicit set
/// instead of doing a full directory scan: the AGENTS.md convention
/// is well-defined and three `fs::read_to_string` probes are cheaper
/// than enumerating the workspace root (and on case-insensitive
/// filesystems all three resolve to the same inode anyway).
const AGENTS_MD_VARIANTS: &[&str] = &["AGENTS.md", "agents.md", "Agents.md"];

/// Structured outcome of a single AGENTS.md probe attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentsMdProbe {
    /// File found on disk, fits the byte cap, and was injected into
    /// the prompt.
    Found {
        /// Resolved absolute / verbatim path of the matched file.
        path: String,
        /// Size of the file in bytes.
        bytes: usize,
        /// Which casing matched first
        /// (`AGENTS.md`, `agents.md`, or `Agents.md`).
        variant: &'static str,
    },
    /// `folder_path` is a real directory but no AGENTS.md variant is
    /// present.
    Missing {
        /// The folder path that was probed.
        folder: String,
    },
    /// File was found but exceeded [`AGENTS_MD_MAX_BYTES`] and was
    /// therefore skipped (not truncated).
    OverCap {
        /// Path of the oversize file.
        path: String,
        /// Actual size in bytes.
        bytes: usize,
        /// Configured cap.
        cap: usize,
    },
    /// `folder_path` did not point at a directory.
    NotADir {
        /// The path that was probed.
        folder: String,
    },
}

/// Read result bundling a [`AgentsMdProbe`] with the file content,
/// when applicable. Internal-only so we can probe and read in a
/// single pass without exposing the content on the public enum.
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
/// holding the file content. Useful for tests or any caller that
/// wants to surface the probe verdict (e.g. on the run event stream)
/// without reaching into the rendered prompt.
#[must_use]
pub fn probe_agents_md(folder_path: &str) -> AgentsMdProbe {
    read(folder_path).probe
}

/// Append the AGENTS.md section to `prompt` (when one is found),
/// returning the structured outcome and emitting the appropriate
/// `tracing` event.
pub fn append(prompt: &mut String, folder_path: &str) -> AgentsMdProbe {
    let AgentsMdRead { probe, content } = read(folder_path);
    log_probe(&probe);
    if let (AgentsMdProbe::Found { variant, .. }, Some(body)) = (&probe, content) {
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
                assert!(
                    matches!(variant, "AGENTS.md" | "agents.md" | "Agents.md"),
                    "unexpected variant {variant}",
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
    fn probe_returns_found_with_lowercase_variant() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agents.md"), "lower").unwrap();
        let folder = dir.path().to_string_lossy().into_owned();

        match probe_agents_md(&folder) {
            AgentsMdProbe::Found { variant, .. } => {
                // On case-insensitive filesystems (Windows / macOS default)
                // the first probe `AGENTS.md` may open the same inode as
                // `agents.md`. On case-sensitive filesystems (Linux) the
                // lowercase probe wins. Both outcomes satisfy the contract.
                assert!(
                    matches!(variant, "AGENTS.md" | "agents.md"),
                    "expected lowercase or case-insensitive match, got {variant}",
                );
            }
            other => panic!("expected Found, got {other:?}"),
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
