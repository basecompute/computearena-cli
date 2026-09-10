use dialoguer::{console::Style, theme::ColorfulTheme};

#[derive(Clone, Copy)]
pub(crate) struct Rgb(u8, u8, u8);

impl Rgb {
    pub(crate) const fn style(self) -> Style {
        Style::new().true_color(self.0, self.1, self.2)
    }

    const fn stderr_style(self) -> Style {
        self.style().for_stderr()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TerminalTheme {
    pub(crate) brand: Rgb,
    pub(crate) neutral: Rgb,
    pub(crate) danger: Rgb,
    pub(crate) accent: Rgb,
    pub(crate) selection_background: Rgb,
}

// BaseCompute terminal palette. Keeping the RGB values here makes presentation
// configurable without scattering escape sequences or color literals through
// command logic.
pub(crate) const BASECOMPUTE_THEME: TerminalTheme = TerminalTheme {
    brand: Rgb(195, 255, 77),
    neutral: Rgb(156, 163, 175),
    danger: Rgb(255, 95, 95),
    accent: Rgb(124, 192, 222),
    selection_background: Rgb(0, 18, 27),
};

pub(crate) fn selector_theme() -> ColorfulTheme {
    let theme = BASECOMPUTE_THEME;
    ColorfulTheme {
        prompt_style: theme.brand.stderr_style().bold(),
        prompt_prefix: theme.brand.stderr_style().bold().apply_to("›".to_string()),
        success_prefix: theme.brand.stderr_style().bold().apply_to("✓".to_string()),
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
