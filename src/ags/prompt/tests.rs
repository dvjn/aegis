use super::*;
use demand::Theme;

#[test]
fn the_theme_uses_the_aegis_accent() {
    assert_eq!(THEME.cursor_str, "›");
    assert_eq!(THEME.cursor.fg(), Theme::color_rgb(47, 111, 237).fg());
}
