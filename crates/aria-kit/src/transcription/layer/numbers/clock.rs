/// The half of the day a time names, said right after it.
#[derive(Clone, Copy)]
pub(super) enum Meridiem {
    Am,
    Pm,
}

/// How a meridiem was written.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Form {
    /// "pm", "PM", "AM".
    Letters,
    /// "a.m.", "p.m.", whose last period is the abbreviation's own.
    Dotted,
    /// "am" in any case but capitals, also the verb: a meridiem only after
    /// minutes, "nine fifty five am", but "which one am I".
    AlsoAVerb,
}

impl Meridiem {
    /// The meridiem `heard`, without its last period, names, and its form.
    pub(super) fn parse(heard: &str) -> Option<(Self, Form)> {
        let is = |name: &str| name.eq_ignore_ascii_case(heard);
        let meridiem = if is("am") || is("a.m") {
            Self::Am
        } else if is("pm") || is("p.m") {
            Self::Pm
        } else {
            return None;
        };
        let form = if heard.contains('.') {
            Form::Dotted
        } else if is("am") && heard != "AM" {
            Form::AlsoAVerb
        } else {
            Form::Letters
        };
        Some((meridiem, form))
    }

    /// `hour` and its `minutes` with the meridiem: `9:55 PM`, `3 PM`.
    pub(super) fn write(self, hour: u128, minutes: Option<u128>) -> String {
        let half = match self {
            Self::Am => "AM",
            Self::Pm => "PM",
        };
        match minutes {
            Some(minutes) => format!("{hour}:{minutes:02} {half}"),
            None => format!("{hour} {half}"),
        }
    }
}
