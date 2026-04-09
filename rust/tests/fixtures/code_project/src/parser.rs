pub fn parse_token(input: &str) -> Option<(Token, &str)> {
    if input.starts_with('(') {
        Some((Token::LParen, &input[1..]))
    } else if input.starts_with(')') {
        Some((Token::RParen, &input[1..]))
    } else {
        None
    }
}

pub enum Token {
    LParen,
    RParen,
    Ident(String),
}
