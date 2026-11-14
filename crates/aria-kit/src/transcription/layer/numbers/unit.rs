use super::grammar::Number;

/// What a number counts, said right after it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Unit {
    Dollar,
    Cent,
    Percent,
}

impl Unit {
    /// The unit `word` names after `number`. "one dollars" names none: a
    /// count of one takes the singular.
    pub(super) fn parse(word: &str, number: &Number) -> Option<Self> {
        let plural = number.integer() != Some(1);
        match word {
            "dollar" => Some(Self::Dollar),
            "dollars" if plural => Some(Self::Dollar),
            "cent" => Some(Self::Cent),
            "cents" if plural => Some(Self::Cent),
            "percent" => Some(Self::Percent),
            _ => None,
        }
    }

    /// `number` with its unit: `$5`, `$2.50`, `$0.25`, `5%`. `None` for
    /// cents that aren't a whole count.
    pub(super) fn write(self, number: &Number) -> Option<String> {
        match self {
            // One decimal place of dollars reads as tens of cents.
            Self::Dollar if number.decimal_places() == Some(1) => Some(format!("${number}0")),
            Self::Dollar => Some(format!("${number}")),
            Self::Cent => number.integer().map(in_dollars),
            Self::Percent => Some(format!("{number}%")),
        }
    }
}

/// "five dollars and twenty cents" as `$5.20`. `None` unless both are whole
/// counts, with under a hundred cents.
pub(super) fn dollars_and_cents(dollars: &Number, cents: &Number) -> Option<String> {
    let cents = cents.integer().filter(|cents| *cents < 100)?;
    let total = dollars.integer()?.checked_mul(100)?.checked_add(cents)?;
    Some(in_dollars(total))
}

/// `cents` as dollars: 125 is `$1.25`.
fn in_dollars(cents: u128) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}
