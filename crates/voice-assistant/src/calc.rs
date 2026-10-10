//! Spoken sums: "what's 5 plus 5", "12 times 3", "15 percent of 80", "100 divided by 4", left to
//! right as people say them. Numbers as digits, the way the transcriber writes them.

use crate::lang::Lang;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    PercentOf,
}

/// The sum as said ("5 plus 5") and its result, if `text` is a sum.
pub fn parse(text: &str) -> Option<(String, f64)> {
    let lower = text.to_lowercase().replace(['?', '!'], " ").replace(',', "");
    let tokens: Vec<String> = lower
        .split_whitespace()
        .map(|t| t.trim_end_matches('.').to_owned())
        .flat_map(split_symbols)
        .filter(|t| !t.is_empty())
        .collect();
    let start = tokens.iter().position(|t| number(t).is_some())?;
    let mut value = number(&tokens[start])?;
    let mut said = vec![tokens[start].clone()];
    let mut i = start + 1;
    let mut ops = 0;
    while i < tokens.len() {
        let Some((op, width)) = operator(&tokens[i..]) else { break };
        let Some(next) = tokens.get(i + width).and_then(|t| number(t)) else { break };
        said.extend(tokens[i..=i + width].iter().cloned());
        value = match op {
            Op::Add => value + next,
            Op::Sub => value - next,
            Op::Mul => value * next,
            Op::Div if next == 0.0 => return None,
            Op::Div => value / next,
            Op::PercentOf => value / 100.0 * next,
        };
        ops += 1;
        i += width + 1;
    }
    // Anything but a sum after the numbers ("5 plus 5 people") is not arithmetic.
    let rest = &tokens[i..];
    (ops > 0 && rest.iter().all(|t| ["equal", "equals", "is", "blir", "er"].contains(&t.as_str())))
        .then(|| (said.join(" "), value))
}

/// "5 plus 5 is 10."
pub fn answer(said: &str, value: f64, lang: Lang) -> String {
    let is = if lang == Lang::Norwegian { "er" } else { "is" };
    let said = said.replace('*', "times").replace('/', "divided by").replace('+', "plus");
    format!("{} {is} {}.", capitalize(&said), format_number(value, lang))
}

fn format_number(value: f64, lang: Lang) -> String {
    if (value - value.round()).abs() < 1e-9 {
        return format!("{}", value.round() as i64);
    }
    let text = format!("{:.2}", value).trim_end_matches('0').trim_end_matches('.').to_owned();
    if lang == Lang::Norwegian {
        text.replace('.', ",")
    } else {
        text
    }
}

fn number(token: &str) -> Option<f64> {
    token.replace(',', ".").parse::<f64>().ok().filter(|n| n.is_finite())
}

/// "5+5" and "10%" as separate tokens.
fn split_symbols(token: String) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in token.chars() {
        if "+*/x×%".contains(c) && !current.is_empty() && current.chars().all(|d| d.is_ascii_digit() || d == '.') {
            out.push(std::mem::take(&mut current));
            out.push(c.to_string());
        } else {
            current.push(c);
        }
    }
    out.push(current);
    out
}

/// The operator at the start of `tokens` and how many tokens it takes.
fn operator(tokens: &[String]) -> Option<(Op, usize)> {
    let t = |i: usize| tokens.get(i).map(String::as_str).unwrap_or_default();
    Some(match (t(0), t(1)) {
        ("plus" | "+" | "pluss" | "and", _) => (Op::Add, 1),
        ("minus" | "-", _) => (Op::Sub, 1),
        ("times" | "x" | "×" | "*" | "ganger", _) => (Op::Mul, 1),
        ("multiplied" | "multiplisert", "by" | "med") => (Op::Mul, 2),
        ("divided" | "delt", "by" | "på") => (Op::Div, 2),
        ("over" | "/", _) => (Op::Div, 1),
        ("percent" | "%" | "prosent", "of" | "av") => (Op::PercentOf, 2),
        _ => return None,
    })
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calc(text: &str) -> Option<f64> {
        parse(text).map(|(_, v)| v)
    }

    #[test]
    fn works_out_spoken_sums() {
        assert_eq!(calc("What's 5 plus 5?"), Some(10.0));
        assert_eq!(calc("what is 12 times 3"), Some(36.0));
        assert_eq!(calc("15 percent of 80"), Some(12.0));
        assert_eq!(calc("100 divided by 4"), Some(25.0));
        assert_eq!(calc("what's 7 minus 10"), Some(-3.0));
        assert_eq!(calc("5+5"), Some(10.0));
        assert_eq!(calc("hva er 3 ganger 4"), Some(12.0));
        assert_eq!(calc("2 plus 3 times 4"), Some(20.0));
    }

    #[test]
    fn leaves_other_numbers_alone() {
        assert_eq!(calc("set a timer for 5 minutes"), None);
        assert_eq!(calc("dim the lights to 30 percent"), None);
        assert_eq!(calc("10 divided by 0"), None);
        assert_eq!(calc("5 plus 5 people"), None);
    }

    #[test]
    fn says_the_result() {
        let (said, value) = parse("What's 5 plus 5?").unwrap();
        assert_eq!(answer(&said, value, Lang::English), "5 plus 5 is 10.");
        let (said, value) = parse("10 divided by 4").unwrap();
        assert_eq!(answer(&said, value, Lang::English), "10 divided by 4 is 2.5.");
        assert_eq!(answer("10 delt på 4", 2.5, Lang::Norwegian), "10 delt på 4 er 2,5.");
    }
}
