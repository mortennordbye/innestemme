//! Unit conversions, offline: "convert 10 miles to kilometres", "how many grams in a pound",
//! "what's 20 degrees Fahrenheit in Celsius", "hvor mange centimeter er en tomme". Lengths,
//! weights, volumes (cooking measures too) and temperatures. No currencies: they need live rates.

use crate::lang::Lang;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dimension {
    Length,
    Mass,
    Volume,
    Temperature,
}

#[derive(Debug, PartialEq)]
pub struct Unit {
    dimension: Dimension,
    /// Base units per unit: metres, grams, millilitres. Unused for temperatures.
    factor: f64,
    english: (&'static str, &'static str),
    norwegian: (&'static str, &'static str),
    /// How it is said, both languages, singular and plural.
    said: &'static [&'static str],
}

const fn unit(
    dimension: Dimension,
    factor: f64,
    english: (&'static str, &'static str),
    norwegian: (&'static str, &'static str),
    said: &'static [&'static str],
) -> Unit {
    Unit { dimension, factor, english, norwegian, said }
}

use Dimension::*;

/// US cups and spoons in English, metric spoons in Norwegian ("spiseskje" is 15 ml).
const UNITS: &[Unit] = &[
    unit(
        Length,
        0.001,
        ("millimetre", "millimetres"),
        ("millimeter", "millimeter"),
        &["millimetre", "millimetres", "millimeter", "millimeters", "millimeteren", "mm"],
    ),
    unit(
        Length,
        0.01,
        ("centimetre", "centimetres"),
        ("centimeter", "centimeter"),
        &["centimetre", "centimetres", "centimeter", "centimeters", "centimeteren", "cm"],
    ),
    unit(Length, 1.0, ("metre", "metres"), ("meter", "meter"), &["metre", "metres", "meter", "meters", "meteren"]),
    unit(
        Length,
        1000.0,
        ("kilometre", "kilometres"),
        ("kilometer", "kilometer"),
        &["kilometre", "kilometres", "kilometer", "kilometers", "kilometeren", "km"],
    ),
    unit(Length, 0.0254, ("inch", "inches"), ("tomme", "tommer"), &["inch", "inches", "tomme", "tommer", "tommen"]),
    unit(Length, 0.3048, ("foot", "feet"), ("fot", "fot"), &["foot", "feet", "ft", "fot", "foten"]),
    unit(Length, 0.9144, ("yard", "yards"), ("yard", "yards"), &["yard", "yards"]),
    unit(Length, 1609.344, ("mile", "miles"), ("engelsk mil", "engelske mil"), &["mile", "miles"]),
    // The Norwegian mil.
    unit(Length, 10_000.0, ("Norwegian mile", "Norwegian miles"), ("mil", "mil"), &["mil", "milen"]),
    unit(Mass, 1.0, ("gram", "grams"), ("gram", "gram"), &["gram", "grams", "grammes", "grammet", "g"]),
    unit(
        Mass,
        1000.0,
        ("kilogram", "kilograms"),
        ("kilo", "kilo"),
        &["kilogram", "kilograms", "kilo", "kilos", "kiloen", "kg"],
    ),
    unit(Mass, 453.592_37, ("pound", "pounds"), ("pund", "pund"), &["pound", "pounds", "lb", "lbs", "pund", "pundet"]),
    unit(Mass, 28.349_523, ("ounce", "ounces"), ("unse", "unser"), &["ounce", "ounces", "oz", "unse", "unser"]),
    unit(Mass, 6350.293, ("stone", "stone"), ("stone", "stone"), &["stone", "stones"]),
    unit(
        Volume,
        1.0,
        ("millilitre", "millilitres"),
        ("milliliter", "milliliter"),
        &["millilitre", "millilitres", "milliliter", "milliliters", "ml"],
    ),
    unit(
        Volume,
        100.0,
        ("decilitre", "decilitres"),
        ("desiliter", "desiliter"),
        &["decilitre", "decilitres", "deciliter", "deciliters", "desiliter", "dl"],
    ),
    unit(
        Volume,
        1000.0,
        ("litre", "litres"),
        ("liter", "liter"),
        &["litre", "litres", "liter", "liters", "literen", "l"],
    ),
    unit(Volume, 236.588, ("cup", "cups"), ("kopp", "kopper"), &["cup", "cups", "kopp", "kopper", "koppen"]),
    unit(
        Volume,
        14.787,
        ("tablespoon", "tablespoons"),
        ("amerikansk spiseskje", "amerikanske spiseskjeer"),
        &["tablespoon", "tablespoons", "tbsp"],
    ),
    unit(
        Volume,
        4.929,
        ("teaspoon", "teaspoons"),
        ("amerikansk teskje", "amerikanske teskjeer"),
        &["teaspoon", "teaspoons", "tsp"],
    ),
    unit(
        Volume,
        15.0,
        ("Norwegian tablespoon", "Norwegian tablespoons"),
        ("spiseskje", "spiseskjeer"),
        &["spiseskje", "spiseskjeer", "ss"],
    ),
    unit(
        Volume,
        5.0,
        ("Norwegian teaspoon", "Norwegian teaspoons"),
        ("teskje", "teskjeer"),
        &["teskje", "teskjeer", "ts"],
    ),
    unit(Volume, 29.574, ("fluid ounce", "fluid ounces"), ("væskeunse", "væskeunser"), &["floz"]),
    unit(Volume, 473.176, ("pint", "pints"), ("pint", "pint"), &["pint", "pints"]),
    unit(Volume, 3785.41, ("gallon", "gallons"), ("gallon", "gallon"), &["gallon", "gallons"]),
    unit(
        Temperature,
        0.0,
        ("degree Celsius", "degrees Celsius"),
        ("grad celsius", "grader celsius"),
        &["celsius", "centigrade", "c", "°c"],
    ),
    unit(
        Temperature,
        0.0,
        ("degree Fahrenheit", "degrees Fahrenheit"),
        ("grad fahrenheit", "grader fahrenheit"),
        &["fahrenheit", "f", "°f"],
    ),
    unit(Temperature, 0.0, ("kelvin", "kelvin"), ("kelvin", "kelvin"), &["kelvin", "k"]),
];

const CELSIUS: usize = 25;
const FAHRENHEIT: usize = 26;

/// Words before a unit that only say what kind it is: "20 degrees Fahrenheit", "fluid ounces".
const UNIT_FILLER: &[&str] = &["degrees", "degree", "grader", "grad", "fluid"];
/// Words between the two units of "10 miles in kilometres".
const INTO: &[&str] = &["in", "into", "to", "as", "i", "til", "på", "om"];
/// "A pound", "en tomme": one.
const ONE: &[&str] = &["a", "an", "one", "en", "ei", "et", "ett", "én"];
/// Words that may follow the request.
const TRAILING: &[&str] = &["please", "then", "takk", "da"];

#[derive(Debug, Clone, PartialEq)]
pub struct Conversion {
    pub value: f64,
    from: usize,
    to: usize,
}

// The value is always finite.
impl Eq for Conversion {}

/// "1,000" and "5,280": groups of three digits after a comma, as the transcriber writes English
/// thousands. Anything else with a comma ("1,5", "0,250") is a decimal.
fn thousands(word: &str) -> bool {
    let mut groups = word.split(',');
    let first = groups.next().unwrap_or_default().trim_start_matches('-');
    let whole = |g: &str| g.chars().all(|c| c.is_ascii_digit());
    word.contains(',')
        && (1..=3).contains(&first.len())
        && whole(first)
        && !first.starts_with('0')
        && groups.all(|g| g.split('.').next().is_some_and(|g| g.len() == 3 && whole(g)))
}

/// Words as the transcriber writes them, with "10km" and "20°F" split, "1,5" as a decimal and
/// "1,000" as a thousand.
fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split_whitespace() {
        // Kept: decimal points and commas, minus signs, fraction slashes and degree signs, which
        // `normalize` drops. A dash before a number is a minus too.
        let word: String = raw
            .chars()
            .map(|c| if matches!(c, '–' | '−') { '-' } else { c })
            .filter(|c| c.is_alphanumeric() || "'.,-/°".contains(*c))
            .flat_map(char::to_lowercase)
            .collect();
        let word = word.trim_matches(['.', ',']);
        let word = if thousands(word) { word.replace(',', "") } else { word.replace(',', ".") };
        if word.is_empty() {
            continue;
        }
        if word.contains('/') {
            out.push(word);
            continue;
        }
        let digits = word.chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-').count();
        if digits > 0 && digits < word.len() && word[..digits].parse::<f64>().is_ok() {
            out.push(word[..digits].to_owned());
            out.push(word[digits..].to_owned());
        } else {
            out.push(word);
        }
    }
    // "fl oz" and "fluid ounces" are volumes, where a bare ounce is a weight.
    let mut joined: Vec<String> = Vec::with_capacity(out.len());
    let mut i = 0;
    while i < out.len() {
        let next = out.get(i + 1).map(String::as_str);
        if matches!(out[i].as_str(), "fl" | "fluid") && matches!(next, Some("oz" | "ounce" | "ounces")) {
            joined.push("floz".into());
            i += 2;
        } else {
            joined.push(out[i].clone());
            i += 1;
        }
    }
    joined
}

fn unit_at(tokens: &[String], i: usize) -> Option<usize> {
    let word = tokens.get(i)?.as_str();
    // "Engelske mil" is the English mile.
    if ["mil", "milen"].contains(&word) && i > 0 && ["engelsk", "engelske"].contains(&tokens[i - 1].as_str()) {
        return UNITS.iter().position(|u| u.said.contains(&"miles"));
    }
    UNITS.iter().position(|u| u.said.contains(&word))
}

/// Small numbers as words, which the transcriber often writes. English only: Norwegian "to" (two)
/// is also "to" between the units.
const NUMBER_WORDS: &[&str] = &[
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
    "twenty",
];

fn number(token: &str) -> Option<f64> {
    if ONE.contains(&token) {
        return Some(1.0);
    }
    if let Some(n) = NUMBER_WORDS.iter().position(|w| *w == token) {
        return Some(n as f64);
    }
    // "1/2 cup".
    if let Some((top, bottom)) = token.split_once('/') {
        let (top, bottom) = (top.parse::<f64>().ok()?, bottom.parse::<f64>().ok()?);
        return (bottom != 0.0).then(|| top / bottom).filter(|n| n.is_finite());
    }
    token.parse::<f64>().ok().filter(|n| n.is_finite())
}

/// A fraction said as a word: "half", "quarter", "halv".
fn fraction_word(token: &str) -> Option<f64> {
    match token {
        "half" | "halves" | "halv" | "halvt" | "halve" => Some(0.5),
        "quarter" | "quarters" => Some(0.25),
        _ => None,
    }
}

/// Past "of" and "a" after a fraction: "half (of) a cup", "a quarter of a pound".
fn after_fraction(tokens: &[String], mut i: usize) -> usize {
    if tokens.get(i).is_some_and(|t| t == "of") {
        i += 1;
    }
    if tokens.get(i).is_some_and(|t| t == "a" || t == "an") {
        i += 1;
    }
    i
}

/// "And a half", "og en halv" at `i`: the fraction and the index after it.
fn and_a_fraction(tokens: &[String], i: usize) -> Option<(f64, usize)> {
    let word = |at: usize| tokens.get(at).map(String::as_str);
    if !matches!(word(i)?, "and" | "og") || !ONE.contains(&word(i + 1)?) {
        return None;
    }
    Some((fraction_word(word(i + 2)?)?, i + 3))
}

/// The amount said at `i` and the index after it: "10", "minus 40", "half a", "a quarter of a",
/// "en halv", "1/2", "one and a half", "1 1/2", "three quarters of a".
fn value_at(tokens: &[String], i: usize) -> Option<(f64, usize)> {
    let word = |at: usize| tokens.get(at).map(String::as_str);
    // The rest of an amount begun before: "1 1/2", "one and a half", "half" after "a".
    let continues = i > 0 && word(i - 1).is_some_and(|w| number(w).is_some() || w == "and" || w == "og");
    let (sign, i) = match word(i)? {
        "minus" | "negative" => (-1.0, i + 1),
        _ => (1.0, i),
    };
    let first = word(i)?;
    let fraction_next = word(i + 1).and_then(fraction_word);
    // "Half a cup"; "a quarter cup", "en halv kopp".
    if let Some(fraction) = fraction_word(first) {
        return (!continues).then(|| (sign * fraction, after_fraction(tokens, i + 1)));
    }
    if ONE.contains(&first) {
        if let Some(fraction) = fraction_next {
            return (!continues).then(|| (sign * fraction, after_fraction(tokens, i + 2)));
        }
    }
    let whole = number(first)?;
    if continues && first.contains('/') {
        return None;
    }
    // "Three quarters of a cup", "three quarter cup".
    if let Some(fraction) = fraction_next {
        return Some((sign * whole * fraction, after_fraction(tokens, i + 2)));
    }
    // "One and a half", "en og en halv".
    if let Some((fraction, after)) = and_a_fraction(tokens, i + 1) {
        return Some((sign * (whole + fraction), after));
    }
    // "1 1/2".
    if let Some(part) = word(i + 1).filter(|w| w.contains('/')).and_then(number) {
        return Some((sign * (whole + part), i + 2));
    }
    Some((sign * whole, i + 1))
}

/// The amount and unit at `i`, with any more of the same kind after it added: "5 feet 10
/// inches", "2 pounds and 3 ounces". The index after them.
fn amount_at(tokens: &[String], i: usize) -> Option<(f64, usize, usize)> {
    let (mut value, after) = value_at(tokens, i)?;
    let unit_index = skip_filler(tokens, after);
    let unit = unit_at(tokens, unit_index)?;
    let mut next = unit_index + 1;
    // Temperatures do not add up.
    if UNITS[unit].dimension == Temperature {
        return Some((value, unit, next));
    }
    // "A mile and a half".
    if let Some((fraction, after)) = and_a_fraction(tokens, next) {
        value += fraction;
        next = after;
    }
    loop {
        let start = if tokens.get(next).is_some_and(|t| t == "and" || t == "og") { next + 1 } else { next };
        let Some((more, after)) = value_at(tokens, start) else { break };
        let Some(more_unit) = unit_at(tokens, after) else { break };
        if UNITS[more_unit].dimension != UNITS[unit].dimension {
            break;
        }
        // "5 feet 2 feet" says the amount twice or wrongly: not a guess at which.
        if more_unit == unit {
            return None;
        }
        value += more * UNITS[more_unit].factor / UNITS[unit].factor;
        next = after + 1;
    }
    Some((value, unit, next))
}

/// The first non-filler token index at or after `i`.
fn skip_filler(tokens: &[String], mut i: usize) -> usize {
    while tokens
        .get(i)
        .is_some_and(|t| UNIT_FILLER.contains(&t.as_str()) || ["engelsk", "engelske"].contains(&t.as_str()))
    {
        i += 1;
    }
    i
}

/// The two units, if they measure the same thing; a weight ounce next to a volume is a fluid ounce.
fn compatible(from: usize, to: usize) -> Option<(usize, usize)> {
    let fluid = |u: usize, other: usize| {
        if UNITS[u].said[0] == "ounce" && UNITS[other].dimension == Volume {
            UNITS.iter().position(|v| v.said[0] == "floz").unwrap_or(u)
        } else {
            u
        }
    };
    let (from, to) = (fluid(from, to), fluid(to, from));
    (from != to && UNITS[from].dimension == UNITS[to].dimension).then_some((from, to))
}

/// The conversion `text` asks for, if it is one.
pub fn parse(text: &str) -> Option<Conversion> {
    let tokens = tokens(text);
    let mut end = tokens.len();
    while end > 0 && TRAILING.contains(&tokens[end - 1].as_str()) {
        end -= 1;
    }
    let tokens = &tokens[..end];
    // "How many grams (are there) in a pound", "hvor mange centimeter er (det i) en tomme".
    if let Some(at) =
        tokens.windows(2).position(|p| (p[0] == "how" && p[1] == "many") || (p[0] == "hvor" && p[1] == "mange"))
    {
        let to_at = skip_filler(tokens, at + 2);
        let to = unit_at(tokens, to_at)?;
        let between = ["are", "is", "there", "in", "to", "er", "det", "i", "på", "går", "make", "makes", "goes"];
        let mut i = to_at + 1;
        while tokens.get(i).is_some_and(|t| between.contains(&t.as_str())) {
            i += 1;
        }
        let (value, from, end) = amount_at(tokens, i)?;
        if end != tokens.len() {
            return None;
        }
        let (from, to) = compatible(from, to)?;
        return Some(Conversion { value, from, to });
    }
    // "(Convert) 10 miles to kilometres", "what's 20 degrees Fahrenheit in Celsius".
    (0..tokens.len()).find_map(|at| {
        // The second half of "5 feet 10 inches" is not the amount asked about.
        if at > 0 && unit_at(tokens, at - 1).is_some() {
            return None;
        }
        let (value, from, end) = amount_at(tokens, at)?;
        if !tokens.get(end).is_some_and(|t| INTO.contains(&t.as_str())) {
            return None;
        }
        let to_at = skip_filler(tokens, end + 1);
        let to = unit_at(tokens, to_at)?;
        if to_at + 1 != tokens.len() {
            return None;
        }
        let (from, to) = compatible(from, to)?;
        Some(Conversion { value, from, to })
    })
}

impl Conversion {
    pub fn result(&self) -> f64 {
        let (from, to) = (&UNITS[self.from], &UNITS[self.to]);
        if from.dimension != Temperature {
            return self.value * from.factor / to.factor;
        }
        let celsius = match self.from {
            CELSIUS => self.value,
            FAHRENHEIT => (self.value - 32.0) * 5.0 / 9.0,
            _ => self.value - 273.15,
        };
        match self.to {
            CELSIUS => celsius,
            FAHRENHEIT => celsius * 9.0 / 5.0 + 32.0,
            _ => celsius + 273.15,
        }
    }
}

/// Enough digits to be useful by voice: whole numbers from 1000, one decimal from 10, else two.
/// Temperatures get one.
fn spoken_number(value: f64, temperature: bool, lang: Lang) -> String {
    let decimals = if value.abs() >= 1000.0 {
        0
    } else if value.abs() >= 10.0 || temperature {
        1
    } else if value.abs() >= 0.01 || value == 0.0 {
        2
    } else {
        // Two significant digits, so a tiny result is not "0".
        ((-value.abs().log10()).floor() as usize + 2).min(12)
    };
    let mut text = format!("{value:.decimals$}");
    if text.contains('.') {
        text = text.trim_end_matches('0').trim_end_matches('.').to_owned();
    }
    if text == "-0" {
        text = "0".into();
    }
    if let Some(rest) = text.strip_prefix('-') {
        text = format!("minus {rest}");
    }
    if lang == Lang::Norwegian {
        text.replace('.', ",")
    } else {
        text
    }
}

fn named(unit: &Unit, value: &str, lang: Lang) -> String {
    let (one, many) = if lang == Lang::Norwegian { unit.norwegian } else { unit.english };
    format!("{value} {}", if ["1", "minus 1"].contains(&value) { one } else { many })
}

/// "10 miles is 16.1 kilometres."
pub fn answer(conversion: &Conversion, lang: Lang) -> String {
    let (from, to) = (&UNITS[conversion.from], &UNITS[conversion.to]);
    let temperature = from.dimension == Temperature;
    let said = named(from, &spoken_number(conversion.value, temperature, lang), lang);
    let result = named(to, &spoken_number(conversion.result(), temperature, lang), lang);
    let is = if lang == Lang::Norwegian { "er" } else { "is" };
    format!("{said} {is} {result}.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn say(text: &str) -> Option<String> {
        parse(text).map(|c| answer(&c, Lang::English))
    }

    #[test]
    fn converts_spoken_requests() {
        assert_eq!(say("convert 10 miles to kilometers").as_deref(), Some("10 miles is 16.1 kilometres."));
        assert_eq!(say("5 miles in kilometers").as_deref(), Some("5 miles is 8.05 kilometres."));
        assert_eq!(say("how many grams in a pound").as_deref(), Some("1 pound is 453.6 grams."));
        assert_eq!(say("How many grams are there in a pound?").as_deref(), Some("1 pound is 453.6 grams."));
        assert_eq!(
            say("what's 20 degrees fahrenheit in celsius").as_deref(),
            Some("20 degrees Fahrenheit is minus 6.7 degrees Celsius.")
        );
        assert_eq!(
            say("what is 100 degrees celsius in fahrenheit?").as_deref(),
            Some("100 degrees Celsius is 212 degrees Fahrenheit.")
        );
        assert_eq!(say("how many centimetres are there in an inch?").as_deref(), Some("1 inch is 2.54 centimetres."));
        assert_eq!(say("how many tablespoons in a cup").as_deref(), Some("1 cup is 16 tablespoons."));
        assert_eq!(say("convert 2 cups to millilitres").as_deref(), Some("2 cups is 473.2 millilitres."));
        assert_eq!(say("how many ounces in a pound").as_deref(), Some("1 pound is 16 ounces."));
        assert_eq!(say("how many fluid ounces in a litre").as_deref(), Some("1 litre is 33.8 fluid ounces."));
        assert_eq!(say("how many ounces in a litre").as_deref(), Some("1 litre is 33.8 fluid ounces."));
        assert_eq!(say("6 feet in metres").as_deref(), Some("6 feet is 1.83 metres."));
        assert_eq!(say("what's 70 kg in lb").as_deref(), Some("70 kilograms is 154.3 pounds."));
        assert_eq!(say("convert 1,5 litres to cups").as_deref(), Some("1.5 litres is 6.34 cups."));
        assert_eq!(say("10km in miles").as_deref(), Some("10 kilometres is 6.21 miles."));
        assert_eq!(say("how many inches is 30 centimetres").as_deref(), Some("30 centimetres is 11.8 inches."));
    }

    #[test]
    fn reads_amounts_as_they_are_said() {
        // English thousands, not "1 gram".
        assert_eq!(say("convert 1,000 grams to pounds").as_deref(), Some("1000 grams is 2.2 pounds."));
        assert_eq!(say("5,280 feet in miles").as_deref(), Some("5280 feet is 1 mile."));
        assert_eq!(
            say("minus 40 degrees celsius in fahrenheit").as_deref(),
            Some("minus 40 degrees Celsius is minus 40 degrees Fahrenheit.")
        );
        assert_eq!(say("–5 c in f").as_deref(), Some("minus 5 degrees Celsius is 23 degrees Fahrenheit."));
        assert_eq!(say("convert half a cup to millilitres").as_deref(), Some("0.5 cups is 118.3 millilitres."));
        assert_eq!(say("a quarter of a pound in grams").as_deref(), Some("0.25 pounds is 113.4 grams."));
        assert_eq!(say("convert 1/2 cup to ml").as_deref(), Some("0.5 cups is 118.3 millilitres."));
        assert_eq!(say("convert ten miles to km").as_deref(), Some("10 miles is 16.1 kilometres."));
        assert_eq!(say("1.5l to cups").as_deref(), Some("1.5 litres is 6.34 cups."));
        assert_eq!(say("1 millimetre in miles").as_deref(), Some("1 millimetre is 0.00000062 miles."));
        assert_eq!(
            say("minus 1 degree celsius in fahrenheit").as_deref(),
            Some("minus 1 degree Celsius is 30.2 degrees Fahrenheit.")
        );
    }

    #[test]
    fn adds_up_amounts_in_two_units() {
        assert_eq!(say("5 feet 10 inches in centimetres").as_deref(), Some("5.83 feet is 177.8 centimetres."));
        assert_eq!(say("2 pounds and 3 ounces in grams").as_deref(), Some("2.19 pounds is 992.2 grams."));
        assert_eq!(say("how many centimetres in 5 feet 10 inches").as_deref(), Some("5.83 feet is 177.8 centimetres."));
        // The same unit twice says nothing sure.
        assert_eq!(parse("convert 5 feet 2 feet to metres"), None);
    }

    #[test]
    fn reads_mixed_numbers() {
        let value = |text: &str| parse(text).map(|c| c.value);
        assert_eq!(value("convert one and a half cups to millilitres"), Some(1.5));
        assert_eq!(value("convert 1 1/2 cups to millilitres"), Some(1.5));
        assert_eq!(value("2 and a half pounds in kilograms"), Some(2.5));
        assert_eq!(value("three quarter cup in millilitres"), Some(0.75));
        assert_eq!(value("three quarters of a cup in millilitres"), Some(0.75));
        assert_eq!(value("a quarter cup in millilitres"), Some(0.25));
        assert_eq!(value("a mile and a half in kilometres"), Some(1.5));
        assert_eq!(value("how many millilitres in a quarter cup"), Some(0.25));
        assert_eq!(value("how many millilitres in one and a half cups"), Some(1.5));
        assert_eq!(value("how many kilometres in a mile and a half"), Some(1.5));
        assert_eq!(value("hva er en og en halv kopp i desiliter"), Some(1.5));
        // A decimal comma before three digits, when the whole number is nought.
        assert_eq!(value("convert 0,250 kg to grams"), Some(0.25));
    }

    #[test]
    fn converts_norwegian_requests() {
        let say = |text: &str| parse(text).map(|c| answer(&c, Lang::Norwegian));
        assert_eq!(say("hvor mange centimeter er en tomme").as_deref(), Some("1 tomme er 2,54 centimeter."));
        assert_eq!(say("hvor mange kilometer er en mil").as_deref(), Some("1 mil er 10 kilometer."));
        assert_eq!(say("hva er 10 engelske mil i kilometer").as_deref(), Some("10 engelske mil er 16,1 kilometer."));
        assert_eq!(say("konverter 2 kopper til desiliter").as_deref(), Some("2 kopper er 4,73 desiliter."));
        assert_eq!(say("hvor mange gram er det i et pund").as_deref(), Some("1 pund er 453,6 gram."));
    }

    #[test]
    fn leaves_other_requests_alone() {
        assert_eq!(parse("add a pound of butter and a cup of sugar to the shopping list"), None);
        assert_eq!(parse("set a timer for 10 minutes"), None);
        assert_eq!(parse("how many miles to Bergen"), None);
        assert_eq!(parse("what's 20 degrees in Oslo"), None);
        assert_eq!(parse("set the bedroom to 20 degrees"), None);
        assert_eq!(parse("convert 10 miles to grams"), None);
        assert_eq!(parse("how many people live in Oslo"), None);
    }

    #[test]
    fn temperature_indices_point_at_their_units() {
        assert_eq!(UNITS[CELSIUS].said[0], "celsius");
        assert_eq!(UNITS[FAHRENHEIT].said[0], "fahrenheit");
    }
}
