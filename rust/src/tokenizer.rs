//! Custom code-aware tokenizer for sakuin.
//!
//! Splits text on code-relevant boundaries:
//! - camelCase / PascalCase boundaries (`getFoo` → `get`, `Foo`)
//! - snake_case underscores (`get_foo` → `get`, `foo`)
//! - punctuation and whitespace
//! - digits vs. letters (`abc123` → `abc`, `123`)
//!
//! Combined with `LowerCaser`, this enables case-insensitive substring matching
//! within identifiers: searching for `get` matches `getFoo`, `get_bar`, etc.

use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

/// Name used to register this tokenizer with Tantivy's `TokenizerManager`.
pub const CODE_TOKENIZER_NAME: &str = "code";

/// A tokenizer that splits source code and file paths on word sub-boundaries.
#[derive(Clone, Default)]
pub struct CodeTokenizer {
    token: Token,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharKind {
    Upper,
    Lower,
    Digit,
    /// Anything else — whitespace, punctuation, path separators, etc.
    Other,
}

fn classify(c: char) -> CharKind {
    if c.is_uppercase() {
        CharKind::Upper
    } else if c.is_lowercase() {
        CharKind::Lower
    } else if c.is_ascii_digit() {
        CharKind::Digit
    } else {
        CharKind::Other
    }
}

pub fn split_code(text: &str) -> Vec<(usize, usize)> {
    let mut tokens = Vec::new();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    if chars.is_empty() {
        return tokens;
    }

    let n = chars.len();
    let mut start = None; // byte offset of current token start

    for i in 0..n {
        let (byte_pos, ch) = chars[i];
        let kind = classify(ch);

        if kind == CharKind::Other {
            // Separator — flush current token
            if let Some(s) = start.take() {
                tokens.push((s, byte_pos));
            }
            continue;
        }

        if start.is_none() {
            // Start a new token
            start = Some(byte_pos);
            continue;
        }

        // We're inside a token. Check if we need to split.
        let prev_kind = classify(chars[i - 1].1);

        // Handle acronym-to-word boundary: HTTPSClient → HTTPS|Client
        // When we see Upper followed by Lower (the "Cl" in "Client"), and the
        // char before that was also Upper (the "S" in "HTTPS"), we need to
        // split *before the previous char* (the "C"), not before the current
        // char. We detect this by looking ahead: if current is Upper and next
        // is Lower, and prev was Upper, split before current.
        if kind == CharKind::Upper
            && i + 1 < n
            && classify(chars[i + 1].1) == CharKind::Lower
            && prev_kind == CharKind::Upper
        {
            if let Some(s) = start.take() {
                tokens.push((s, byte_pos));
            }
            start = Some(byte_pos);
            continue;
        }

        let split = match (prev_kind, kind) {
            // camelCase: get|Foo
            (CharKind::Lower, CharKind::Upper) => true,
            // digits vs. letters
            (CharKind::Digit, CharKind::Lower) => true,
            (CharKind::Digit, CharKind::Upper) => true,
            (CharKind::Lower, CharKind::Digit) => true,
            (CharKind::Upper, CharKind::Digit) => true,
            _ => false,
        };

        if split {
            if let Some(s) = start.take() {
                tokens.push((s, byte_pos));
            }
            start = Some(byte_pos);
        }
    }

    // Flush trailing token
    if let Some(s) = start {
        tokens.push((s, text.len()));
    }

    tokens
}

pub struct CodeTokenStream<'a> {
    text: &'a str,
    spans: Vec<(usize, usize)>,
    index: usize,
    token: &'a mut Token,
}

impl Tokenizer for CodeTokenizer {
    type TokenStream<'a> = CodeTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> CodeTokenStream<'a> {
        self.token.reset();
        let spans = split_code(text);
        CodeTokenStream {
            text,
            spans,
            index: 0,
            token: &mut self.token,
        }
    }
}

impl<'a> TokenStream for CodeTokenStream<'a> {
    fn advance(&mut self) -> bool {
        if self.index >= self.spans.len() {
            return false;
        }
        let (from, to) = self.spans[self.index];
        self.token.text.clear();
        self.token.text.push_str(&self.text[from..to]);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tokenize(text: &str) -> Vec<String> {
        let mut tokenizer = CodeTokenizer::default();
        let mut stream = tokenizer.token_stream(text);
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        tokens
    }

    #[test]
    fn test_camel_case() {
        assert_eq!(tokenize("getFoo"), vec!["get", "Foo"]);
        assert_eq!(tokenize("getFooBar"), vec!["get", "Foo", "Bar"]);
    }

    #[test]
    fn test_pascal_case() {
        assert_eq!(tokenize("FooBar"), vec!["Foo", "Bar"]);
    }

    #[test]
    fn test_snake_case() {
        assert_eq!(tokenize("get_foo_bar"), vec!["get", "foo", "bar"]);
    }

    #[test]
    fn test_screaming_snake() {
        assert_eq!(tokenize("MAX_FILE_SIZE"), vec!["MAX", "FILE", "SIZE"]);
    }

    #[test]
    fn test_acronym_then_word() {
        assert_eq!(tokenize("HTTPSClient"), vec!["HTTPS", "Client"]);
        assert_eq!(tokenize("XMLParser"), vec!["XML", "Parser"]);
    }

    #[test]
    fn test_digits() {
        assert_eq!(tokenize("abc123def"), vec!["abc", "123", "def"]);
        assert_eq!(tokenize("v2beta1"), vec!["v", "2", "beta", "1"]);
    }

    #[test]
    fn test_path() {
        assert_eq!(
            tokenize("src/foo/bar_baz.rs"),
            vec!["src", "foo", "bar", "baz", "rs"]
        );
    }

    #[test]
    fn test_punctuation() {
        assert_eq!(tokenize("fn main() {"), vec!["fn", "main"]);
    }

    #[test]
    fn test_empty() {
        assert_eq!(tokenize(""), Vec::<String>::new());
    }

    #[test]
    fn test_whitespace_only() {
        assert_eq!(tokenize("   "), Vec::<String>::new());
    }

    #[test]
    fn test_single_word() {
        assert_eq!(tokenize("hello"), vec!["hello"]);
    }

    #[test]
    fn test_mixed_code() {
        assert_eq!(
            tokenize("let myVar = someFunc(arg1, arg2);"),
            vec!["let", "my", "Var", "some", "Func", "arg", "1", "arg", "2"]
        );
    }
}
