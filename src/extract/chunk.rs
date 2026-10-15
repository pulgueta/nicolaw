//! Splits page text into chunks of about 180 to 300 words, on sentence boundaries.

/// A chunk closes when it has at least this number of words.
pub const MIN_WORDS: usize = 180;
/// A chunk never has more than this number of words.
pub const MAX_WORDS: usize = 300;

#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    pub page: u32,
    /// Position of the chunk in its page, from 0.
    pub idx: u32,
    pub text: String,
}

/// Splits the text of one page into chunks. Two adjacent chunks share one sentence, so that a
/// statement that crosses a chunk boundary is still found in one chunk.
pub fn chunk(page: u32, text: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current: Vec<Vec<&str>> = Vec::new();
    // True when `current` has a sentence that no chunk contains yet.
    let mut fresh = false;
    let mut emit = |sentences: &[Vec<&str>]| {
        let text = sentences
            .iter()
            .map(|sentence| sentence.join(" "))
            .collect::<Vec<_>>()
            .join(" ");
        chunks.push(Chunk {
            page,
            idx: chunks.len() as u32,
            text,
        });
    };

    for sentence in sentences(text) {
        if word_count(&current) + sentence.len() > MAX_WORDS && fresh {
            emit(&current);
            current = overlap(&current, sentence.len());
        }
        if word_count(&current) + sentence.len() > MAX_WORDS {
            current.clear();
        }
        current.push(sentence);
        fresh = true;
        if word_count(&current) >= MIN_WORDS {
            emit(&current);
            current = overlap(&current, 0);
            fresh = false;
        }
    }
    if fresh {
        emit(&current);
    }
    chunks
}

/// The last sentence of `sentences`, if it leaves room for `next` more words.
fn overlap<'a>(sentences: &[Vec<&'a str>], next: usize) -> Vec<Vec<&'a str>> {
    match sentences.last() {
        Some(last) if last.len() + next <= MAX_WORDS => vec![last.clone()],
        _ => Vec::new(),
    }
}

fn word_count(sentences: &[Vec<&str>]) -> usize {
    sentences.iter().map(Vec::len).sum()
}

/// The sentences of `text`, as lists of words. A sentence ends with a word that ends with
/// ".", "!", "?", or "…". A sentence longer than `MAX_WORDS` is cut into parts.
fn sentences(text: &str) -> Vec<Vec<&str>> {
    let mut sentences = Vec::new();
    let mut current = Vec::new();
    for word in text.split_whitespace() {
        current.push(word);
        if word.ends_with(['.', '!', '?', '…']) || current.len() == MAX_WORDS {
            sentences.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        sentences.push(current);
    }
    sentences
}

#[cfg(test)]
mod tests {
    use super::{Chunk, MAX_WORDS, MIN_WORDS, chunk};

    fn words(text: &str) -> usize {
        text.split_whitespace().count()
    }

    /// A sentence of 10 words with a unique number, for example "Frase 7 ...".
    fn sentence(n: usize) -> String {
        format!("Frase {n} tiene ocho palabras más para la prueba.")
    }

    #[test]
    fn empty_text_has_no_chunks() {
        assert!(chunk(1, "").is_empty());
        assert!(chunk(1, "  \n\t ").is_empty());
    }

    #[test]
    fn short_text_is_one_chunk_with_normalized_spaces() {
        let chunks = chunk(3, "Primera frase.\n\nSegunda   frase.");

        assert_eq!(
            chunks,
            [Chunk {
                page: 3,
                idx: 0,
                text: "Primera frase. Segunda frase.".into()
            }]
        );
    }

    #[test]
    fn long_text_splits_on_sentences_with_one_sentence_of_overlap() {
        let sentences: Vec<String> = (0..100).map(sentence).collect();
        let chunks = chunk(2, &sentences.join(" "));

        assert!(chunks.len() > 2);
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.page, 2);
            assert_eq!(chunk.idx, i as u32);
            assert!(words(&chunk.text) <= MAX_WORDS, "chunk {i} is too long");
            if i + 1 < chunks.len() {
                assert!(words(&chunk.text) >= MIN_WORDS, "chunk {i} is too short");
            }
            // Each chunk starts and ends on a sentence boundary.
            assert!(chunk.text.starts_with("Frase "));
            assert!(chunk.text.ends_with('.'));
        }
        for pair in chunks.windows(2) {
            let last = pair[0].text.rsplit(". ").next().unwrap();
            assert!(pair[1].text.starts_with(last.trim_end_matches('.')));
        }
        // No sentence is lost.
        let all: String = chunks.iter().map(|c| c.text.as_str()).collect();
        for s in &sentences {
            assert!(all.contains(s.as_str()), "missing: {s}");
        }
    }

    #[test]
    fn a_sentence_longer_than_the_maximum_is_cut_by_words() {
        let text = vec!["palabra"; 700].join(" ");

        let chunks = chunk(1, &text);

        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| words(&c.text) <= MAX_WORDS));
        let total: usize = chunks.iter().map(|c| words(&c.text)).sum();
        assert!(total >= 700);
    }
}
