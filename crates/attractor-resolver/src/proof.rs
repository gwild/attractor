//! Proof-fidelity comparison for typed claim and replay receipts.
//!
//! `attractor-proto` owns the portable wire shape. This module owns the L2
//! derivation surface: callers provide the typed claim expectation and the
//! observed receipt, and the comparator returns the only [`ReceiptComparison`]
//! value that may be used for proof adjudication.

use attractor_proto::{
    ClaimHash, FieldRef, ReceiptComparison, ReceiptInt, ReceiptStringList, TypedReceipt,
};

/// Typed claim expectation used by the L2 receipt comparator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptComparisonClaim {
    /// Claim hash this comparison adjudicates.
    pub claim_hash: ClaimHash,
    /// Typed values claimed by the proof body or fixture.
    pub expected_receipt: TypedReceipt,
    /// Fields that must be known and equal before the claim can clear.
    pub load_bearing_fields: Vec<FieldRef>,
}

/// Compare the typed claim expectation with a replay receipt.
///
/// Unknown load-bearing values dominate mismatches: an unknown side means the
/// comparator cannot safely clear or falsify that field yet, so the result is
/// [`ReceiptComparison::Incomplete`]. Only when every load-bearing field is
/// known on both sides can the comparator return equal or mismatch.
#[must_use]
pub fn compare(claim: &ReceiptComparisonClaim, receipt: &TypedReceipt) -> ReceiptComparison {
    let mut unknown_fields = Vec::new();
    let mut mismatch_fields = Vec::new();

    for field in &claim.load_bearing_fields {
        compare_field(
            field,
            &claim.expected_receipt,
            receipt,
            &mut unknown_fields,
            &mut mismatch_fields,
        );
    }

    if !unknown_fields.is_empty() {
        return ReceiptComparison::incomplete(unknown_fields);
    }
    if !mismatch_fields.is_empty() {
        return ReceiptComparison::mismatch(mismatch_fields);
    }
    ReceiptComparison::equal()
}

fn compare_field(
    field: &FieldRef,
    expected: &TypedReceipt,
    actual: &TypedReceipt,
    unknown_fields: &mut Vec<FieldRef>,
    mismatch_fields: &mut Vec<FieldRef>,
) {
    match field.path.as_str() {
        "exit" => compare_int_field(
            field,
            &expected.exit,
            &actual.exit,
            unknown_fields,
            mismatch_fields,
        ),
        "passed_count" => compare_int_field(
            field,
            &expected.passed_count,
            &actual.passed_count,
            unknown_fields,
            mismatch_fields,
        ),
        "failed_count" => compare_int_field(
            field,
            &expected.failed_count,
            &actual.failed_count,
            unknown_fields,
            mismatch_fields,
        ),
        "collected_count" => compare_int_field(
            field,
            &expected.collected_count,
            &actual.collected_count,
            unknown_fields,
            mismatch_fields,
        ),
        "suite_ids" => compare_string_list_field(
            field,
            &expected.suite_ids,
            &actual.suite_ids,
            unknown_fields,
            mismatch_fields,
        ),
        _ => push_unique(unknown_fields, field.clone()),
    }
}

fn compare_int_field(
    field: &FieldRef,
    expected: &ReceiptInt,
    actual: &ReceiptInt,
    unknown_fields: &mut Vec<FieldRef>,
    mismatch_fields: &mut Vec<FieldRef>,
) {
    match (expected, actual) {
        (ReceiptInt::Known { value: left }, ReceiptInt::Known { value: right }) => {
            if left != right {
                push_unique(mismatch_fields, field.clone());
            }
        }
        _ => push_int_unknowns(expected, actual, field, unknown_fields),
    }
}

fn push_int_unknowns(
    expected: &ReceiptInt,
    actual: &ReceiptInt,
    fallback: &FieldRef,
    unknown_fields: &mut Vec<FieldRef>,
) {
    let expected_unknown = if let ReceiptInt::Unknown { field_ref, .. } = expected {
        push_unique(unknown_fields, field_ref.clone());
        true
    } else {
        false
    };
    if let ReceiptInt::Unknown { field_ref, .. } = actual {
        push_unique(unknown_fields, field_ref.clone());
    } else if !expected_unknown {
        push_unique(unknown_fields, fallback.clone());
    }
}

fn compare_string_list_field(
    field: &FieldRef,
    expected: &ReceiptStringList,
    actual: &ReceiptStringList,
    unknown_fields: &mut Vec<FieldRef>,
    mismatch_fields: &mut Vec<FieldRef>,
) {
    match (expected, actual) {
        (ReceiptStringList::Known { value: left }, ReceiptStringList::Known { value: right }) => {
            if left != right {
                push_unique(mismatch_fields, field.clone());
            }
        }
        _ => push_string_list_unknowns(expected, actual, field, unknown_fields),
    }
}

fn push_string_list_unknowns(
    expected: &ReceiptStringList,
    actual: &ReceiptStringList,
    fallback: &FieldRef,
    unknown_fields: &mut Vec<FieldRef>,
) {
    let expected_unknown = if let ReceiptStringList::Unknown { field_ref, .. } = expected {
        push_unique(unknown_fields, field_ref.clone());
        true
    } else {
        false
    };
    if let ReceiptStringList::Unknown { field_ref, .. } = actual {
        push_unique(unknown_fields, field_ref.clone());
    } else if !expected_unknown {
        push_unique(unknown_fields, fallback.clone());
    }
}

fn push_unique(fields: &mut Vec<FieldRef>, field: FieldRef) {
    if fields.iter().all(|existing| existing.path != field.path) {
        fields.push(field);
    }
}

#[cfg(test)]
mod tests {
    use super::{compare, ReceiptComparisonClaim};
    use attractor_proto::{
        ClaimFixtureHash, ClaimHash, ContentHashHex, FieldRef, FixtureManifestKind,
        ReceiptComparison, ReceiptFixtureHash, ReceiptHash, ReceiptInt, ReceiptStringList,
        TypedReceipt,
    };
    use serde::Deserialize;

    const CORPUS: &str = include_str!("../../../fixtures/receipt-comparison/corpus.v0.json");

    fn hash(seed: u8) -> ContentHashHex {
        ContentHashHex::new(format!("{seed:064x}")).unwrap()
    }

    fn field(path: &str) -> FieldRef {
        FieldRef {
            path: path.to_owned(),
        }
    }

    fn claim_fixture() -> ClaimFixtureHash {
        ClaimFixtureHash {
            manifest_kind: FixtureManifestKind::Claim,
            manifest_ref: "fixtures/receipt-comparison/claim.v0.json".to_owned(),
            digest: hash(1),
        }
    }

    fn receipt_fixture() -> ReceiptFixtureHash {
        ReceiptFixtureHash {
            manifest_kind: FixtureManifestKind::Receipt,
            manifest_ref: "fixtures/receipt-comparison/receipt.v0.json".to_owned(),
            digest: hash(2),
        }
    }

    fn claim_hash() -> ClaimHash {
        ClaimHash {
            fixture_hash: claim_fixture(),
            digest: hash(3),
        }
    }

    fn receipt_hash(seed: u8) -> ReceiptHash {
        ReceiptHash {
            fixture_hash: receipt_fixture(),
            digest: hash(seed),
        }
    }

    fn known(value: u64) -> ReceiptInt {
        ReceiptInt::Known { value }
    }

    fn unknown(reason: &str) -> ReceiptInt {
        ReceiptInt::Unknown {
            reason: reason.to_owned(),
            field_ref: field("passed_count"),
        }
    }

    fn typed_receipt(passed_count: ReceiptInt, hash_seed: u8) -> TypedReceipt {
        TypedReceipt {
            receipt_hash: receipt_hash(hash_seed),
            reexec_request_id: "reexec:3031:linux".to_owned(),
            exit: known(0),
            passed_count,
            failed_count: known(0),
            collected_count: known(11),
            suite_ids: ReceiptStringList::Known {
                value: vec!["start_service".to_owned()],
            },
            receipt_fixture_hash: receipt_fixture(),
        }
    }

    fn claim_with_passed_count(passed_count: ReceiptInt) -> ReceiptComparisonClaim {
        ReceiptComparisonClaim {
            claim_hash: claim_hash(),
            expected_receipt: typed_receipt(passed_count, 4),
            load_bearing_fields: vec![field("passed_count")],
        }
    }

    #[test]
    fn known_mismatch_is_derived_from_compare_return() {
        let claim = claim_with_passed_count(known(11));
        let receipt = typed_receipt(known(9), 5);
        let comparison = compare(&claim, &receipt);

        match comparison {
            ReceiptComparison::Mismatch {
                mismatch_fields, ..
            } => {
                assert_eq!(mismatch_fields, vec![field("passed_count")]);
            }
            ReceiptComparison::Equal
            | ReceiptComparison::Incomplete { .. }
            | ReceiptComparison::NeedsReexec { .. } => {
                panic!("known 9-vs-11 must derive mismatch, got {comparison:?}");
            }
            _ => panic!("known 9-vs-11 returned unknown comparison variant"),
        }
    }

    #[test]
    fn unknown_receipt_side_is_incomplete_not_equal_or_mismatch() {
        let claim = claim_with_passed_count(known(11));
        let receipt = typed_receipt(unknown("missing_passed_count"), 5);
        let comparison = compare(&claim, &receipt);

        match comparison {
            ReceiptComparison::Incomplete { unknown_fields, .. } => {
                assert_eq!(unknown_fields, vec![field("passed_count")]);
            }
            ReceiptComparison::Equal
            | ReceiptComparison::Mismatch { .. }
            | ReceiptComparison::NeedsReexec { .. } => {
                panic!("unknown receipt side must not clear or mismatch, got {comparison:?}");
            }
            _ => panic!("unknown receipt side returned unknown comparison variant"),
        }
    }

    #[test]
    fn unknown_on_both_sides_is_incomplete_not_equal() {
        let claim = claim_with_passed_count(unknown("claim_missing_passed_count"));
        let receipt = typed_receipt(unknown("receipt_missing_passed_count"), 5);
        let comparison = compare(&claim, &receipt);

        match comparison {
            ReceiptComparison::Incomplete { unknown_fields, .. } => {
                assert_eq!(unknown_fields, vec![field("passed_count")]);
            }
            ReceiptComparison::Equal
            | ReceiptComparison::Mismatch { .. }
            | ReceiptComparison::NeedsReexec { .. } => {
                panic!("unknown/unknown must not clear, got {comparison:?}");
            }
            _ => panic!("unknown/unknown returned unknown comparison variant"),
        }
    }

    #[test]
    fn known_equal_load_bearing_fields_are_equal() {
        let claim = claim_with_passed_count(known(11));
        let receipt = typed_receipt(known(11), 5);
        let comparison = compare(&claim, &receipt);

        match comparison {
            ReceiptComparison::Equal => {}
            ReceiptComparison::Mismatch { .. }
            | ReceiptComparison::Incomplete { .. }
            | ReceiptComparison::NeedsReexec { .. } => {
                panic!("known equal fields must derive equal, got {comparison:?}");
            }
            _ => panic!("known equal fields returned unknown comparison variant"),
        }
    }

    #[derive(Debug, Deserialize)]
    struct Corpus {
        schema_id: String,
        rows: Vec<CorpusRow>,
    }

    #[derive(Debug, Deserialize)]
    struct CorpusRow {
        id: String,
        claim_passed_count: CorpusInt,
        receipt_passed_count: CorpusInt,
        expect: Option<ReceiptComparison>,
        forbid: Option<ForbidAssertion>,
    }

    #[derive(Clone, Debug, Deserialize)]
    #[serde(tag = "state", rename_all = "snake_case")]
    enum CorpusInt {
        Known { value: u64 },
        Unknown { reason: String, field_ref: FieldRef },
    }

    #[derive(Debug, Deserialize)]
    struct ForbidAssertion {
        allowed_kinds: Vec<ComparisonKind>,
    }

    #[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    enum ComparisonKind {
        Equal,
        Mismatch,
        Incomplete,
        NeedsReexec,
    }

    impl From<CorpusInt> for ReceiptInt {
        fn from(value: CorpusInt) -> Self {
            match value {
                CorpusInt::Known { value } => Self::Known { value },
                CorpusInt::Unknown { reason, field_ref } => Self::Unknown { reason, field_ref },
            }
        }
    }

    fn comparison_kind(comparison: &ReceiptComparison) -> Option<ComparisonKind> {
        match comparison {
            ReceiptComparison::Equal => Some(ComparisonKind::Equal),
            ReceiptComparison::Mismatch { .. } => Some(ComparisonKind::Mismatch),
            ReceiptComparison::Incomplete { .. } => Some(ComparisonKind::Incomplete),
            ReceiptComparison::NeedsReexec { .. } => Some(ComparisonKind::NeedsReexec),
            _ => None,
        }
    }

    fn unknown_side_count(row: &CorpusRow) -> usize {
        usize::from(matches!(row.claim_passed_count, CorpusInt::Unknown { .. }))
            + usize::from(matches!(
                row.receipt_passed_count,
                CorpusInt::Unknown { .. }
            ))
    }

    #[test]
    fn shared_corpus_constrains_derivation_and_unknown_presence_floor() {
        let corpus: Corpus = serde_json::from_str(CORPUS).unwrap();
        assert_eq!(corpus.schema_id, "attractor.receipt_comparison.corpus.v0");

        let mut has_one_unknown_forbid = false;
        let mut has_both_unknown_forbid = false;

        for row in corpus.rows {
            assert!(
                row.expect.is_some() ^ row.forbid.is_some(),
                "row {} must declare exactly one of expect or forbid",
                row.id
            );

            let unknown_sides = unknown_side_count(&row);
            if row.forbid.is_some() && unknown_sides == 1 {
                has_one_unknown_forbid = true;
            }
            if row.forbid.is_some() && unknown_sides == 2 {
                has_both_unknown_forbid = true;
            }

            let claim = claim_with_passed_count(row.claim_passed_count.into());
            let receipt = typed_receipt(row.receipt_passed_count.into(), 5);
            let comparison = compare(&claim, &receipt);

            if let Some(expected) = row.expect {
                assert_eq!(
                    comparison, expected,
                    "row {} expected exact comparison",
                    row.id
                );
            }
            if let Some(forbid) = row.forbid {
                let kind = comparison_kind(&comparison).unwrap_or_else(|| {
                    panic!(
                        "row {} produced an unclassified comparison variant: {comparison:?}",
                        row.id
                    )
                });
                assert!(
                    forbid.allowed_kinds.contains(&kind),
                    "row {} produced disallowed comparison kind {kind:?}: {comparison:?}",
                    row.id
                );
            }
        }

        assert!(
            has_one_unknown_forbid,
            "corpus must include a forbid row with exactly one Unknown side"
        );
        assert!(
            has_both_unknown_forbid,
            "corpus must include a forbid row with both sides Unknown"
        );
    }
}
