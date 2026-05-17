//! ZEB-290 Phase 1: Tier 1 Approval voting mechanism.
//!
//! Implements the Approval ballot (voter approves a subset of options),
//! validation rules per spec §4, and (in later tasks) the deterministic
//! tally per spec §4 tally algorithm.

use serde::{Deserialize, Serialize};

use crate::community_membership::ChannelId;
use crate::community_voting_core::{Eligibility, PollId};

/// Maximum number of options per Tier 1 poll. Spec §4.
pub const MAX_OPTIONS: usize = 20;
/// Maximum option label length in chars. Spec §4.
pub const MAX_OPTION_LABEL_LEN: usize = 80;
/// Minimum window in seconds. Spec §4.
pub const MIN_WINDOW_SECS: u32 = 60;
/// Maximum window in seconds (30 days). Spec §4.
pub const MAX_WINDOW_SECS: u32 = 2_592_000;

/// Tier 1 PollCreate payload, encoded as the envelope's `pd` field.
/// Spec §4 PollConfig payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier1PollConfig {
    /// Option labels (2-20, each ≤ 80 chars).
    #[serde(rename = "o")]
    pub options: Vec<String>,
    /// Window in seconds (60-2_592_000).
    #[serde(rename = "w")]
    pub window_seconds: u32,
    /// Optional minimum quorum (number of ballots required for valid result).
    #[serde(rename = "q", skip_serializing_if = "Option::is_none", default)]
    pub quorum: Option<u32>,
    /// Optional supermajority threshold percent (0-100).
    #[serde(rename = "th", skip_serializing_if = "Option::is_none", default)]
    pub threshold_percent: Option<u8>,
    /// Optional multi-winner top-N (default 1).
    #[serde(rename = "mw", skip_serializing_if = "Option::is_none", default)]
    pub multi_winner: Option<u8>,
    /// Eligibility predicate. Embedded so verify-on-receive doesn't
    /// need a separate event type.
    #[serde(rename = "el")]
    pub eligibility: Eligibility,
    /// Channel where the poll-message card appears. Tier 1 specific.
    /// Uses the project's `ChannelId` bstr wire form for consistency
    /// with other CBOR payloads (no `serde_bytes` dep needed).
    #[serde(rename = "ci")]
    pub channel_id: ChannelId,
}

/// Tier 1 BallotCast payload. Spec §4 Ballot payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier1Ballot {
    /// PollId reference (envelope `pd` carries this even on ballots
    /// to identify which poll the ballot is for).
    #[serde(rename = "pi")]
    pub poll_id: PollId,
    /// Approved option indices, deduped and sorted ascending.
    #[serde(rename = "ap")]
    pub approved_indices: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationError {
    TooFewOptions,
    TooManyOptions,
    EmptyOptionLabel(usize),
    OptionLabelTooLong(usize),
    WindowTooShort,
    WindowTooLong,
    ThresholdOutOfRange,
    MultiWinnerZero,
    MultiWinnerExceedsOptions,
    EmptyBallot,
    AbstentionBallot,
    IndexOutOfRange,
    IndicesNotSortedDeduped,
}

/// Validate a PollConfig at create-time (before signing) or at receive
/// time (after deserialize). Spec §4 PollConfig constraints.
pub fn validate_poll_config(cfg: &Tier1PollConfig) -> Result<(), ValidationError> {
    if cfg.options.len() < 2 {
        return Err(ValidationError::TooFewOptions);
    }
    if cfg.options.len() > MAX_OPTIONS {
        return Err(ValidationError::TooManyOptions);
    }
    for (i, label) in cfg.options.iter().enumerate() {
        if label.is_empty() {
            return Err(ValidationError::EmptyOptionLabel(i));
        }
        if label.chars().count() > MAX_OPTION_LABEL_LEN {
            return Err(ValidationError::OptionLabelTooLong(i));
        }
    }
    if cfg.window_seconds < MIN_WINDOW_SECS {
        return Err(ValidationError::WindowTooShort);
    }
    if cfg.window_seconds > MAX_WINDOW_SECS {
        return Err(ValidationError::WindowTooLong);
    }
    if let Some(th) = cfg.threshold_percent {
        if th > 100 {
            return Err(ValidationError::ThresholdOutOfRange);
        }
    }
    if let Some(mw) = cfg.multi_winner {
        if mw == 0 {
            return Err(ValidationError::MultiWinnerZero);
        }
        if mw as usize > cfg.options.len() {
            return Err(ValidationError::MultiWinnerExceedsOptions);
        }
    }
    Ok(())
}

/// Validate a Ballot against its poll's config. Spec §4 ballot constraints.
///
/// Check ordering matters: structural checks (empty, range, sort/dedup)
/// run before the abstention check so that `[0, 0, 2]` against a 3-option
/// poll surfaces as `IndicesNotSortedDeduped` rather than as a spurious
/// `AbstentionBallot` (raw len happens to equal options.len()).
pub fn validate_ballot(ballot: &Tier1Ballot, cfg: &Tier1PollConfig) -> Result<(), ValidationError> {
    if ballot.approved_indices.is_empty() {
        return Err(ValidationError::EmptyBallot);
    }
    for &i in &ballot.approved_indices {
        if (i as usize) >= cfg.options.len() {
            return Err(ValidationError::IndexOutOfRange);
        }
    }
    for w in ballot.approved_indices.windows(2) {
        if w[0] >= w[1] {
            return Err(ValidationError::IndicesNotSortedDeduped);
        }
    }
    if ballot.approved_indices.len() == cfg.options.len() {
        return Err(ValidationError::AbstentionBallot);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_config() -> Tier1PollConfig {
        Tier1PollConfig {
            options: vec!["Pizza".into(), "Burgers".into(), "Sushi".into()],
            window_seconds: 3600,
            quorum: None,
            threshold_percent: None,
            multi_winner: None,
            eligibility: Eligibility {
                min_power: 0,
                min_vouching_depth: None,
                sortition_size: None,
            },
            channel_id: ChannelId([0x11; 16]),
        }
    }

    #[test]
    fn good_config_validates() {
        assert_eq!(validate_poll_config(&good_config()), Ok(()));
    }

    #[test]
    fn too_few_options_rejected() {
        let mut c = good_config();
        c.options = vec!["only one".into()];
        assert_eq!(
            validate_poll_config(&c),
            Err(ValidationError::TooFewOptions)
        );
    }

    #[test]
    fn too_many_options_rejected() {
        let mut c = good_config();
        c.options = (0..21).map(|i| format!("opt{i}")).collect();
        assert_eq!(
            validate_poll_config(&c),
            Err(ValidationError::TooManyOptions)
        );
    }

    #[test]
    fn label_too_long_rejected() {
        let mut c = good_config();
        c.options[1] = "x".repeat(81);
        assert_eq!(
            validate_poll_config(&c),
            Err(ValidationError::OptionLabelTooLong(1))
        );
    }

    #[test]
    fn empty_label_rejected() {
        let mut c = good_config();
        c.options[0] = "".into();
        assert_eq!(
            validate_poll_config(&c),
            Err(ValidationError::EmptyOptionLabel(0))
        );
    }

    #[test]
    fn window_too_short_rejected() {
        let mut c = good_config();
        c.window_seconds = 30;
        assert_eq!(
            validate_poll_config(&c),
            Err(ValidationError::WindowTooShort)
        );
    }

    #[test]
    fn window_too_long_rejected() {
        let mut c = good_config();
        c.window_seconds = 2_592_001;
        assert_eq!(
            validate_poll_config(&c),
            Err(ValidationError::WindowTooLong)
        );
    }

    #[test]
    fn threshold_over_100_rejected() {
        let mut c = good_config();
        c.threshold_percent = Some(101);
        assert_eq!(
            validate_poll_config(&c),
            Err(ValidationError::ThresholdOutOfRange)
        );
    }

    #[test]
    fn multi_winner_zero_rejected() {
        let mut c = good_config();
        c.multi_winner = Some(0);
        assert_eq!(
            validate_poll_config(&c),
            Err(ValidationError::MultiWinnerZero)
        );
    }

    #[test]
    fn multi_winner_exceeds_options_rejected() {
        let mut c = good_config();
        c.multi_winner = Some(5);
        assert_eq!(
            validate_poll_config(&c),
            Err(ValidationError::MultiWinnerExceedsOptions)
        );
    }

    #[test]
    fn good_ballot_validates() {
        let cfg = good_config();
        let b = Tier1Ballot {
            poll_id: PollId([0xaa; 32]),
            approved_indices: vec![0, 2],
        };
        assert_eq!(validate_ballot(&b, &cfg), Ok(()));
    }

    #[test]
    fn empty_ballot_rejected() {
        let cfg = good_config();
        let b = Tier1Ballot {
            poll_id: PollId([0xaa; 32]),
            approved_indices: vec![],
        };
        assert_eq!(validate_ballot(&b, &cfg), Err(ValidationError::EmptyBallot));
    }

    #[test]
    fn approve_all_rejected_as_abstention() {
        let cfg = good_config();
        let b = Tier1Ballot {
            poll_id: PollId([0xaa; 32]),
            approved_indices: vec![0, 1, 2],
        };
        assert_eq!(
            validate_ballot(&b, &cfg),
            Err(ValidationError::AbstentionBallot)
        );
    }

    #[test]
    fn out_of_range_index_rejected() {
        let cfg = good_config();
        let b = Tier1Ballot {
            poll_id: PollId([0xaa; 32]),
            approved_indices: vec![0, 5],
        };
        assert_eq!(
            validate_ballot(&b, &cfg),
            Err(ValidationError::IndexOutOfRange)
        );
    }

    #[test]
    fn unsorted_indices_rejected() {
        let cfg = good_config();
        let b = Tier1Ballot {
            poll_id: PollId([0xaa; 32]),
            approved_indices: vec![2, 0],
        };
        assert_eq!(
            validate_ballot(&b, &cfg),
            Err(ValidationError::IndicesNotSortedDeduped)
        );
    }

    #[test]
    fn duplicate_indices_rejected() {
        let cfg = good_config();
        let b = Tier1Ballot {
            poll_id: PollId([0xaa; 32]),
            approved_indices: vec![0, 0, 2],
        };
        assert_eq!(
            validate_ballot(&b, &cfg),
            Err(ValidationError::IndicesNotSortedDeduped)
        );
    }

    #[test]
    fn config_round_trips_via_cbor() {
        let cfg = good_config();
        let mut encoded = Vec::new();
        ciborium::into_writer(&cfg, &mut encoded).expect("encode");
        let decoded: Tier1PollConfig = ciborium::from_reader(&encoded[..]).expect("decode");
        assert_eq!(cfg, decoded);
    }

    #[test]
    fn ballot_round_trips_via_cbor() {
        let b = Tier1Ballot {
            poll_id: PollId([0xaa; 32]),
            approved_indices: vec![1, 3, 7],
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&b, &mut encoded).expect("encode");
        let decoded: Tier1Ballot = ciborium::from_reader(&encoded[..]).expect("decode");
        assert_eq!(b, decoded);
    }
}
