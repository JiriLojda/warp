use fuzzy_match::FuzzyMatchResult;
use warp_completer::completer::{
    EngineFileType, Match, MatchRequirement, MatchStrategy, MatchedSuggestion,
    OptionCaseSensitivity, Priority, Suggestion, SuggestionResults, SuggestionType,
    TopLevelCommandCaseSensitivity,
};
use warp_completer::meta::Span;

use super::{results_from_json, results_to_json};

#[test]
fn results_survive_a_round_trip() {
    let mut directory = Suggestion::new(
        "src/",
        "src/",
        Some("Directory".to_owned()),
        SuggestionType::Argument,
        Priority::new(10),
    );
    directory.file_type = Some(EngineFileType::Directory);
    let results = SuggestionResults {
        replacement_span: Span::new(3, 5),
        match_strategy: MatchStrategy::Fuzzy,
        suggestions: vec![
            MatchedSuggestion::new(
                directory,
                Match::Prefix {
                    is_case_sensitive: true,
                },
            ),
            MatchedSuggestion::new(
                Suggestion::new(
                    "--force",
                    "--force",
                    None,
                    SuggestionType::Option(
                        MatchRequirement::UniquePrefixOnly,
                        OptionCaseSensitivity::CaseInsensitive,
                    ),
                    Priority::new(-3),
                ),
                Match::Fuzzy {
                    match_result: FuzzyMatchResult {
                        score: 42,
                        matched_indices: vec![2, 3],
                    },
                },
            ),
            MatchedSuggestion::new(
                Suggestion::new(
                    "git",
                    "git",
                    None,
                    SuggestionType::Command(TopLevelCommandCaseSensitivity::CaseInsensitive),
                    Priority::new(0),
                ),
                Match::Exact {
                    is_case_sensitive: false,
                },
            ),
        ],
    };
    assert_eq!(results_from_json(&results_to_json(&results)), Some(results));
}
