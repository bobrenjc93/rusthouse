use std::hash::{Hash, Hasher};

/// A SQL identifier whose quoting controls case sensitivity and keyword status.
#[derive(Clone, Debug)]
pub(crate) struct Identifier {
    pub value: String,
    pub quoted: bool,
}

impl Identifier {
    pub(crate) fn unquoted(value: String) -> Self {
        Self {
            value,
            quoted: false,
        }
    }

    pub(crate) fn quoted(value: String) -> Self {
        Self {
            value,
            quoted: true,
        }
    }

    pub(crate) fn lookup_key(&self) -> String {
        if self.quoted {
            self.value.clone()
        } else {
            self.value.to_ascii_lowercase()
        }
    }
}

impl PartialEq for Identifier {
    fn eq(&self, other: &Self) -> bool {
        if self.quoted {
            self.value == other.lookup_key()
        } else if other.quoted {
            self.lookup_key() == other.value
        } else {
            self.value.eq_ignore_ascii_case(&other.value)
        }
    }
}

impl Eq for Identifier {}

impl Hash for Identifier {
    fn hash<H: Hasher>(&self, state: &mut H) {
        if self.quoted {
            self.value.hash(state);
        } else {
            self.value.to_ascii_lowercase().hash(state);
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ObjectName(pub Vec<Identifier>);

impl ObjectName {
    pub(crate) fn display(&self) -> String {
        self.0
            .iter()
            .map(|identifier| identifier.value.as_str())
            .collect::<Vec<_>>()
            .join(".")
    }
}
