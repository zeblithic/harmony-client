//! ZEB-158 slice 1: folder manifest types and build helpers.
//!
//! A folder is a `Bundle` whose child-0 is a Book carrying a JSON manifest
//! with `(cid, name, kind)` tuples for each child. See
//! `docs/specs/2026-04-24-folder-primitive-design.md` for the full design.

use serde::{Deserialize, Serialize};

use crate::content_index::ContentKind;

/// Only manifest version this build can read/write. Parse sites MUST reject
/// other versions via `parse_manifest` rather than hand-deserializing, so a
/// future v2 manifest can't be silently rewritten with v1 semantics.
pub const MANIFEST_VERSION: u32 = 1;

/// Outer wrapper so the `folder_manifest` key acts as a self-identifier:
/// a reader with only the bundle bytes can disambiguate a folder from any
/// other kind of bundle by attempting to decode child-0's payload as this
/// shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderManifest {
    pub folder_manifest: ManifestBody,
}

/// The `entries` slice is in bundle order: `entries[i].cid` must equal
/// the bundle's `child[i+1]` for all `i`. `build_folder` guarantees this
/// by construction; callers parsing a manifest from raw bytes must validate
/// the invariant before trusting the entry list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestBody {
    pub version: u32,
    pub entries: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    #[serde(with = "crate::content_index::hex_cid")]
    pub cid: [u8; 32],
    pub name: String,
    pub kind: ContentKind,
}

use harmony_content::bundle::BundleBuilder;
use harmony_content::cid::{ContentFlags, ContentId};

/// A built folder, ready to ingest. The caller must ingest both the
/// manifest book bytes (at `manifest_cid`) and the bundle bytes (at
/// `bundle_cid`) through the event loop's ingest channel before the
/// folder is usable.
#[derive(Debug, Clone)]
pub struct BuiltFolder {
    pub manifest_bytes: Vec<u8>,
    pub manifest_cid: ContentId,
    pub bundle_bytes: Vec<u8>,
    pub bundle_cid: ContentId,
}

/// Build a folder bundle from an ordered list of children.
///
/// The `_folder_name` parameter is accepted for symmetry with call sites
/// that also pass it into the sidecar; names are NOT part of the manifest's
/// own identity (renaming a folder changes its parent's manifest, not its
/// own).
///
/// Returns the manifest book bytes + CID and the bundle bytes + CID; the
/// caller is responsible for ingesting both.
///
/// Empty folders are representable (`children: []`) — the returned bundle
/// has exactly one child (the manifest), which satisfies BundleBuilder's
/// ≥1-child requirement.
pub fn build_folder(
    _folder_name: &str,
    children: &[ManifestEntry],
) -> Result<BuiltFolder, String> {
    let manifest = FolderManifest {
        folder_manifest: ManifestBody {
            version: MANIFEST_VERSION,
            entries: children.to_vec(),
        },
    };
    let manifest_bytes =
        serde_json::to_vec(&manifest).map_err(|e| format!("manifest serialize: {e}"))?;
    let manifest_cid =
        ContentId::for_book(&manifest_bytes, ContentFlags::default())
            .map_err(|e| format!("manifest CID: {e:?}"))?;

    let mut builder = BundleBuilder::new();
    builder.add(manifest_cid);
    for entry in children {
        builder.add(ContentId::from_bytes(entry.cid));
    }
    let (bundle_bytes, bundle_cid) = builder
        .build_with_flags(ContentFlags::default())
        .map_err(|e| format!("folder bundle build: {e:?}"))?;

    Ok(BuiltFolder {
        manifest_bytes,
        manifest_cid,
        bundle_bytes,
        bundle_cid,
    })
}

/// Validate that a parsed manifest is consistent with the bundle's child
/// layout. `bundle_children[0]` is the manifest book itself;
/// `bundle_children[1..]` must match `manifest.folder_manifest.entries[i].cid`
/// position-for-position. Any parse site that intends to READ the entry list
/// for surfacing to users, or REWRITE it for an ancestor cascade, must run
/// this check first — otherwise a malformed ancestor could let an edit
/// silently drop or invent children.
pub fn validate_manifest_matches_bundle(
    manifest: &FolderManifest,
    bundle_children: &[[u8; 32]],
) -> Result<(), String> {
    if bundle_children.is_empty() {
        return Err("folder bundle has no children (manifest missing)".to_string());
    }
    let children_after_manifest = &bundle_children[1..];
    if manifest.folder_manifest.entries.len() != children_after_manifest.len() {
        return Err(format!(
            "manifest/bundle mismatch: manifest has {} entries, bundle has {} children after manifest",
            manifest.folder_manifest.entries.len(),
            children_after_manifest.len()
        ));
    }
    for (i, entry) in manifest.folder_manifest.entries.iter().enumerate() {
        if entry.cid != children_after_manifest[i] {
            return Err(format!("manifest/bundle cid mismatch at index {i}"));
        }
    }
    Ok(())
}

/// Deserialize a folder manifest from bytes and reject unsupported versions.
/// This is the ONLY approved entry point into `FolderManifest` from raw
/// wire/cache bytes — direct `serde_json::from_slice::<FolderManifest>(...)`
/// bypasses the version gate and risks silently rewriting a future v2
/// manifest with v1 semantics.
pub fn parse_manifest(bytes: &[u8]) -> Result<FolderManifest, String> {
    let manifest: FolderManifest =
        serde_json::from_slice(bytes).map_err(|e| format!("manifest parse: {e}"))?;
    if manifest.folder_manifest.version != MANIFEST_VERSION {
        return Err(format!(
            "unsupported folder_manifest version: {} (this build supports {})",
            manifest.folder_manifest.version, MANIFEST_VERSION
        ));
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_manifest_round_trip() {
        let m = FolderManifest {
            folder_manifest: ManifestBody {
                version: 1,
                entries: vec![],
            },
        };
        let bytes = serde_json::to_vec(&m).expect("serialize");
        let parsed: FolderManifest =
            serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(parsed, m);
        assert!(parsed.folder_manifest.entries.is_empty());
    }

    #[test]
    fn manifest_with_mixed_entries_round_trip() {
        let m = FolderManifest {
            folder_manifest: ManifestBody {
                version: 1,
                entries: vec![
                    ManifestEntry {
                        cid: [0xAA; 32],
                        name: "foo.txt".into(),
                        kind: ContentKind::Leaf,
                    },
                    ManifestEntry {
                        cid: [0xBB; 32],
                        name: "photos".into(),
                        kind: ContentKind::Folder,
                    },
                    ManifestEntry {
                        cid: [0xCC; 32],
                        name: "bar.png".into(),
                        kind: ContentKind::Leaf,
                    },
                ],
            },
        };
        let bytes = serde_json::to_vec(&m).expect("serialize");
        let parsed: FolderManifest =
            serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(parsed, m, "order and fields must survive round-trip");

        // Spot-check the wire format contains hex-encoded CIDs and lowercase kinds.
        let json = String::from_utf8(bytes).expect("utf-8");
        assert!(json.contains("\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\""));
        assert!(json.contains("\"kind\":\"folder\""));
        assert!(json.contains("\"kind\":\"leaf\""));
    }

    #[test]
    fn build_empty_folder() {
        let built = build_folder("", &[]).expect("build succeeds");
        // BundleBuilder's wire format is raw concatenated CIDs with no framing
        // header — see harmony-content/src/bundle.rs build_with_flags. An empty
        // folder's bundle therefore equals its single child (the manifest CID)
        // verbatim.
        assert_eq!(built.bundle_bytes.len(), 32);
        assert_eq!(&built.bundle_bytes[..], &built.manifest_cid.to_bytes()[..]);
        // Manifest must itself be a parseable empty folder manifest.
        let parsed: FolderManifest = serde_json::from_slice(&built.manifest_bytes)
            .expect("manifest is valid JSON");
        assert_eq!(parsed.folder_manifest.version, 1);
        assert!(parsed.folder_manifest.entries.is_empty());
    }

    #[test]
    fn build_folder_with_two_children() {
        let children = vec![
            ManifestEntry {
                cid: [0x11; 32],
                name: "a.txt".into(),
                kind: ContentKind::Leaf,
            },
            ManifestEntry {
                cid: [0x22; 32],
                name: "b".into(),
                kind: ContentKind::Folder,
            },
        ];
        let built = build_folder("parent", &children).expect("build");

        // Bundle wire format: raw concatenated 32-byte CIDs, no header — see
        // harmony-content/src/bundle.rs. Layout is [manifest_cid, child_0, child_1].
        assert_eq!(built.bundle_bytes.len(), 96);
        assert_eq!(&built.bundle_bytes[0..32], &built.manifest_cid.to_bytes()[..]);
        assert_eq!(&built.bundle_bytes[32..64], &[0x11u8; 32]);
        assert_eq!(&built.bundle_bytes[64..96], &[0x22u8; 32]);

        // Manifest enumerates children in the same order.
        let parsed: FolderManifest =
            serde_json::from_slice(&built.manifest_bytes).expect("parse");
        assert_eq!(parsed.folder_manifest.entries.len(), 2);
        assert_eq!(parsed.folder_manifest.entries[0].cid, [0x11; 32]);
        assert_eq!(parsed.folder_manifest.entries[1].kind, ContentKind::Folder);
    }

    #[test]
    fn parse_manifest_accepts_current_version() {
        let built = build_folder("", &[]).expect("build");
        let parsed = parse_manifest(&built.manifest_bytes).expect("v1 parses");
        assert_eq!(parsed.folder_manifest.version, MANIFEST_VERSION);
    }

    #[test]
    fn parse_manifest_rejects_future_version() {
        // Hand-craft a v2 manifest by serializing a FolderManifest with version=2.
        let future = FolderManifest {
            folder_manifest: ManifestBody {
                version: 2,
                entries: vec![],
            },
        };
        let bytes = serde_json::to_vec(&future).expect("serialize");
        let err = parse_manifest(&bytes).expect_err("v2 must be rejected");
        assert!(
            err.contains("unsupported folder_manifest version: 2"),
            "expected version-reject message, got: {err}"
        );
    }

    #[test]
    fn parse_manifest_rejects_malformed_bytes() {
        let err = parse_manifest(b"not json").expect_err("garbage must be rejected");
        assert!(err.starts_with("manifest parse:"), "got: {err}");
    }

    #[test]
    fn validate_manifest_matches_bundle_accepts_consistent() {
        let entries = vec![
            ManifestEntry {
                cid: [0x11; 32],
                name: "a".into(),
                kind: ContentKind::Leaf,
            },
            ManifestEntry {
                cid: [0x22; 32],
                name: "b".into(),
                kind: ContentKind::Folder,
            },
        ];
        let manifest = FolderManifest {
            folder_manifest: ManifestBody {
                version: 1,
                entries: entries.clone(),
            },
        };
        // Bundle layout: [manifest_placeholder, child_1, child_2]
        let bundle_children = vec![[0xFF; 32], [0x11; 32], [0x22; 32]];
        validate_manifest_matches_bundle(&manifest, &bundle_children).expect("consistent");
    }

    #[test]
    fn validate_manifest_matches_bundle_rejects_length_mismatch() {
        let manifest = FolderManifest {
            folder_manifest: ManifestBody {
                version: 1,
                entries: vec![ManifestEntry {
                    cid: [0x11; 32],
                    name: "a".into(),
                    kind: ContentKind::Leaf,
                }],
            },
        };
        // Manifest claims 1 entry, bundle has 2 children after the manifest slot.
        let bundle_children = vec![[0xFF; 32], [0x11; 32], [0x22; 32]];
        let err = validate_manifest_matches_bundle(&manifest, &bundle_children)
            .expect_err("length mismatch must be rejected");
        assert!(err.contains("manifest has 1 entries, bundle has 2"), "got: {err}");
    }

    #[test]
    fn validate_manifest_matches_bundle_rejects_cid_mismatch() {
        let manifest = FolderManifest {
            folder_manifest: ManifestBody {
                version: 1,
                entries: vec![ManifestEntry {
                    cid: [0x11; 32],
                    name: "a".into(),
                    kind: ContentKind::Leaf,
                }],
            },
        };
        // Bundle's child after manifest is [0x22], but manifest says [0x11].
        let bundle_children = vec![[0xFF; 32], [0x22; 32]];
        let err = validate_manifest_matches_bundle(&manifest, &bundle_children)
            .expect_err("cid mismatch must be rejected");
        assert!(err.contains("cid mismatch at index 0"), "got: {err}");
    }

    #[test]
    fn validate_manifest_matches_bundle_rejects_empty_bundle() {
        let manifest = FolderManifest {
            folder_manifest: ManifestBody {
                version: 1,
                entries: vec![],
            },
        };
        let err = validate_manifest_matches_bundle(&manifest, &[])
            .expect_err("empty bundle must be rejected");
        assert!(err.contains("no children"), "got: {err}");
    }
}
