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

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}
