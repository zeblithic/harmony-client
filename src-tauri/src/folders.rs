//! ZEB-158 slice 1: folder manifest types and build helpers.
//!
//! A folder is a `Bundle` whose child-0 is a Book carrying a JSON manifest
//! with `(cid, name, kind)` tuples for each child. See
//! `docs/specs/2026-04-24-folder-primitive-design.md` for the full design.

use serde::{Deserialize, Serialize};

use crate::content_index::ContentKind;

/// Outer wrapper so the `folder_manifest` key acts as a self-identifier:
/// a reader with only the bundle bytes can disambiguate a folder from any
/// other kind of bundle by attempting to decode child-0's payload as this
/// shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderManifest {
    pub folder_manifest: ManifestBody,
}

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
            version: 1,
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
        // Empty folder's bundle bytes are exactly the 32-byte manifest CID.
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

        // Bundle bytes = concat(manifest_cid, child_0_cid, child_1_cid) = 96 bytes.
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
}
