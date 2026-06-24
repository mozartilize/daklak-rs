use viet_ime_edit_strategy::DeleteUnit;

#[derive(Debug)]
pub(crate) struct FirefoxContenteditableQuirk {
    expected_echo: Option<PendingEcho>,
    delete_unit: DeleteUnit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingEcho {
    expected: String,
    delete_echo: String,
}

impl FirefoxContenteditableQuirk {
    pub(crate) fn new() -> Self {
        Self {
            expected_echo: None,
            delete_unit: DeleteUnit::Bytes,
        }
    }

    pub(crate) fn delete_unit(&self) -> DeleteUnit {
        self.delete_unit
    }

    pub(crate) fn reset_delete_unit_after_use(&mut self) {
        if self.delete_unit == DeleteUnit::Chars {
            self.delete_unit = DeleteUnit::Bytes;
        }
    }

    pub(crate) fn record_expected_echo(
        &mut self,
        expected: String,
        delete_echo: String,
    ) {
        self.expected_echo = Some(PendingEcho {
            expected,
            delete_echo,
        });
    }

    pub(crate) fn clear(&mut self) {
        self.expected_echo = None;
        self.delete_unit = DeleteUnit::Bytes;
    }
}
