//! Carries [`SuggestionResults`] as JSON, so viewers can show completions computed on the sharer.
use fuzzy_match::FuzzyMatchResult;
use serde_json::{Value, json};
use warp_completer::completer::{
    EngineFileType, Match, MatchRequirement, MatchStrategy, MatchedSuggestion,
    OptionCaseSensitivity, Priority, Suggestion, SuggestionResults, SuggestionType,
    TopLevelCommandCaseSensitivity,
};
use warp_completer::meta::Span;

fn case_name(is_case_sensitive: bool) -> &'static str {
    if is_case_sensitive {
        "case_sensitive"
    } else {
        "case_insensitive"
    }
}

fn type_json(suggestion_type: &SuggestionType) -> Value {
    match suggestion_type {
        SuggestionType::Command(case) => json!({
            "kind": "command",
            "case": case_name(*case == TopLevelCommandCaseSensitivity::CaseSensitive),
        }),
        SuggestionType::Variable => json!({ "kind": "variable" }),
        SuggestionType::Argument => json!({ "kind": "argument" }),
        SuggestionType::Subcommand => json!({ "kind": "subcommand" }),
        SuggestionType::Option(requirement, case) => json!({
            "kind": "option",
            "unique_prefix": *requirement == MatchRequirement::UniquePrefixOnly,
            "case": case_name(*case == OptionCaseSensitivity::CaseSensitive),
        }),
    }
}

fn type_from_json(value: &Value) -> SuggestionType {
    let case_sensitive = value["case"] != "case_insensitive";
    match value["kind"].as_str() {
        Some("command") => SuggestionType::Command(if case_sensitive {
            TopLevelCommandCaseSensitivity::CaseSensitive
        } else {
            TopLevelCommandCaseSensitivity::CaseInsensitive
        }),
        Some("variable") => SuggestionType::Variable,
        Some("subcommand") => SuggestionType::Subcommand,
        Some("option") => SuggestionType::Option(
            if value["unique_prefix"] == true {
                MatchRequirement::UniquePrefixOnly
            } else {
                MatchRequirement::EntireName
            },
            if case_sensitive {
                OptionCaseSensitivity::CaseSensitive
            } else {
                OptionCaseSensitivity::CaseInsensitive
            },
        ),
        _ => SuggestionType::Argument,
    }
}

fn match_json(match_type: &Match) -> Value {
    match match_type {
        Match::Prefix { is_case_sensitive } => {
            json!({ "kind": "prefix", "case_sensitive": is_case_sensitive })
        }
        Match::Exact { is_case_sensitive } => {
            json!({ "kind": "exact", "case_sensitive": is_case_sensitive })
        }
        Match::Fuzzy { match_result } => json!({
            "kind": "fuzzy",
            "score": match_result.score,
            "matched_indices": match_result.matched_indices,
        }),
    }
}

fn match_from_json(value: &Value) -> Match {
    let is_case_sensitive = value["case_sensitive"] == true;
    match value["kind"].as_str() {
        Some("exact") => Match::Exact { is_case_sensitive },
        Some("fuzzy") => Match::Fuzzy {
            match_result: FuzzyMatchResult {
                score: value["score"].as_i64().unwrap_or_default(),
                matched_indices: serde_json::from_value(value["matched_indices"].clone())
                    .unwrap_or_default(),
            },
        },
        _ => Match::Prefix { is_case_sensitive },
    }
}

fn strategy_name(strategy: MatchStrategy) -> &'static str {
    match strategy {
        MatchStrategy::CaseSensitive => "case_sensitive",
        MatchStrategy::CaseInsensitive => "case_insensitive",
        MatchStrategy::Fuzzy => "fuzzy",
    }
}

fn strategy_from_name(name: Option<&str>) -> MatchStrategy {
    match name {
        Some("case_sensitive") => MatchStrategy::CaseSensitive,
        Some("fuzzy") => MatchStrategy::Fuzzy,
        _ => MatchStrategy::CaseInsensitive,
    }
}

/// Spans are byte offsets into the text the results were computed for.
#[cfg_attr(target_family = "wasm", allow(dead_code))]
pub(crate) fn results_to_json(results: &SuggestionResults) -> Value {
    json!({
        "replacement_span": {
            "start": results.replacement_span.start(),
            "end": results.replacement_span.end(),
        },
        "match_strategy": strategy_name(results.match_strategy),
        "suggestions": results.suggestions.iter().map(|matched| {
            let suggestion = &matched.suggestion;
            json!({
                "display": suggestion.display.as_str(),
                "replacement": suggestion.replacement.as_str(),
                "description": suggestion.description,
                "type": type_json(&suggestion.suggestion_type),
                "priority": suggestion.priority.value(),
                "is_hidden": suggestion.is_hidden,
                "file_type": suggestion.file_type,
                "is_abbreviation": suggestion.is_abbreviation,
                "match": match_json(&matched.match_type),
            })
        }).collect::<Vec<_>>(),
    })
}

#[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
pub(crate) fn results_from_json(value: &Value) -> Option<SuggestionResults> {
    let span = &value["replacement_span"];
    let suggestions = value["suggestions"]
        .as_array()?
        .iter()
        .map(|item| {
            let mut suggestion = Suggestion::new(
                item["display"].as_str().unwrap_or_default(),
                item["replacement"].as_str().unwrap_or_default(),
                item["description"].as_str().map(str::to_owned),
                type_from_json(&item["type"]),
                Priority::new(item["priority"].as_i64().unwrap_or_default() as i32),
            );
            suggestion.is_hidden = item["is_hidden"] == true;
            suggestion.is_abbreviation = item["is_abbreviation"] == true;
            suggestion.file_type =
                serde_json::from_value::<Option<EngineFileType>>(item["file_type"].clone())
                    .unwrap_or_default();
            MatchedSuggestion::new(suggestion, match_from_json(&item["match"]))
        })
        .collect();
    Some(SuggestionResults {
        replacement_span: Span::new(
            span["start"].as_u64()? as usize,
            span["end"].as_u64()? as usize,
        ),
        suggestions,
        match_strategy: strategy_from_name(value["match_strategy"].as_str()),
    })
}

#[cfg(test)]
#[path = "remote_completions_tests.rs"]
mod tests;
