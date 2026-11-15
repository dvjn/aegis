use anyhow::{Context, Result};
use regex::Regex;
use std::collections::BTreeMap;

/// One named pattern. `validate` rejects a match that the pattern alone cannot
/// tell apart from ordinary text, such as a digit run that fails a checksum.
pub struct Detector {
    pub name: &'static str,
    pub pattern: &'static str,
    pub validate: Option<fn(&str) -> bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding<'a> {
    pub detector: &'a str,
    pub secret: &'a str,
    pub start: usize,
    pub end: usize,
}

/// The detectors of one guardrail, compiled into a single alternation so a
/// body is walked once however many detectors are enabled.
pub struct DetectorSet {
    regex: Option<Regex>,
    detectors: Vec<CompiledDetector>,
}

struct CompiledDetector {
    name: String,
    validate: Option<fn(&str) -> bool>,
}

impl DetectorSet {
    pub fn new(detectors: Vec<&'static Detector>) -> Self {
        let patterns = detectors
            .iter()
            .map(|detector| (detector.name.to_owned(), detector.pattern.to_owned()));
        let mut set = Self::compile(patterns).expect("detector patterns are valid");
        for (compiled, detector) in set.detectors.iter_mut().zip(detectors) {
            compiled.validate = detector.validate;
        }
        set
    }

    /// Compiles user-defined patterns. Each name must be a valid capture group
    /// name; `config` refuses anything else before this is reached.
    pub fn regex(detectors: &BTreeMap<String, String>) -> Result<Self> {
        Self::compile(
            detectors
                .iter()
                .map(|(name, pattern)| (name.clone(), pattern.clone())),
        )
    }

    fn compile(patterns: impl Iterator<Item = (String, String)>) -> Result<Self> {
        let (detectors, alternatives): (Vec<_>, Vec<_>) = patterns
            .map(|(name, pattern)| {
                let alternative = format!("(?P<{name}>{pattern})");
                (
                    CompiledDetector {
                        name,
                        validate: None,
                    },
                    alternative,
                )
            })
            .unzip();
        let regex = if alternatives.is_empty() {
            None
        } else {
            Some(
                Regex::new(&alternatives.join("|"))
                    .context("detector patterns do not compile together")?,
            )
        };
        Ok(Self { regex, detectors })
    }

    pub fn is_empty(&self) -> bool {
        self.regex.is_none()
    }

    pub fn find<'a>(&'a self, text: &'a str) -> Vec<Finding<'a>> {
        let Some(regex) = &self.regex else {
            return Vec::new();
        };
        regex
            .captures_iter(text)
            .filter_map(|captures| {
                self.detectors.iter().find_map(|detector| {
                    let matched = captures.name(&detector.name)?;
                    let text = matched.as_str();
                    if detector.validate.is_some_and(|valid| !valid(text)) {
                        return None;
                    }
                    Some(Finding {
                        detector: &detector.name,
                        secret: text,
                        start: matched.start(),
                        end: matched.end(),
                    })
                })
            })
            .collect()
    }
}

/// Selects `enabled` from `catalogue` by name. `None` selects everything.
pub fn select(
    catalogue: &'static [Detector],
    enabled: Option<&[String]>,
) -> Vec<&'static Detector> {
    match enabled {
        None => catalogue.iter().collect(),
        Some(names) => catalogue
            .iter()
            .filter(|detector| names.iter().any(|name| name == detector.name))
            .collect(),
    }
}

pub fn names(catalogue: &'static [Detector]) -> Vec<&'static str> {
    catalogue.iter().map(|detector| detector.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    static CATALOGUE: [Detector; 2] = [
        Detector {
            name: "letters",
            pattern: r"\b[a-z]{4}\b",
            validate: None,
        },
        Detector {
            name: "checked",
            pattern: r"\b[0-9]{4}\b",
            validate: Some(|text| text.starts_with('1')),
        },
    ];

    #[test]
    fn a_set_finds_only_the_detectors_it_was_given() {
        let set = DetectorSet::new(select(&CATALOGUE, Some(&["letters".to_owned()])));
        let found = set.find("word 1234");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].detector, "letters");
        assert_eq!(found[0].secret, "word");
    }

    #[test]
    fn a_validator_rejects_a_match_the_pattern_cannot_tell_apart() {
        let set = DetectorSet::new(select(&CATALOGUE, None));
        assert_eq!(
            set.find("1234").first().map(|finding| finding.secret),
            Some("1234")
        );
        assert!(set.find("9876").is_empty(), "the validator rejects it");
    }

    #[test]
    fn an_empty_set_matches_nothing_and_compiles_no_pattern() {
        let set = DetectorSet::new(select(&CATALOGUE, Some(&[])));
        assert!(set.is_empty());
        assert!(set.find("word 1234").is_empty());
    }

    #[test]
    fn user_patterns_compile_into_a_set_named_after_their_keys() {
        let set = DetectorSet::regex(&BTreeMap::from([(
            "employee_id".to_owned(),
            r"\bEMP-\d{6}\b".to_owned(),
        )]))
        .expect("the pattern compiles");
        let found = set.find("badge EMP-123456 here");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].detector, "employee_id");
        assert_eq!(found[0].secret, "EMP-123456");
        assert!(
            DetectorSet::regex(&BTreeMap::from([("broken".to_owned(), "(".to_owned())])).is_err()
        );
    }

    #[test]
    fn every_name_is_listed_for_configuration() {
        assert_eq!(names(&CATALOGUE), ["letters", "checked"]);
    }
}
