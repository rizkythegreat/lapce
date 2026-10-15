use alacritty_terminal::term::TermMode;
use floem::keyboard::Modifiers;

/// Encode wheel input for the application, or return None for local scrollback.
pub(super) fn scroll_input(
    mode: TermMode,
    modifiers: Modifiers,
    lines: i32,
    column: usize,
    row: usize,
) -> Option<Vec<u8>> {
    if modifiers.shift() || mode.contains(TermMode::VI) {
        return None;
    }

    if mode.intersects(TermMode::MOUSE_MODE) {
        let mut button = if lines > 0 { 64 } else { 65 };
        if modifiers.alt() {
            button += 8;
        }
        if modifiers.control() {
            button += 16;
        }

        let report = if mode.contains(TermMode::SGR_MOUSE) {
            format!("\x1b[<{button};{};{}M", column + 1, row + 1).into_bytes()
        } else {
            let utf8 = mode.contains(TermMode::UTF8_MOUSE);
            let limit = if utf8 { 2015 } else { 223 };
            if column >= limit || row >= limit {
                // The legacy protocol cannot represent this position.
                return Some(Vec::new());
            }
            let mut report = vec![0x1b, b'[', b'M', 32 + button];
            for position in [column, row] {
                let value = (33 + position) as u32;
                if utf8 {
                    let mut buffer = [0; 4];
                    report.extend_from_slice(
                        char::from_u32(value)
                            .unwrap()
                            .encode_utf8(&mut buffer)
                            .as_bytes(),
                    );
                } else {
                    report.push(value as u8);
                }
            }
            report
        };
        Some(report.repeat(lines.unsigned_abs() as usize))
    } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        let arrow = if lines > 0 { b"\x1bOA" } else { b"\x1bOB" };
        Some(arrow.repeat(lines.unsigned_abs() as usize))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgr_wheel_reports_direction_position_and_modifiers() {
        let mode = TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
        assert_eq!(
            scroll_input(mode, Modifiers::empty(), 2, 119, 9).unwrap(),
            b"\x1b[<64;120;10M\x1b[<64;120;10M"
        );
        assert_eq!(
            scroll_input(mode, Modifiers::ALT | Modifiers::CONTROL, -1, 0, 0)
                .unwrap(),
            b"\x1b[<89;1;1M"
        );
    }

    #[test]
    fn legacy_mouse_preserves_non_utf8_bytes() {
        assert_eq!(
            scroll_input(TermMode::MOUSE_REPORT_CLICK, Modifiers::empty(), 1, 100, 0)
                .unwrap(),
            vec![0x1b, b'[', b'M', 96, 133, 33]
        );
        assert_eq!(
            scroll_input(
                TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE,
                Modifiers::empty(),
                -1,
                100,
                0,
            )
            .unwrap(),
            "\x1b[Ma\u{85}!".as_bytes()
        );
    }

    #[test]
    fn legacy_mouse_ignores_unrepresentable_positions() {
        assert_eq!(
            scroll_input(TermMode::MOUSE_DRAG, Modifiers::empty(), 1, 223, 0),
            Some(Vec::new())
        );
    }

    #[test]
    fn alternate_screen_uses_arrows_only_when_enabled() {
        let mode = TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL;
        assert_eq!(
            scroll_input(mode, Modifiers::empty(), -2, 0, 0).unwrap(),
            b"\x1bOB\x1bOB"
        );
        assert!(
            scroll_input(TermMode::ALT_SCREEN, Modifiers::empty(), 1, 0, 0)
                .is_none()
        );
        assert!(
            scroll_input(TermMode::ALTERNATE_SCROLL, Modifiers::empty(), 1, 0, 0)
                .is_none()
        );
    }

    #[test]
    fn mouse_reporting_takes_priority_over_alternate_scroll() {
        let mode = TermMode::ALT_SCREEN
            | TermMode::ALTERNATE_SCROLL
            | TermMode::MOUSE_MOTION
            | TermMode::SGR_MOUSE;
        assert_eq!(
            scroll_input(mode, Modifiers::empty(), 1, 0, 0).unwrap(),
            b"\x1b[<64;1;1M"
        );
    }

    #[test]
    fn shift_vi_and_normal_shell_use_local_scrollback() {
        let mode = TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
        assert!(scroll_input(mode, Modifiers::SHIFT, 1, 0, 0).is_none());
        assert!(
            scroll_input(mode | TermMode::VI, Modifiers::empty(), 1, 0, 0)
                .is_none()
        );
        assert!(
            scroll_input(TermMode::empty(), Modifiers::empty(), 1, 0, 0)
                .is_none()
        );
    }
}
