//! Current faults have owners; resolving one fault must not hide another.
use std::collections::BTreeMap;

#[derive(Eq, PartialEq, Ord, PartialOrd)]
pub(super) enum Fault {
    Sampling,
    Identification,
    Restore(String),
    Disarm,
}

#[derive(Default)]
pub(super) struct Faults(BTreeMap<Fault, String>);

impl Faults {
    pub(super) fn set(&mut self, source: Fault, message: String) {
        self.0.insert(source, message);
    }

    pub(super) fn clear(&mut self, source: &Fault) {
        self.0.remove(source);
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn message(&self) -> Option<String> {
        (!self.0.is_empty()).then(|| self.0.values().cloned().collect::<Vec<_>>().join("；"))
    }
}
