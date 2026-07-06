#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Pressed,
    Released,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_are_distinct() {
        assert_ne!(HotkeyEvent::Pressed, HotkeyEvent::Released);
    }

    #[test]
    fn is_copy_and_eq() {
        let a = HotkeyEvent::Pressed;
        let b = a;
        assert_eq!(a, b);
    }
}
