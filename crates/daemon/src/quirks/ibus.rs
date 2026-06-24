#[derive(Debug, Default)]
pub(crate) struct IbusSurroundingQuirk {
    surrounding_echo_since_correction: bool,
    surrounding_saw_correction: bool,
    surrounding_corrections_without_echo: u32,
}

impl IbusSurroundingQuirk {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn mark_surrounding_frame_seen(&mut self) {
        self.surrounding_echo_since_correction = true;
    }

    pub(crate) fn note_correction_and_should_downgrade(&mut self) -> bool {
        if !self.surrounding_saw_correction {
            self.surrounding_saw_correction = true;
            self.surrounding_echo_since_correction = false;
            return false;
        }

        if self.surrounding_echo_since_correction {
            self.surrounding_corrections_without_echo = 0;
            self.surrounding_echo_since_correction = false;
            return false;
        }

        self.surrounding_corrections_without_echo += 1;
        self.surrounding_corrections_without_echo >= 1
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_correction_never_downgrades() {
        let mut quirk = IbusSurroundingQuirk::new();

        assert!(!quirk.note_correction_and_should_downgrade());
    }

    #[test]
    fn echoed_corrections_never_downgrade() {
        let mut quirk = IbusSurroundingQuirk::new();

        assert!(!quirk.note_correction_and_should_downgrade());
        quirk.mark_surrounding_frame_seen();
        assert!(!quirk.note_correction_and_should_downgrade());
        quirk.mark_surrounding_frame_seen();
        assert!(!quirk.note_correction_and_should_downgrade());
    }

    #[test]
    fn repeated_missing_echoes_downgrade_after_threshold() {
        let mut quirk = IbusSurroundingQuirk::new();

        assert!(!quirk.note_correction_and_should_downgrade());
        assert!(quirk.note_correction_and_should_downgrade());
    }

    #[test]
    fn reset_clears_downgrade_history() {
        let mut quirk = IbusSurroundingQuirk::new();

        assert!(!quirk.note_correction_and_should_downgrade());
        assert!(quirk.note_correction_and_should_downgrade());
        quirk.reset();
        assert!(!quirk.note_correction_and_should_downgrade());
    }
}
