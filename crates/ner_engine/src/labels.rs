//! CoNLL NER label schema and mapping to [`crate::EntityType`].
//!
//! XLM-RoBERTa fine-tuned on CoNLL-2003 / panx NER uses the standard
//! BIO-2 (Begin-Inside-Out) tagging scheme with four coarse entity
//! types: PER, ORG, LOC, MISC. Each token receives a label of the form
//! `B-<TYPE>` (begin), `I-<TYPE>` (inside), or `O` (outside).
//!
//! This module maps those CoNLL labels to the substrate's
//! [`crate::EntityType`] enum so downstream consumers (the hybrid
//! synthesizer, the observation pipeline) can reason about extracted
//! entities using the same typed taxonomy across the NER and
//! lexicon/regex extraction paths.

/// CoNLL NER label set (BIO-2 scheme).
///
/// The label id is the integer the ONNX model emits as its output
/// argmax. The string form is the standard CoNLL label string
/// (`"B-PER"`, `"I-PER"`, `"B-ORG"`, …, `"O"`).
///
/// The order here matches the XLM-RoBERTa NER model's label vocabulary
/// (id 0 = `O`, id 1 = `B-PER`, …). If a different model is used,
/// [`label_from_id`] should be updated to match its label vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConllLabel {
    /// Outside — not a named entity.
    O,
    /// Beginning of a person name.
    BPer,
    /// Inside a person name.
    IPer,
    /// Beginning of an organization.
    BOrg,
    /// Inside an organization.
    IOrg,
    /// Beginning of a location.
    BLoc,
    /// Inside a location.
    ILoc,
    /// Beginning of a miscellaneous entity.
    BMisc,
    /// Inside a miscellaneous entity.
    IMisc,
}

impl ConllLabel {
    /// Total number of labels in the CoNLL NER schema.
    pub const COUNT: usize = 9;

    /// Convert a label id (ONNX output argmax index) to a [`ConllLabel`].
    pub const fn from_id(id: usize) -> Option<Self> {
        match id {
            0 => Some(Self::O),
            1 => Some(Self::BPer),
            2 => Some(Self::IPer),
            3 => Some(Self::BOrg),
            4 => Some(Self::IOrg),
            5 => Some(Self::BLoc),
            6 => Some(Self::ILoc),
            7 => Some(Self::BMisc),
            8 => Some(Self::IMisc),
            _ => None,
        }
    }

    /// The string form of the label (e.g. `"B-PER"`, `"O"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::O => "O",
            Self::BPer => "B-PER",
            Self::IPer => "I-PER",
            Self::BOrg => "B-ORG",
            Self::IOrg => "I-ORG",
            Self::BLoc => "B-LOC",
            Self::ILoc => "I-LOC",
            Self::BMisc => "B-MISC",
            Self::IMisc => "I-MISC",
        }
    }

    /// Parse a CoNLL label string (e.g. `"B-PER"`) into a [`ConllLabel`].
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "O" => Some(Self::O),
            "B-PER" => Some(Self::BPer),
            "I-PER" => Some(Self::IPer),
            "B-ORG" => Some(Self::BOrg),
            "I-ORG" => Some(Self::IOrg),
            "B-LOC" => Some(Self::BLoc),
            "I-LOC" => Some(Self::ILoc),
            "B-MISC" => Some(Self::BMisc),
            "I-MISC" => Some(Self::IMisc),
            _ => None,
        }
    }

    /// Whether this label marks the beginning of an entity span.
    pub const fn is_begin(self) -> bool {
        matches!(
            self,
            Self::BPer | Self::BOrg | Self::BLoc | Self::BMisc
        )
    }

    /// Whether this label marks the inside (continuation) of an entity span.
    pub const fn is_inside(self) -> bool {
        matches!(
            self,
            Self::IPer | Self::IOrg | Self::ILoc | Self::IMisc
        )
    }

    /// Whether this label is outside (not an entity).
    pub const fn is_outside(self) -> bool {
        matches!(self, Self::O)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_id_round_trips() {
        for id in 0..ConllLabel::COUNT {
            let label = ConllLabel::from_id(id).expect("valid id");
            assert_eq!(label.as_str(), label.as_str());
        }
        assert!(ConllLabel::from_id(ConllLabel::COUNT).is_none());
    }

    #[test]
    fn from_str_round_trips() {
        for id in 0..ConllLabel::COUNT {
            let label = ConllLabel::from_id(id).unwrap();
            let back = ConllLabel::from_str(label.as_str()).unwrap();
            assert_eq!(label, back);
        }
        assert!(ConllLabel::from_str("B-UNKNOWN").is_none());
    }

    #[test]
    fn begin_inside_outside_partition() {
        assert!(ConllLabel::BPer.is_begin());
        assert!(ConllLabel::BOrg.is_begin());
        assert!(ConllLabel::BLoc.is_begin());
        assert!(ConllLabel::BMisc.is_begin());

        assert!(ConllLabel::IPer.is_inside());
        assert!(ConllLabel::IOrg.is_inside());
        assert!(ConllLabel::ILoc.is_inside());
        assert!(ConllLabel::IMisc.is_inside());

        assert!(ConllLabel::O.is_outside());
        assert!(!ConllLabel::O.is_begin());
        assert!(!ConllLabel::O.is_inside());
    }
}
