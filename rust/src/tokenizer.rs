//! Character-aligned trigram tokenizer. Pairs with the line-scan verifier in
//! `search.rs`, which is the source of truth for matching.

use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

pub const TRIGRAM_TOKENIZER_NAME: &str = "trigram";

#[derive(Clone, Default)]
pub struct TrigramTokenizer {
    token: Token,
}

pub struct TrigramTokenStream<'a> {
    text: &'a str,
    /// Byte offset of each char start, plus `text.len()` as a sentinel.
    char_offsets: Vec<usize>,
    index: usize,
    token: &'a mut Token,
}

impl Tokenizer for TrigramTokenizer {
    type TokenStream<'a> = TrigramTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> TrigramTokenStream<'a> {
        self.token.reset();
        let mut char_offsets: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
        char_offsets.push(text.len());
        TrigramTokenStream {
            text,
            char_offsets,
            index: 0,
            token: &mut self.token,
        }
    }
}

impl<'a> TokenStream for TrigramTokenStream<'a> {
    fn advance(&mut self) -> bool {
        if self.index + 3 >= self.char_offsets.len() {
            return false;
        }
        let from = self.char_offsets[self.index];
        let to = self.char_offsets[self.index + 3];
        self.token.text.clear();
        self.token.text.push_str(&self.text[from..to]);
        // Must match `query_trigrams` exactly — correctness invariant.
        self.token.text.make_ascii_lowercase();
        self.token.offset_from = from;
        self.token.offset_to = to;
        self.token.position = self.index;
        self.index += 1;
        true
    }

    fn token(&self) -> &Token {
        self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}

/// Returns an empty vec for inputs shorter than 3 chars; callers must fall
/// back to another retrieval strategy (typically `AllQuery`).
pub fn query_trigrams(text: &str) -> Vec<String> {
    let mut char_offsets: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    char_offsets.push(text.len());
    if char_offsets.len() < 4 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(char_offsets.len() - 3);
    for i in 0..char_offsets.len() - 3 {
        let mut tg = text[char_offsets[i]..char_offsets[i + 3]].to_string();
        tg.make_ascii_lowercase();
        out.push(tg);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokenize(text: &str) -> Vec<String> {
        let mut tokenizer = TrigramTokenizer::default();
        let mut stream = tokenizer.token_stream(text);
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        tokens
    }

    #[test]
    fn test_basic() {
        assert_eq!(tokenize("abcd"), vec!["abc", "bcd"]);
    }

    #[test]
    fn test_lowercase() {
        assert_eq!(tokenize("ABCD"), vec!["abc", "bcd"]);
        assert_eq!(tokenize("AbCd"), vec!["abc", "bcd"]);
    }

    #[test]
    fn test_too_short() {
        assert_eq!(tokenize(""), Vec::<String>::new());
        assert_eq!(tokenize("a"), Vec::<String>::new());
        assert_eq!(tokenize("ab"), Vec::<String>::new());
    }

    #[test]
    fn test_exactly_three() {
        assert_eq!(tokenize("abc"), vec!["abc"]);
    }

    #[test]
    fn test_punctuation_and_spaces_preserved() {
        assert_eq!(tokenize("a b"), vec!["a b"]);
        assert_eq!(tokenize("a::"), vec!["a::"]);
    }

    #[test]
    fn test_multibyte_chars() {
        let toks = tokenize("héllo");
        assert_eq!(toks, vec!["hél", "éll", "llo"]);
    }

    #[test]
    fn test_query_trigrams_matches_tokenizer() {
        for input in &["foo", "FooBar", "std::vector", "a b c", "héllo"] {
            assert_eq!(
                query_trigrams(input),
                tokenize(input),
                "mismatch for {input:?}"
            );
        }
    }

    #[test]
    fn test_query_trigrams_short() {
        assert!(query_trigrams("").is_empty());
        assert!(query_trigrams("a").is_empty());
        assert!(query_trigrams("ab").is_empty());
        assert_eq!(query_trigrams("abc"), vec!["abc"]);
    }

    #[test]
    fn test_offsets() {
        let mut tokenizer = TrigramTokenizer::default();
        let mut stream = tokenizer.token_stream("abcd");
        let mut offsets = Vec::new();
        while stream.advance() {
            let t = stream.token();
            offsets.push((t.offset_from, t.offset_to));
        }
        assert_eq!(offsets, vec![(0, 3), (1, 4)]);
    }
}
