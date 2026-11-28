#[derive(Debug, PartialEq)]
pub(crate) enum SearchExpression {
    Term(Field, String),
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
    Not(Box<Self>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Field {
    Text,
    IndexedText,
    From,
    To,
    Cc,
    Subject,
}

#[derive(Debug, PartialEq)]
enum Token {
    Term(Field, String),
    And,
    Or,
    Not,
}

fn tokens(input: &str) -> Option<Vec<Token>> {
    let mut chars = input.chars().peekable();
    let mut tokens = Vec::new();
    while chars.peek().is_some() {
        let mut word = String::new();
        let mut quoted = false;
        let mut has_quotes = false;
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    quoted = !quoted;
                    has_quotes = true;
                }
                '\\' if quoted => word.push(chars.next()?),
                c if c.is_whitespace() && !quoted => break,
                c => word.push(c),
            }
        }
        if quoted {
            return None;
        }
        if word.is_empty() {
            continue;
        }
        let field = word.split_once(':').and_then(|(name, value)| {
            let field = match name.to_ascii_lowercase().as_str() {
                "text" => Field::IndexedText,
                "from" => Field::From,
                "to" => Field::To,
                "cc" => Field::Cc,
                "subject" => Field::Subject,
                _ => return None,
            };
            Some((field, value.to_owned()))
        });
        let token = match (has_quotes, field, word.as_str()) {
            (_, Some((_, value)), _) if value.is_empty() => return None,
            (_, Some((field, value)), _) => Token::Term(field, value),
            (false, None, "AND") => Token::And,
            (false, None, "OR") => Token::Or,
            (false, None, "NOT") => Token::Not,
            _ => Token::Term(Field::Text, word),
        };
        tokens.push(token);
        if tokens.len() > 64 {
            return None;
        }
    }
    Some(tokens)
}

fn term(tokens: &mut std::iter::Peekable<std::vec::IntoIter<Token>>) -> Option<SearchExpression> {
    match tokens.next()? {
        Token::Term(field, text) => Some(SearchExpression::Term(field, text)),
        Token::Not => Some(SearchExpression::Not(Box::new(term(tokens)?))),
        _ => None,
    }
}

fn conjunction(
    tokens: &mut std::iter::Peekable<std::vec::IntoIter<Token>>,
) -> Option<SearchExpression> {
    let mut expression = term(tokens)?;
    while matches!(
        tokens.peek(),
        Some(Token::And | Token::Term(..) | Token::Not)
    ) {
        if tokens.peek() == Some(&Token::And) {
            tokens.next();
        }
        expression = SearchExpression::And(Box::new(expression), Box::new(term(tokens)?));
    }
    Some(expression)
}

fn parse(input: &str) -> Option<SearchExpression> {
    let tokens = tokens(input)?;
    if !input.contains('"')
        && tokens
            .iter()
            .all(|token| matches!(token, Token::Term(Field::Text, _)))
    {
        return None;
    }
    let mut tokens = tokens.into_iter().peekable();
    let mut expression = conjunction(&mut tokens)?;
    while tokens.peek() == Some(&Token::Or) {
        tokens.next();
        expression =
            SearchExpression::Or(Box::new(expression), Box::new(conjunction(&mut tokens)?));
    }
    tokens.next().is_none().then_some(expression)
}

pub(crate) fn keywords(input: &str) -> SearchExpression {
    parse(input).unwrap_or_else(|| SearchExpression::Term(Field::Text, input.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{keywords, Field, SearchExpression::*};

    #[test]
    fn parses_fields_quotes_and_boolean_precedence() {
        assert_eq!(
            keywords("from:alice OR subject:\"release plan\" AND NOT to:bob"),
            Or(
                Box::new(Term(Field::From, "alice".into())),
                Box::new(And(
                    Box::new(Term(Field::Subject, "release plan".into())),
                    Box::new(Not(Box::new(Term(Field::To, "bob".into())))),
                ))
            ),
        );
        assert_eq!(
            keywords("release plan"),
            Term(Field::Text, "release plan".into())
        );
    }
}
