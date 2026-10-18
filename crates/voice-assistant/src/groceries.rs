//! Shopping list items under their Norwegian names. A Norwegian item comes back from the listener
//! spelled by ear ("Koffer", "Bullion", "Sinket") or as its English word ("milk"); each heard item is
//! matched to a Norwegian grocery, or to something already on the list, by its English word or by
//! sound, and written the Norwegian way. The sound key spells Norwegian and English the same way
//! ("kj" and "sh", "j" and "y"), and the consonants count most: the vowels are what a transcriber
//! gets wrong.

use crate::names::similarity;

/// A heard item is taken for a grocery from this close (`score`); tuned on `examples/grocery_eval.rs`,
/// where lower thresholds also turned "Cola" into Kål.
const MIN_SCORE: f32 = 0.7;

/// Norwegian groceries and the English words for them.
const GROCERIES: &[(&str, &[&str])] = &[
    // Dairy and eggs
    ("Melk", &["milk"]),
    ("Lettmelk", &["light milk", "low fat milk", "semi skimmed milk"]),
    ("Helmelk", &["whole milk", "full fat milk"]),
    ("Skummet melk", &["skimmed milk", "skim milk"]),
    ("Havremelk", &["oat milk"]),
    ("Fløte", &["cream"]),
    ("Kremfløte", &["whipping cream", "heavy cream"]),
    ("Matfløte", &["cooking cream"]),
    ("Rømme", &["sour cream"]),
    ("Lettrømme", &["light sour cream"]),
    ("Crème fraîche", &["creme fraiche"]),
    ("Yoghurt", &["yogurt", "yoghurt"]),
    ("Gresk yoghurt", &["greek yogurt", "greek yoghurt"]),
    ("Kesam", &[]),
    ("Kulturmelk", &["buttermilk"]),
    ("Smør", &["butter"]),
    ("Margarin", &["margarine"]),
    ("Meierismør", &[]),
    ("Ost", &["cheese"]),
    ("Brunost", &["brown cheese"]),
    ("Gulost", &["yellow cheese"]),
    ("Norvegia", &[]),
    ("Jarlsberg", &[]),
    ("Revet ost", &["grated cheese", "shredded cheese"]),
    ("Smøreost", &["cheese spread", "cream cheese"]),
    ("Kremost", &[]),
    ("Fetaost", &["feta", "feta cheese"]),
    ("Mozzarella", &[]),
    ("Parmesan", &[]),
    ("Cottage cheese", &[]),
    ("Egg", &["egg", "eggs"]),
    // Bread and breakfast
    ("Brød", &["bread"]),
    ("Grovbrød", &["wholemeal bread", "brown bread"]),
    ("Loff", &["white bread"]),
    ("Rundstykker", &["rolls", "bread rolls", "buns"]),
    ("Knekkebrød", &["crispbread", "crackers"]),
    ("Flatbrød", &["flatbread"]),
    ("Lompe", &["lomper"]),
    ("Tortilla", &["tortillas", "wraps"]),
    ("Pålegg", &["cold cuts", "sandwich toppings"]),
    ("Leverpostei", &["liver pate", "liver paste"]),
    ("Kaviar", &["caviar"]),
    ("Makrell i tomat", &["mackerel in tomato"]),
    ("Syltetøy", &["jam"]),
    ("Honning", &["honey"]),
    ("Peanøttsmør", &["peanut butter"]),
    ("Nugatti", &["chocolate spread"]),
    ("Havregryn", &["oats", "oatmeal", "porridge oats"]),
    ("Müsli", &["muesli", "granola"]),
    ("Frokostblanding", &["cereal", "breakfast cereal"]),
    // Meat and fish
    ("Kjøttdeig", &["minced meat", "mince", "ground beef"]),
    ("Kyllingfilet", &["chicken breast", "chicken fillet"]),
    ("Kylling", &["chicken"]),
    ("Kyllingvinger", &["chicken wings"]),
    ("Bacon", &[]),
    ("Skinke", &["ham"]),
    ("Kokt skinke", &["boiled ham"]),
    ("Salami", &[]),
    ("Pølser", &["sausages", "hot dogs"]),
    ("Grillpølser", &["barbecue sausages"]),
    ("Wienerpølser", &["wieners"]),
    ("Kjøttkaker", &["meatballs"]),
    ("Biff", &["steak", "beef"]),
    ("Svinekjøtt", &["pork"]),
    ("Koteletter", &["pork chops", "chops"]),
    ("Lammekjøtt", &["lamb"]),
    ("Pinnekjøtt", &[]),
    ("Ribbe", &["pork ribs", "ribs"]),
    ("Laks", &["salmon"]),
    ("Røkt laks", &["smoked salmon"]),
    ("Torsk", &["cod"]),
    ("Fiskekaker", &["fish cakes"]),
    ("Fiskepinner", &["fish fingers", "fish sticks"]),
    ("Reker", &["prawns", "shrimp", "shrimps"]),
    ("Tunfisk", &["tuna"]),
    ("Sild", &["herring"]),
    ("Makrell", &["mackerel"]),
    // Fruit and vegetables
    ("Epler", &["apples", "apple"]),
    ("Bananer", &["bananas", "banana"]),
    ("Appelsiner", &["oranges", "orange"]),
    ("Klementiner", &["clementines", "mandarins"]),
    ("Sitron", &["lemon", "lemons"]),
    ("Lime", &["limes"]),
    ("Pærer", &["pears", "pear"]),
    ("Druer", &["grapes"]),
    ("Jordbær", &["strawberries"]),
    ("Blåbær", &["blueberries"]),
    ("Bringebær", &["raspberries"]),
    ("Avokado", &["avocado", "avocados"]),
    ("Melon", &["melon"]),
    ("Vannmelon", &["watermelon"]),
    ("Ananas", &["pineapple"]),
    ("Poteter", &["potatoes", "potato"]),
    ("Søtpotet", &["sweet potato", "sweet potatoes"]),
    ("Gulrøtter", &["carrots", "carrot"]),
    ("Løk", &["onion", "onions"]),
    ("Rødløk", &["red onion", "red onions"]),
    ("Vårløk", &["spring onion", "spring onions", "scallions"]),
    ("Hvitløk", &["garlic"]),
    ("Tomater", &["tomatoes", "tomato"]),
    ("Cherrytomater", &["cherry tomatoes"]),
    ("Agurk", &["cucumber"]),
    ("Paprika", &["peppers", "bell pepper", "red pepper"]),
    ("Salat", &["lettuce", "salad"]),
    ("Isbergsalat", &["iceberg lettuce"]),
    ("Spinat", &["spinach"]),
    ("Brokkoli", &["broccoli"]),
    ("Blomkål", &["cauliflower"]),
    ("Kål", &["cabbage"]),
    ("Squash", &["courgette", "zucchini"]),
    ("Sopp", &["mushrooms", "mushroom"]),
    ("Mais", &["corn", "sweetcorn"]),
    ("Erter", &["peas"]),
    ("Bønner", &["beans"]),
    ("Ingefær", &["ginger"]),
    ("Chili", &["chilli", "chillies"]),
    ("Purre", &["leek", "leeks"]),
    ("Selleri", &["celery"]),
    ("Persille", &["parsley"]),
    ("Basilikum", &["basil"]),
    ("Koriander", &["coriander", "cilantro"]),
    ("Dill", &[]),
    // Dry goods and cooking
    ("Pasta", &["spaghetti", "penne"]),
    ("Spagetti", &[]),
    ("Ris", &["rice"]),
    ("Nudler", &["noodles"]),
    ("Mel", &["flour"]),
    ("Hvetemel", &["wheat flour", "plain flour"]),
    ("Sukker", &["sugar"]),
    ("Melis", &["icing sugar", "powdered sugar"]),
    ("Gjær", &["yeast"]),
    ("Bakepulver", &["baking powder"]),
    ("Salt", &[]),
    ("Pepper", &["black pepper"]),
    ("Olje", &["oil", "cooking oil"]),
    ("Olivenolje", &["olive oil"]),
    ("Eddik", &["vinegar"]),
    ("Ketchup", &[]),
    ("Sennep", &["mustard"]),
    ("Majones", &["mayonnaise", "mayo"]),
    ("Remulade", &[]),
    ("Soyasaus", &["soy sauce"]),
    ("Tacokrydder", &["taco seasoning", "taco spice"]),
    ("Tacosaus", &["taco sauce", "salsa"]),
    ("Tacolefser", &["taco shells", "taco wraps"]),
    ("Hermetiske tomater", &["canned tomatoes", "tinned tomatoes", "chopped tomatoes"]),
    ("Tomatpuré", &["tomato puree", "tomato paste"]),
    ("Buljong", &["stock", "bouillon", "stock cubes"]),
    ("Kakao", &["cocoa", "hot chocolate"]),
    ("Kaffe", &["coffee"]),
    ("Te", &["tea"]),
    ("Juice", &[]),
    ("Appelsinjuice", &["orange juice"]),
    ("Eplejuice", &["apple juice"]),
    ("Saft", &["cordial", "juice concentrate"]),
    ("Brus", &["soda", "soft drink", "fizzy drink"]),
    ("Farris", &["sparkling water"]),
    ("Vann", &["water"]),
    ("Øl", &["beer"]),
    ("Vin", &["wine"]),
    // Snacks and sweets
    ("Sjokolade", &["chocolate"]),
    ("Potetgull", &["crisps", "chips", "potato chips"]),
    ("Nøtter", &["nuts"]),
    ("Peanøtter", &["peanuts"]),
    ("Kjeks", &["biscuits", "cookies"]),
    ("Godteri", &["candy", "sweets"]),
    ("Smågodt", &["pick and mix"]),
    ("Is", &["ice cream"]),
    ("Pizza", &["frozen pizza"]),
    ("Grandiosa", &[]),
    ("Boller", &["sweet buns", "cinnamon buns"]),
    ("Vafler", &["waffles"]),
    // Said in English or Norwegian as they are, kept from sounding like a grocery: "Cola" is not Kål.
    ("Cola", &["coke"]),
    ("Pepsi", &[]),
    ("Pesto", &[]),
    ("Ramen", &[]),
    ("Hummus", &[]),
    ("Tofu", &[]),
    ("Cheddar", &[]),
    ("Croissanter", &["croissants"]),
    ("Gave", &["present", "gift"]),
    ("Blomster", &["flowers"]),
    ("Kort", &["card", "birthday card"]),
    ("Frimerker", &["stamps"]),
    ("Kattesand", &["cat litter"]),
    ("Kattemat", &["cat food"]),
    ("Hundemat", &["dog food"]),
    ("Lyspærer", &["light bulbs"]),
    ("Stearinlys", &["candles"]),
    ("Teip", &["tape"]),
    ("Lim", &["glue"]),
    ("Plaster", &["plasters", "band aids"]),
    ("Paracet", &["painkillers", "paracetamol"]),
    // Household
    ("Dopapir", &["toilet paper", "toilet roll"]),
    ("Tørkepapir", &["kitchen roll", "paper towels"]),
    ("Servietter", &["napkins"]),
    ("Oppvaskmiddel", &["washing up liquid", "dish soap"]),
    ("Oppvasktabletter", &["dishwasher tablets"]),
    ("Vaskemiddel", &["laundry detergent", "washing powder"]),
    ("Tøymykner", &["fabric softener"]),
    ("Søppelposer", &["bin bags", "trash bags", "garbage bags"]),
    ("Plastfolie", &["cling film", "plastic wrap"]),
    ("Aluminiumsfolie", &["tin foil", "aluminium foil", "aluminum foil"]),
    ("Bakepapir", &["baking paper", "parchment paper"]),
    ("Batterier", &["batteries"]),
    ("Tannkrem", &["toothpaste"]),
    ("Sjampo", &["shampoo"]),
    ("Såpe", &["soap"]),
    ("Bleier", &["nappies", "diapers"]),
];

/// Every Norwegian grocery name the module knows.
pub fn names() -> Vec<&'static str> {
    GROCERIES.iter().map(|(name, _)| *name).collect()
}

/// The Norwegian name for each heard item: its own spelling when it already is one, the grocery
/// its English word names, or the closest by sound among the groceries and what has been on the
/// list (`known`, e.g. brands of your own); else as heard.
pub struct Groceries {
    /// Each name with its sound key and consonants.
    names: Vec<(String, String, String)>,
}

impl Groceries {
    pub fn new<S: AsRef<str>>(known: impl IntoIterator<Item = S>) -> Self {
        let known: Vec<String> = known.into_iter().map(|k| k.as_ref().trim().to_owned()).collect();
        let names = GROCERIES
            .iter()
            .map(|(name, _)| name.to_string())
            .chain(known)
            .filter(|n| !n.is_empty())
            .map(|n| {
                let key = key(&n);
                let skeleton = skeleton(&key);
                (n, key, skeleton)
            })
            .collect();
        Self { names }
    }

    pub fn norwegian(&self, heard: &str) -> String {
        let said = heard.trim().to_lowercase();
        let named = GROCERIES.iter().find(|(name, _)| name.to_lowercase() == said);
        if let Some((name, _)) =
            named.or_else(|| GROCERIES.iter().find(|(_, english)| english.contains(&said.as_str())))
        {
            return (*name).to_owned();
        }
        // The whole item against each name, not a word of it: "pepper sauce" is not Pepper.
        let heard_key = key(heard);
        if heard_key.chars().count() < 3 {
            return heard.to_owned();
        }
        let heard_skeleton = skeleton(&heard_key);
        let score = |(_, key, skeleton): &&(String, String, String)| {
            0.5 * similarity(&heard_skeleton, skeleton) + 0.5 * similarity(&heard_key, key)
        };
        match self.names.iter().max_by(|a, b| score(a).total_cmp(&score(b))) {
            Some(best) if score(&best) >= MIN_SCORE => best.0.clone(),
            _ => heard.to_owned(),
        }
    }
}

/// Lowercase letters, Norwegian letters folded, and the spellings of one sound in either language
/// written alike.
fn key(text: &str) -> String {
    let mut s: String = text
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            'a'..='z' => Some(c),
            'æ' | 'é' | 'è' | 'ê' => Some('e'),
            'ø' | 'ö' | 'å' => Some('o'),
            'ü' => Some('u'),
            _ => None,
        })
        .collect();
    for (from, to) in [
        ("skj", "sh"),
        ("kj", "sh"),
        ("sj", "sh"),
        ("gj", "j"),
        ("hj", "j"),
        ("hv", "v"),
        ("ph", "f"),
        ("th", "t"),
        ("dh", "d"),
        ("ck", "k"),
        ("c", "k"),
        ("q", "k"),
        ("x", "ks"),
        ("z", "s"),
        ("w", "v"),
        ("y", "j"),
        ("ee", "i"),
        ("ea", "i"),
        ("oo", "u"),
        ("ou", "u"),
        ("ei", "ai"),
    ] {
        s = s.replace(from, to);
    }
    dedup(&s)
}

/// The consonants, voiced and voiceless alike ("Bender" and "bønner" are close).
fn skeleton(key: &str) -> String {
    let consonants: String = key
        .chars()
        .filter(|c| !"aeiou".contains(*c))
        .map(|c| match c {
            'p' => 'b',
            't' => 'd',
            'g' => 'k',
            'v' => 'f',
            'm' => 'n',
            c => c,
        })
        .collect();
    dedup(&consonants)
}

fn dedup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if !out.ends_with(c) {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_words_become_norwegian() {
        let g = Groceries::new(Vec::<String>::new());
        assert_eq!(g.norwegian("Milk"), "Melk");
        assert_eq!(g.norwegian("eggs"), "Egg");
        assert_eq!(g.norwegian("toilet paper"), "Dopapir");
        assert_eq!(g.norwegian("Brunost"), "Brunost");
    }

    #[test]
    fn misheard_norwegian_is_found_by_sound() {
        let g = Groceries::new(Vec::<String>::new());
        assert_eq!(g.norwegian("Brunnost"), "Brunost");
        // As Whisper wrote them, told the language is Norwegian (`examples/grocery_eval.rs`).
        assert_eq!(g.norwegian("Havere melk"), "Havremelk");
        assert_eq!(g.norwegian("Gråvbru"), "Grovbrød");
        assert_eq!(g.norwegian("Lumpa"), "Lompe");
        assert_eq!(g.norwegian("Oprika"), "Paprika");
        assert_eq!(g.norwegian("Gottere"), "Godteri");
    }

    #[test]
    fn everyday_words_stay_themselves() {
        let g = Groceries::new(Vec::<String>::new());
        assert_eq!(g.norwegian("Cola"), "Cola");
        assert_eq!(g.norwegian("Pesto"), "Pesto");
        assert_eq!(g.norwegian("Cat litter"), "Kattesand");
    }

    #[test]
    fn unknown_items_stay_as_heard() {
        let g = Groceries::new(["Pepsi Max"]);
        assert_eq!(g.norwegian("Pepsi max"), "Pepsi Max");
        assert_eq!(g.norwegian("Birthday candles"), "Birthday candles");
    }
}
