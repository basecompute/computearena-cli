//! The Base Compute palette, as used in a terminal.
//!
//! Values come from the design system's colour tokens
//! (`tokens/colors.css`, primary + secondary + digital system sets). A
//! terminal is a dark surface, so the dark scope applies: Lime is the accent
//! that carries the brand, Turquoise is the focus colour, and status uses the
//! digital-only system set. Chartreuse is an illustration colour in the
//! guidelines and never carries text, so it is absent here.
use dialoguer::{console::Style, theme::ColorfulTheme};

#[derive(Clone, Copy)]
pub(crate) struct Rgb(u8, u8, u8);

impl Rgb {
    pub(crate) const fn style(self) -> Style {
        Style::new().true_color(self.0, self.1, self.2)
    }

    /// Raw components, for interfaces that build their own colours.
    pub(crate) const fn rgb(self) -> (u8, u8, u8) {
        (self.0, self.1, self.2)
    }

    const fn stderr_style(self) -> Style {
        self.style().for_stderr()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TerminalTheme {
    /// Lime — the brand accent: headings, markers, the selected row.
    pub(crate) brand: Rgb,
    /// Inactive — secondary text that should not compete with the accent.
    pub(crate) neutral: Rgb,
    /// Negative — failures and invalid state.
    pub(crate) danger: Rgb,
    /// Positive — completed work.
    pub(crate) positive: Rgb,
    /// Turquoise — the focus colour, and the one saturated mid-tone.
    pub(crate) accent: Rgb,
    /// Deep Ocean — the other half of the signature pairing, used behind Lime.
    pub(crate) selection_background: Rgb,
}

pub(crate) const BASECOMPUTE_THEME: TerminalTheme = TerminalTheme {
    brand: Rgb(0xE8, 0xFF, 0xBD),
    neutral: Rgb(0xBD, 0xBD, 0xBD),
    danger: Rgb(0xE4, 0x3D, 0x3D),
    positive: Rgb(0x40, 0xD8, 0x61),
    accent: Rgb(0x15, 0x7F, 0xA2),
    selection_background: Rgb(0x00, 0x27, 0x3A),
};

pub(crate) fn selector_theme() -> ColorfulTheme {
    let theme = BASECOMPUTE_THEME;
    ColorfulTheme {
        prompt_style: theme.brand.stderr_style().bold(),
        prompt_prefix: theme.brand.stderr_style().bold().apply_to("›".to_string()),
        success_prefix: theme
            .positive
            .stderr_style()
            .bold()
            .apply_to("✓".to_string()),
        values_style: theme.brand.stderr_style(),
        active_item_style: theme.brand.stderr_style(),
        active_item_prefix: theme.brand.stderr_style().bold().apply_to("›".to_string()),
        fuzzy_cursor_style: theme.brand.stderr_style().on_true_color(
            theme.selection_background.0,
            theme.selection_background.1,
            theme.selection_background.2,
        ),
        fuzzy_match_highlight_style: theme.accent.stderr_style().bold(),
        ..ColorfulTheme::default()
    }
}
