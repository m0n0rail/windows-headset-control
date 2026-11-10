use crate::model::CollectionInfo;
use crate::profile::profile_for;

/// True when `c` belongs to a device this project has a profile for.
///
/// Candidate selection is scoped to these: shape-based ranking alone will
/// happily pick an unrelated vendor's HID collection, so positive device
/// identification comes first. The list of supported products is
/// [`crate::profile::PROFILES`].
pub fn is_supported_device(c: &CollectionInfo) -> bool {
    profile_for(c).is_some()
}

/// One ranked enumeration entry, with the reasoning that produced its score.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// Index into the sorted collection slice.
    pub index: usize,
    pub score: u32,
    pub reasons: Vec<String>,
    /// `Some` when the collection may never be used as a control channel.
    pub disqualified: Option<String>,
}

/// Deterministic ordering so that a diagnostic index means the same thing
/// across runs on the same machine with the same devices attached.
pub fn stable_sort_collections(all: &mut [CollectionInfo]) {
    all.sort_by(|a, b| {
        a.vendor_id
            .cmp(&b.vendor_id)
            .then(a.product_id.cmp(&b.product_id))
            .then(a.interface_number.cmp(&b.interface_number))
            .then(a.collection_number.cmp(&b.collection_number))
            .then(a.usage_page.cmp(&b.usage_page))
            .then(a.usage.cmp(&b.usage))
            .then_with(|| a.id.raw().cmp(b.id.raw()))
    });
}

/// Ranks collections as control-channel candidates using descriptor evidence only.
///
/// Returns every collection, so callers can show why a collection was rejected.
/// Disqualified entries always sort last.
pub fn rank_candidates(all: &[CollectionInfo]) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = all
        .iter()
        .enumerate()
        .map(|(index, c)| {
            let mut reasons = Vec::new();
            let mut score = 0u32;

            if c.is_audio_stack_collection() {
                return Candidate {
                    index,
                    score: 0,
                    reasons: vec![
                        "telephony headset collection bound by the Windows audio stack".into(),
                    ],
                    disqualified: Some("reserved for Windows audio".into()),
                };
            }

            if !c.is_vendor_defined() {
                return Candidate {
                    index,
                    score: 0,
                    reasons: vec![format!(
                        "usage page {:#06x} is not vendor-defined",
                        c.usage_page
                    )],
                    disqualified: Some("not a vendor-defined usage page".into()),
                };
            }
            score += 100;
            reasons.push(format!("vendor-defined usage page {:#06x}", c.usage_page));

            let width = c.output_report_len.max(c.feature_report_len);
            if width == 0 {
                return Candidate {
                    index,
                    score: 0,
                    reasons: vec!["no output or feature reports declared".into()],
                    disqualified: Some("no writable report path".into()),
                };
            }
            score += u32::from(width);
            reasons.push(format!("declared report width {width} bytes"));

            if c.input_report_len > 0 {
                score += 10;
                reasons.push(format!(
                    "bidirectional: input report width {} bytes",
                    c.input_report_len
                ));
            }

            Candidate {
                index,
                score,
                reasons,
                disqualified: None,
            }
        })
        .collect();

    out.sort_by(|a, b| {
        a.disqualified
            .is_some()
            .cmp(&b.disqualified.is_some())
            .then(b.score.cmp(&a.score))
            .then(a.index.cmp(&b.index))
    });
    out
}

/// True when exactly one qualified candidate has the top score.
///
/// A tie is never broken automatically: two equally plausible vendor channels
/// mean the evidence is insufficient, and guessing is worse than asking.
///
/// Expects `ranked` in the order `rank_candidates` produces: qualified first,
/// then descending score. Only the top two qualified entries are compared.
pub fn has_unambiguous_winner(ranked: &[Candidate]) -> bool {
    let qualified: Vec<&Candidate> = ranked.iter().filter(|c| c.disqualified.is_none()).collect();
    match qualified.as_slice() {
        [] => false,
        [_only] => true,
        [first, second, ..] => first.score > second.score,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FakeHidBackend, HidBackend};

    const FIXTURE: &str = include_str!("../tests/fixtures/blackshark-v3-pro-ps.json");

    fn collections() -> Vec<CollectionInfo> {
        let mut all = FakeHidBackend::from_fixture_str(FIXTURE)
            .unwrap()
            .enumerate()
            .unwrap();
        stable_sort_collections(&mut all);
        all
    }

    #[test]
    fn sorting_is_deterministic() {
        let a: Vec<String> = collections()
            .iter()
            .map(|c| c.id.raw().to_string())
            .collect();
        let b: Vec<String> = collections()
            .iter()
            .map(|c| c.id.raw().to_string())
            .collect();
        assert_eq!(a, b);
    }

    #[test]
    fn best_candidate_is_the_64_byte_vendor_collection() {
        let all = collections();
        let ranked = rank_candidates(&all);
        let best = ranked.first().expect("at least one candidate");
        assert_eq!(all[best.index].usage_page, 0xFF14);
        assert_eq!(all[best.index].output_report_len, 64);
    }

    #[test]
    fn audio_collection_is_disqualified() {
        let all = collections();
        let ranked = rank_candidates(&all);
        let audio_pos = all
            .iter()
            .position(|c| c.is_audio_stack_collection())
            .unwrap();
        let entry = ranked.iter().find(|r| r.index == audio_pos);
        assert!(entry.is_none() || entry.unwrap().disqualified.is_some());
    }

    #[test]
    fn non_vendor_collections_are_disqualified() {
        let all = collections();
        for c in rank_candidates(&all) {
            if c.disqualified.is_none() {
                assert!(all[c.index].is_vendor_defined());
            }
        }
    }

    #[test]
    fn both_vendor_collections_are_reported() {
        let all = collections();
        let qualified: Vec<_> = rank_candidates(&all)
            .into_iter()
            .filter(|c| c.disqualified.is_none())
            .collect();
        assert_eq!(qualified.len(), 2, "0xFF13 and 0xFF14 both qualify");
    }

    #[test]
    fn equal_scores_produce_no_automatic_winner() {
        let mut all = collections();
        // Force a tie: make 0xFF13 the same width as 0xFF14.
        for c in all.iter_mut() {
            if c.usage_page == 0xFF13 {
                c.output_report_len = 64;
                c.input_report_len = 64;
            }
        }
        let ranked = rank_candidates(&all);
        assert!(!has_unambiguous_winner(&ranked));
    }

    #[test]
    fn distinct_scores_produce_a_winner() {
        assert!(has_unambiguous_winner(&rank_candidates(&collections())));
    }

    #[test]
    fn empty_input_yields_no_candidates() {
        assert!(rank_candidates(&[]).is_empty());
        assert!(!has_unambiguous_winner(&[]));
    }

    #[test]
    fn supported_device_is_recognized_by_vendor_and_product() {
        let all = collections();
        for c in &all {
            assert!(is_supported_device(c));
        }
    }

    #[test]
    fn unsupported_vendor_is_not_recognized() {
        let mut c = all_first();
        c.vendor_id = 0x1770;
        assert!(!is_supported_device(&c));
    }

    #[test]
    fn unsupported_product_is_not_recognized() {
        let mut c = all_first();
        c.product_id = 0x0577;
        assert!(!is_supported_device(&c));
    }

    fn all_first() -> CollectionInfo {
        collections()
            .into_iter()
            .next()
            .expect("fixture has collections")
    }
}
