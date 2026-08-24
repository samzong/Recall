use ratatui::style::Color;

pub(crate) struct Theme {
    pub(crate) text: Color,
    pub(crate) text_muted: Color,
    pub(crate) accent: Color,
    pub(crate) highlight: Color,
    pub(crate) success: Color,
    pub(crate) error: Color,
    pub(crate) info: Color,

    pub(crate) border_focus: Color,
    pub(crate) border_idle: Color,
    pub(crate) background: Color,
    pub(crate) popup_bg: Color,

    pub(crate) selected_fg: Color,
    pub(crate) selected_bg: Color,
    // Dimmer band behind the selected message in the preview/viewing panes; distinct
    // from the strong list/filter row selection above.
    pub(crate) message_highlight: Color,

    pub(crate) match_fg: Color,
    pub(crate) match_bg: Color,

    pub(crate) source: Color,
    pub(crate) user: Color,
    pub(crate) assistant: Color,
    pub(crate) summary: Color,

    pub(crate) scrollbar_thumb: Color,
    pub(crate) scrollbar_track: Color,

    // Usage dashboard: skill-audit accent and the categorical token-mix series
    pub(crate) skill: Color,
    pub(crate) token_input: Color,
    pub(crate) token_output: Color,
    pub(crate) token_cache_read: Color,
    pub(crate) token_cache_write: Color,
    pub(crate) token_reasoning: Color,
}

pub(crate) const THEME: Theme = Theme {
    text: Color::Reset,
    text_muted: Color::DarkGray,
    accent: Color::Yellow,
    highlight: Color::Cyan,
    success: Color::Green,
    error: Color::Red,
    info: Color::Blue,

    border_focus: Color::Cyan,
    border_idle: Color::DarkGray,
    background: Color::Reset,
    popup_bg: Color::Reset,

    selected_fg: Color::Black,
    selected_bg: Color::Cyan,
    message_highlight: Color::DarkGray,

    match_fg: Color::Black,
    match_bg: Color::Yellow,

    source: Color::Green,
    user: Color::Cyan,
    assistant: Color::Green,
    summary: Color::Green,

    scrollbar_thumb: Color::Cyan,
    scrollbar_track: Color::DarkGray,

    skill: Color::Magenta,
    token_input: Color::Cyan,
    token_output: Color::Green,
    token_cache_read: Color::Blue,
    token_cache_write: Color::Magenta,
    token_reasoning: Color::Yellow,
};
