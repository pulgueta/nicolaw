//! The JSON output of the language model, and the passages for the NLI model.

use serde::Deserialize;

use crate::store::SuggestionKind;

/// A segment gives at most this number of claims.
pub const MAX_CLAIMS: usize = 5;

/// A premise has at most this number of words. With a claim, the pair stays below the 512
/// tokens of the NLI model (Spanish text is about 1.5 to 2 tokens per word).
pub const MAX_PREMISE_WORDS: usize = 150;

/// The claims in the output of the language model: the strings of the first JSON array,
/// trimmed, without empty strings, at most [`MAX_CLAIMS`]. Output that is not an array of
/// strings gives no claims.
pub fn parse_claims(text: &str) -> Vec<String> {
    let Some(json) = between(text, '[', ']') else {
        return Vec::new();
    };
    let Ok(claims) = serde_json::from_str::<Vec<String>>(json) else {
        return Vec::new();
    };
    claims
        .into_iter()
        .map(|claim| claim.trim().to_owned())
        .filter(|claim| !claim.is_empty())
        .take(MAX_CLAIMS)
        .collect()
}

/// The explanation of a flagged pair, after validation.
#[derive(Clone, Debug, PartialEq)]
pub struct Explanation {
    pub contradiction: bool,
    pub kind: SuggestionKind,
    /// In Spanish, for the lawyer.
    pub explanation: String,
    /// An exact part of the claim.
    pub quote_claim: String,
    /// An exact part of the evidence chunk.
    pub quote_evidence: String,
}

#[derive(Deserialize)]
struct RawExplanation {
    contradiction: bool,
    kind: String,
    explanation_es: String,
    quote_claim: String,
    quote_evidence: String,
}

/// Reads the JSON object of the language model. Returns `None` when the JSON is not valid, the
/// kind is unknown, or a quote is empty or is not an exact substring of its source: the model
/// must not invent or change a quote.
pub fn parse_explanation(text: &str, claim: &str, chunk: &str) -> Option<Explanation> {
    let raw: RawExplanation = serde_json::from_str(between(text, '{', '}')?).ok()?;
    let exact = |quote: &str, source: &str| !quote.trim().is_empty() && source.contains(quote);
    if !exact(&raw.quote_claim, claim) || !exact(&raw.quote_evidence, chunk) {
        return None;
    }
    Some(Explanation {
        contradiction: raw.contradiction,
        kind: SuggestionKind::parse(&raw.kind)?,
        explanation: raw.explanation_es.trim().to_owned(),
        quote_claim: raw.quote_claim,
        quote_evidence: raw.quote_evidence,
    })
}

/// The text from the first `open` to the last `close`, both included.
fn between(text: &str, open: char, close: char) -> Option<&str> {
    let start = text.find(open)?;
    let end = text.rfind(close)?;
    (start < end).then(|| &text[start..=end])
}

/// Splits an evidence chunk into premises of at most [`MAX_PREMISE_WORDS`] words, on sentence
/// boundaries. Each premise is an exact substring of the chunk. A sentence that is longer than
/// the maximum is cut by words.
pub fn premises(chunk: &str) -> Vec<String> {
    // The byte range of each word, and true if the word ends a sentence.
    let mut words: Vec<(usize, usize, bool)> = Vec::new();
    let mut start = None;
    for (index, c) in chunk.char_indices().chain([(chunk.len(), ' ')]) {
        match (c.is_whitespace(), start) {
            (true, Some(begin)) => {
                let word = &chunk[begin..index];
                words.push((begin, index, word.ends_with(['.', '!', '?', '…'])));
                start = None;
            }
            (false, None) => start = Some(index),
            _ => {}
        }
    }

    let mut parts = Vec::new();
    let mut first = 0;
    while first < words.len() {
        let limit = (first + MAX_PREMISE_WORDS).min(words.len());
        // The end of the last whole sentence that fits, else the word limit.
        let end = (first..limit)
            .rev()
            .find(|&i| words[i].2)
            .filter(|_| limit < words.len())
            .map_or(limit, |i| i + 1);
        parts.push(chunk[words[first].0..words[end - 1].1].to_owned());
        first = end;
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::{
        Explanation, MAX_CLAIMS, MAX_PREMISE_WORDS, parse_claims, parse_explanation, premises,
    };
    use crate::store::SuggestionKind;

    const CLAIM: &str = "El acusado estaba en Medellín el 12 de marzo.";
    const CHUNK: &str =
        "Según el registro del hotel, el acusado estaba en Bogotá el 12 de marzo de 2024.";

    #[test]
    fn claims_are_the_strings_of_the_array_trimmed_and_at_most_five() {
        let text = r#"["  El acusado estaba en Medellín.  ", "", "Pagó 1.500.000 pesos.", "c3", "c4", "c5", "c6"]"#;

        let claims = parse_claims(text);

        assert_eq!(claims.len(), MAX_CLAIMS);
        assert_eq!(claims[0], "El acusado estaba en Medellín.");
        assert_eq!(claims[1], "Pagó 1.500.000 pesos.");
    }

    #[test]
    fn claims_are_found_in_text_around_the_array_and_bad_json_gives_none() {
        assert_eq!(parse_claims("Claro: [\"Uno.\"] fin"), ["Uno."]);
        assert!(parse_claims("[\"sin cierre").is_empty());
        assert!(parse_claims("{\"a\": 1}").is_empty());
        assert!(parse_claims("[1, 2]").is_empty());
    }

    fn json(contradiction: bool, kind: &str, quote_claim: &str, quote_evidence: &str) -> String {
        serde_json::json!({
            "contradiction": contradiction,
            "kind": kind,
            "explanation_es": "La prueba ubica al acusado en Bogotá ese día.",
            "quote_claim": quote_claim,
            "quote_evidence": quote_evidence,
        })
        .to_string()
    }

    #[test]
    fn a_valid_explanation_has_exact_quotes_of_the_claim_and_the_chunk() {
        let text = json(
            true,
            "location",
            "en Medellín",
            "estaba en Bogotá el 12 de marzo",
        );

        assert_eq!(
            parse_explanation(&text, CLAIM, CHUNK),
            Some(Explanation {
                contradiction: true,
                kind: SuggestionKind::Location,
                explanation: "La prueba ubica al acusado en Bogotá ese día.".into(),
                quote_claim: "en Medellín".into(),
                quote_evidence: "estaba en Bogotá el 12 de marzo".into(),
            })
        );
    }

    #[test]
    fn an_explanation_with_a_quote_that_is_not_exact_is_rejected() {
        // Changed words, changed accents, or an empty quote.
        for (quote_claim, quote_evidence) in [
            ("en Cali", "estaba en Bogotá"),
            ("en Medellin", "estaba en Bogotá"),
            ("en Medellín", "estaba en Bogota"),
            ("en Medellín", "el acusado estuvo en Bogotá"),
            ("", "estaba en Bogotá"),
            ("en Medellín", "  "),
        ] {
            let text = json(true, "location", quote_claim, quote_evidence);
            assert_eq!(
                parse_explanation(&text, CLAIM, CHUNK),
                None,
                "{quote_claim} / {quote_evidence}"
            );
        }
    }

    #[test]
    fn an_explanation_with_a_bad_kind_or_bad_json_is_rejected() {
        let bad_kind = json(true, "weather", "en Medellín", "estaba en Bogotá");
        let missing = r#"{"contradiction": true, "kind": "date"}"#;

        assert_eq!(parse_explanation(&bad_kind, CLAIM, CHUNK), None);
        assert_eq!(parse_explanation(missing, CLAIM, CHUNK), None);
        assert_eq!(parse_explanation("no", CLAIM, CHUNK), None);
    }

    #[test]
    fn an_explanation_is_found_in_text_around_the_object() {
        let text = format!(
            "Respuesta: {} ",
            json(false, "date", "el 12 de marzo", "el 12 de marzo de 2024")
        );

        let explanation = parse_explanation(&text, CLAIM, CHUNK).unwrap();

        assert!(!explanation.contradiction);
        assert_eq!(explanation.kind, SuggestionKind::Date);
    }

    #[test]
    fn a_short_chunk_is_one_premise() {
        assert_eq!(premises(CHUNK), [CHUNK]);
        assert!(premises("   ").is_empty());
    }

    #[test]
    fn a_long_chunk_splits_on_sentences_into_exact_parts() {
        let sentences: Vec<String> = (0..40)
            .map(|n| format!("Frase {n} tiene ocho palabras más para la prueba."))
            .collect();
        let text = sentences.join(" ");

        let parts = premises(&text);

        assert!(parts.len() >= 3);
        for part in &parts {
            assert!(text.contains(part.as_str()), "not exact: {part}");
            assert!(part.split_whitespace().count() <= MAX_PREMISE_WORDS);
            assert!(part.starts_with("Frase ") && part.ends_with('.'));
        }
        let all = parts.join(" ");
        assert!(sentences.iter().all(|s| all.contains(s.as_str())));
    }

    #[test]
    fn a_sentence_longer_than_the_maximum_is_cut_by_words() {
        let text = vec!["palabra"; MAX_PREMISE_WORDS * 2 + 10].join(" ");

        let parts = premises(&text);

        assert_eq!(parts.len(), 3);
        assert!(
            parts
                .iter()
                .all(|part| part.split_whitespace().count() <= MAX_PREMISE_WORDS)
        );
    }
}
