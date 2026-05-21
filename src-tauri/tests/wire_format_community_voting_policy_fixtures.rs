//! ZEB-298+ZEB-312 PR 2 Task 4: pin the wire-format encoding of
//! `CommunityVotingPolicy` so existing communities upgrade-in-place
//! when a new field is added later. Tests assert both directions:
//! encoded bytes match, AND absent fields decode to default.

use harmony_app::community_voting_conviction::CommunityVotingPolicy;

#[test]
fn default_policy_encodes_to_empty_cbor_map() {
    // All fields use #[serde(default)]; ciborium emits an empty map
    // when nothing differs from default. The default representation
    // is what existing communities effectively store before adopting
    // the policy struct.
    let p = CommunityVotingPolicy::default();
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&p, &mut buf).expect("encode default");
    assert_eq!(
        buf,
        vec![0xA0],
        "default policy must encode as empty CBOR map"
    );
}

#[test]
fn notify_on_delegate_signal_true_encodes_with_nd_key() {
    let p = CommunityVotingPolicy {
        notify_on_delegate_signal: true,
    };
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&p, &mut buf).expect("encode true");
    // CBOR map(1) with key text(2)="nd" and value bool(true):
    //   0xA1 = map of 1 entry
    //   0x62 = text string of 2 bytes
    //   'n' 'd' = the key
    //   0xF5 = bool(true)
    assert_eq!(buf, vec![0xA1, 0x62, b'n', b'd', 0xF5]);
}

#[test]
fn absent_field_decodes_as_default_false() {
    // Empty CBOR map (what a community without a stored policy looks
    // like once we start storing them).
    let bytes = [0xA0u8];
    let p: CommunityVotingPolicy = ciborium::de::from_reader(&bytes[..]).expect("decode empty map");
    assert!(!p.notify_on_delegate_signal);
}

#[test]
fn round_trip_preserves_field() {
    let p = CommunityVotingPolicy {
        notify_on_delegate_signal: true,
    };
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&p, &mut buf).expect("encode");
    let decoded: CommunityVotingPolicy = ciborium::de::from_reader(&buf[..]).expect("decode");
    assert_eq!(decoded, p);
}
