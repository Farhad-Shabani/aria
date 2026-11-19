/// A month, said by name.
#[derive(Clone, Copy)]
pub(super) struct Month {
    name: &'static str,
    /// In the longest year.
    days: u128,
}

const MONTHS: [Month; 12] = [
    Month::new("January", 31),
    Month::new("February", 29),
    Month::new("March", 31),
    Month::new("April", 30),
    Month::new("May", 31),
    Month::new("June", 30),
    Month::new("July", 31),
    Month::new("August", 31),
    Month::new("September", 30),
    Month::new("October", 31),
    Month::new("November", 30),
    Month::new("December", 31),
];

impl Month {
    const fn new(name: &'static str, days: u128) -> Self {
        Self { name, days }
    }

    /// The month `heard` names, in any case. "March" and "May" are also
    /// everyday words, so they name a month only capitalized: "May first",
    /// but "you may first".
    pub(super) fn parse(heard: &str) -> Option<Self> {
        let month = MONTHS
            .into_iter()
            .find(|month| month.name.eq_ignore_ascii_case(heard))?;
        let also_a_word = matches!(month.name, "March" | "May");
        (!also_a_word || heard.starts_with(char::is_uppercase)).then_some(month)
    }

    /// `day` of the month, and its `year`: `March 5`, `March 5, 2015`.
    /// `None` past the month's last day.
    pub(super) fn write(self, day: u128, year: Option<u128>) -> Option<String> {
        if !(1..=self.days).contains(&day) {
            return None;
        }
        Some(match year {
            Some(year) => format!("{} {day}, {year}", self.name),
            None => format!("{} {day}", self.name),
        })
    }
}
