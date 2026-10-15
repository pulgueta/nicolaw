//! The search terms of a claim.

/// A claim gives at most this number of search terms.
const MAX_TERMS: usize = 16;

/// Spanish words that do not help to find a passage. Accents matter: "sí" is in the list, but
/// "si" is short and is dropped by length.
const STOP_WORDS: &[&str] = &[
    "ahí", "al", "allí", "ante", "aquel", "aquella", "aquí", "bajo", "cada", "como", "con",
    "contra", "cual", "cuál", "cuando", "cuándo", "del", "desde", "donde", "dónde", "durante",
    "ella", "ellas", "ellos", "entonces", "entre", "era", "eran", "esa", "esas", "ese", "eso",
    "esos", "esta", "está", "estaba", "estaban", "estado", "están", "estar", "estas", "este",
    "esto", "estos", "estuvo", "fue", "fueron", "había", "han", "has", "hasta", "hay", "hoy",
    "las", "les", "los", "más", "mis", "mismo", "muy", "nos", "nosotros", "otra", "otro", "para",
    "pero", "por", "porque", "qué", "que", "quien", "quién", "según", "ser", "sido", "sin",
    "sobre", "son", "sus", "también", "tan", "tenía", "tiene", "toda", "todas", "todo", "todos",
    "tras", "una", "unas", "uno", "unos", "usted", "ustedes", "sí", "yo",
];

/// The search terms of a claim, for [`crate::store::Store::search_any`]: lowercase words
/// without stop words and short words, in claim order, with no duplicates. A word with a digit
/// is always kept, with its inner punctuation, so that "12/03/2024" and "1.500.000" stay one
/// term (the search makes a phrase of it).
pub fn query_terms(claim: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in claim.split_whitespace() {
        let word = word
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        let has_digit = word.chars().any(|c| c.is_ascii_digit());
        let keep = has_digit || (word.chars().count() >= 3 && !STOP_WORDS.contains(&&*word));
        if keep && !terms.contains(&word) {
            terms.push(word);
        }
        if terms.len() == MAX_TERMS {
            break;
        }
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::query_terms;
    use crate::store::Store;

    #[test]
    fn terms_keep_numbers_dates_and_names_and_drop_stop_words() {
        let terms = query_terms(
            "El acusado dijo que el 12 de marzo de 2024 pagó $1.500.000 en Bogotá, al señor Pérez.",
        );

        assert_eq!(
            terms,
            [
                "acusado",
                "dijo",
                "12",
                "marzo",
                "2024",
                "pagó",
                "1.500.000",
                "bogotá",
                "señor",
                "pérez"
            ]
        );
    }

    #[test]
    fn terms_keep_a_date_with_slashes_as_one_term_and_have_no_duplicates() {
        assert_eq!(
            query_terms("El 12/03/2024 el acusado, el ACUSADO y su hermano."),
            ["12/03/2024", "acusado", "hermano"]
        );
    }

    #[test]
    fn terms_of_fts_syntax_are_safe_to_search() {
        let store = Store::open_in_memory().unwrap();
        let case = store.create_case("Caso").unwrap().id;

        for claim in [
            "\"OR\" NOT acusado* (",
            "a\"b\"c NEAR(x y)",
            "^col:valor -menos +más",
            "",
            "¿?¡! ... --",
        ] {
            let terms = query_terms(claim);
            assert!(
                store.search_any(case, &terms, 20).is_ok(),
                "claim: {claim}, terms: {terms:?}"
            );
        }
        assert!(query_terms("¿?¡! ... --").is_empty());
    }
}
