//! Shell-style words for the Arguments and Login command fields.

pub(super) fn split(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote = None;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), _) => word.push(ch),
            (_, '\\') => {
                let next = chars.next().ok_or("Trailing backslash")?;
                if quote == Some('"') && !matches!(next, '$' | '`' | '"' | '\\' | '\n') {
                    word.push('\\');
                }
                if next != '\n' {
                    word.push(next);
                    started = true;
                }
            }
            (Some(_), _) => word.push(ch),
            (None, '\'' | '"') => {
                quote = Some(ch);
                started = true;
            }
            (None, _) if ch.is_whitespace() => {
                if std::mem::take(&mut started) {
                    words.push(std::mem::take(&mut word));
                }
            }
            (None, _) => {
                word.push(ch);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("Unclosed quote".into());
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

pub(super) fn join(words: &[String]) -> String {
    words.iter().map(|word| super::super::super::util::quote_if_needed(word)).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_words_and_round_trip() {
        assert_eq!(split("acp --agent-engine=v3").unwrap(), ["acp", "--agent-engine=v3"]);
        let words = vec!["a b".into(), "".into(), "it's".into(), "a\\b".into(), "a\"b".into()];
        assert_eq!(split(&join(&words)).unwrap(), words);
        assert_eq!(
            split(r#"one\ two "three four" 'five six' x"y z" "a\qb""#).unwrap(),
            ["one two", "three four", "five six", "xy z", "a\\qb"]
        );
        assert!(split("'unfinished").is_err());
        assert!(split("unfinished\\").is_err());
    }
}
