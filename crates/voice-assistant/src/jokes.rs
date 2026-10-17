//! Jokes: English ones dirty, Norwegian ones the classic kind, both from built-in lists so their
//! speech can be rendered ahead of time. The list is walked from a random start, so nothing repeats
//! until every joke has been told. Numbers are written as words: a sentence with digits is not kept
//! in the speech cache.

use crate::lang::Lang;

const ENGLISH: &[&str] = &[
    "I tried to tell a premature ejaculation joke. It came out too early.",
    "Why do men give their privates a name? So a stranger isn't making ninety percent of their decisions.",
    "Sex is like maths. Add the bed, subtract the clothes, divide the legs, and pray you don't multiply.",
    "What's long, hard and full of seamen? A submarine.",
    "My partner said we need more foreplay. So I bought a second controller.",
    "What did the toaster say to the slice of bread? I want you inside me.",
    "What did the banana say to the vibrator? Why are you shaking? She's going to eat me.",
    "What's the difference between a tyre and three hundred used condoms? One's a Goodyear. The other's a great year.",
    "Why did the condom fly across the room? It got pissed off.",
    "What do you call a nun in a wheelchair? Virgin Mobile.",
    "What's the difference between your job and your partner? After five years, your job still sucks.",
    "What did the elephant say to the naked man? How do you breathe through something that small?",
    "How is a push-up bra like a bag of crisps? You open it, and it's half air.",
    "What's the best part of gardening? Getting down and dirty with your hoes.",
    "Viagra sales are up again. Analysts call it a very firm market.",
    "My partner says I'm terrible in bed. Strong opinion for someone with two minutes of evidence.",
    "I asked my date what she wanted for dessert. She said, something long and hard. So I took her to a two hour opera.",
    "Why did the snowman pull down his trousers? He heard the snowblower coming.",
    "What's the difference between a G-spot and a golf ball? A man will actually look for a golf ball.",
    "Why did the bra go to jail? It was holding up two suspects.",
    "I don't have a dirty mind. I have a sexy imagination.",
    "What's the difference between being hungry and being horny? Where you put the cucumber.",
    "Why do women fake orgasms? Because men fake foreplay.",
    "How do you make your partner scream during sex? Call them and tell them about it.",
    "What's the difference between a pickpocket and a peeping Tom? One snatches your watch. The other watches your snatch.",
    "Why is sex like a game of bridge? With a good enough hand, you don't need a partner.",
    "What's the speed limit for sex? Sixty eight. At sixty nine, you have to turn around.",
    "What did the hurricane say to the coconut tree? Hold on to your nuts. This is no ordinary blow job.",
    "What do you call a cheap circumcision? A rip-off.",
    "How is a man like a microwave? Thirty seconds, and he's done.",
    "How is sex like air? It's no big deal, unless you're not getting any.",
    "What's the difference between kinky and perverted? Kinky is using a feather. Perverted is using the whole chicken.",
    "What's six inches long, two inches wide, and drives women wild? A hundred dollar bill.",
    "Why do men struggle with eye contact? Breasts don't have eyes.",
    "What do you call a man with a small one? Justin. Just in.",
    "Why don't witches wear knickers? To get a better grip on the broom.",
    "Want to hear a joke about my penis? Never mind. It's too long.",
    "What do you call a herd of cows masturbating? Beef strokin' off.",
    "Why did the cucumber blush? It saw the salad dressing.",
    "What do a lollipop and a one night stand have in common? Once you've licked it, you can't give it back.",
    "My ex called me a condom. Always there for protection, gone the morning after.",
    "What's the difference between a hormone and a vitamin? You can't make a vitamin.",
    "I'm like a firefighter in bed. I come fast and leave the place soaking wet.",
    "Why did the sperm cross the road? Because I put on the wrong sock this morning.",
    "Why is a laptop like a lover? The moment you think it's working, it goes soft and wants a rest.",
    "What's the one thing a man and a beer bottle have in common? They're both empty from the neck up.",
];

const NORWEGIAN: &[&str] = &[
    "Hva er gult og kan ikke svømme? En gravemaskin.",
    "Hva sa nullen til åtteren? Fint belte!",
    "Hva kaller du en bjørn uten tenner? En gummibjørn.",
    "Hva sa havet til stranda? Ingenting, det bare vinket.",
    "Hvorfor var matteboka lei seg? Den hadde så mange problemer.",
    "Hvilket dyr kan hoppe høyere enn et hus? Alle sammen. Hus kan ikke hoppe.",
    "Læreren spør: Hvis jeg har fem epler i den ene hånden og seks i den andre, hva har jeg da? Eleven svarer: Veldig store hender.",
    "Hva sier den ene snømannen til den andre? Lukter det ikke gulrot her?",
    "Hvorfor går ikke fisker på skolen? De er allerede i en stim.",
    "Vet du forskjellen på en god vits og en dårlig vits? Denne var den dårlige.",
];

pub struct Jokes {
    next: usize,
}

impl Default for Jokes {
    fn default() -> Self {
        // Start somewhere different on each run.
        let next =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as usize);
        Self { next }
    }
}

impl Jokes {
    /// Every English joke, to render ahead of time.
    pub fn english() -> &'static [&'static str] {
        ENGLISH
    }

    pub fn tell(&mut self, lang: Lang) -> String {
        let list = if lang == Lang::Norwegian { NORWEGIAN } else { ENGLISH };
        self.next = self.next.wrapping_add(1);
        list[self.next % list.len()].to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_repeats_until_every_joke_is_told() {
        let mut jokes = Jokes::default();
        for (lang, list) in [(Lang::English, ENGLISH), (Lang::Norwegian, NORWEGIAN)] {
            let told: std::collections::HashSet<String> = (0..list.len()).map(|_| jokes.tell(lang)).collect();
            assert_eq!(told.len(), list.len());
        }
    }

    #[test]
    fn english_jokes_have_no_digits() {
        assert!(ENGLISH.iter().all(|j| !j.chars().any(|c| c.is_ascii_digit())));
    }
}
