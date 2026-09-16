use crate::launch::EnvLookup;
use ratatui::style::Color;

#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub(crate) accent: Color,
    pub(crate) primary: Color,
    pub(crate) muted: Color,
    pub(crate) default: Color,
    pub(crate) configured: Color,
    pub(crate) error: Color,
}

impl Palette {
    pub(crate) fn current(env: &EnvLookup) -> Self {
        if env.get("NO_COLOR").is_some() {
            return Self {
                accent: Color::Reset,
                primary: Color::Reset,
                muted: Color::Reset,
                default: Color::Reset,
                configured: Color::Reset,
                error: Color::Reset,
            };
        }
        Self {
            accent: Color::Rgb(116, 199, 236),
            primary: Color::Rgb(220, 223, 228),
            muted: Color::Rgb(111, 115, 122),
            default: Color::Rgb(166, 218, 149),
            configured: Color::Rgb(137, 180, 250),
            error: Color::Rgb(237, 135, 150),
        }
    }
}
